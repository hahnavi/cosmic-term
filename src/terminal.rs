use alacritty_terminal::{
    Term,
    event::{Event, EventListener, Notify, OnResize, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Indexed},
    index::{Boundary, Column, Direction, Line, Point, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{
        Config, TermDamage, TermMode,
        cell::Flags,
        color::{self, Colors},
        search::RegexSearch,
        viewport_to_point,
    },
    tty::{self, Options},
    vte::ansi::{Color, CursorShape, NamedColor, Rgb},
};
use cosmic::{
    iced::{advanced::graphics::text::font_system, mouse::ScrollDelta},
    widget::{pane_grid, segmented_button},
};
use cosmic_text::{
    Attrs, AttrsList, Buffer, BufferLine, CacheKeyFlags, Family, FeatureTag, FontFeatures,
    LineEnding, Scroll, ShapeRunCache, Shaping, Weight, Wrap,
};
use indexmap::IndexSet;
use std::{
    borrow::Cow,
    collections::HashMap,
    fs, io, mem,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, Weak,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};
use tokio::sync::mpsc;

pub use alacritty_terminal::grid::Scroll as TerminalScroll;

use crate::{
    config::{ColorSchemeKind, Config as AppConfig, ProfileId},
    menu::MenuState,
    mouse_reporter::MouseReporter,
};

/// Minimum contrast between a fixed cursor color and the cell's background.
/// Duplicated from alacritty
pub const MIN_CURSOR_CONTRAST: f64 = 1.5;

/// Maximum number of linewraps followed outside of the viewport during search highlighting.
/// Duplicated from you guessed it.
/// A regex expression can start or end outside the visible screen. Therefore, without this constant, some regular expressions would not match at the top and bottom.
pub const MAX_SEARCH_LINES: usize = 100;

/// Extra rows rendered above and below the viewport so smooth scrolling can
/// move the buffer window without rebuilding it.
const SCROLL_BUFFER_MARGIN: i32 = 16;

/// Rebuild the metadata set (which forces all buffer lines to be reshaped) once
/// it grows past this many entries, to bound per-terminal memory.
const METADATA_COMPACT_THRESHOLD: usize = 4096;

/// https://github.com/alacritty/alacritty/blob/4a7728bf7fac06a35f27f6c4f31e0d9214e5152b/alacritty/src/config/ui_config.rs#L36-L39
fn url_regex_pattern() -> &'static str {
    "(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file:|git://|ssh:|ftp://)\
         [^\u{0000}-\u{001F}\u{007F}-\u{009F}<>\"\\s{-}\\^⟨⟩`]+"
}

/// The URL regex and its four search DFAs are compiled and cached once and shared by every terminal; a per-terminal copy costs ~85 kB per tab.
fn url_regex_search() -> MutexGuard<'static, RegexSearch> {
    static URL_REGEX_SEARCH: OnceLock<Mutex<RegexSearch>> = OnceLock::new();
    URL_REGEX_SEARCH
        .get_or_init(|| Mutex::new(RegexSearch::new(url_regex_pattern()).unwrap()))
        .lock()
        .unwrap()
}

// Measures the configured font cell size and derives the default terminal size for the pre-spawned PTY.
pub fn startup_terminal_size(app_config: &AppConfig) -> Size {
    let font_stretch = app_config.typed_font_stretch();
    let font_weight = app_config.font_weight;
    let metrics = app_config.metrics(0);

    let attrs = Attrs::new()
        .family(Family::Monospace)
        .weight(Weight(font_weight))
        .stretch(font_stretch);

    let mut buffer = Buffer::new_empty(metrics);
    let cell_width = {
        let mut font_system = font_system().write().unwrap();
        font_system
            .raw()
            .db_mut()
            .set_monospace_family(&app_config.font_name);
        let font_system = font_system.raw();
        buffer.set_wrap(Wrap::None);

        // Use size of space to determine cell size
        buffer.set_text(" ", &attrs, Shaping::Advanced, None);
        let layout = buffer.line_layout(font_system, 0).unwrap();
        let cell_width = layout[0].w;

        font_system.shape_run_cache = ShapeRunCache::default();
        cell_width
    };

    Size {
        width: (80.0 * cell_width).ceil() as u32,
        height: (24.0 * metrics.line_height).ceil() as u32,
        cell_width,
        cell_height: metrics.line_height,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Size {
    pub width: u32,
    pub height: u32,
    pub cell_width: f32,
    pub cell_height: f32,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        ((self.height as f32) / self.cell_height).floor() as usize
    }

    fn columns(&self) -> usize {
        ((self.width as f32) / self.cell_width).floor() as usize
    }
}

impl From<Size> for WindowSize {
    fn from(size: Size) -> Self {
        Self {
            num_lines: size.screen_lines() as u16,
            num_cols: size.columns() as u16,
            cell_width: size.cell_width as u16,
            cell_height: size.cell_height as u16,
        }
    }
}

#[derive(Clone)]
pub struct EventProxy(
    pane_grid::Pane,
    segmented_button::Entity,
    mpsc::UnboundedSender<(pane_grid::Pane, segmented_button::Entity, Event)>,
);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        //TODO: handle error
        let _ = self.2.send((self.0, self.1, event));
    }
}

fn as_bright(mut color: Color) -> Color {
    if let Color::Named(named) = color {
        color = Color::Named(named.to_bright());
    }
    color
}

fn as_dim(mut color: Color) -> Color {
    if let Color::Named(named) = color {
        color = Color::Named(named.to_dim());
    }
    color
}

pub static WINDOW_BG_COLOR: AtomicU32 = AtomicU32::new(0xFF000000);

fn convert_color(colors: &Colors, color: Color) -> cosmic_text::Color {
    let rgb = match color {
        Color::Named(named_color) => match colors[named_color] {
            Some(rgb) => rgb,
            None => {
                if named_color == NamedColor::Background {
                    // Allow using an unset background
                    return cosmic_text::Color(WINDOW_BG_COLOR.load(Ordering::SeqCst));
                } else {
                    log::warn!("missing named color {:?}", named_color);
                    Rgb::default()
                }
            }
        },
        Color::Spec(rgb) => rgb,
        Color::Indexed(index) => {
            if let Some(rgb) = colors[index as usize] {
                rgb
            } else {
                log::warn!("missing indexed color {}", index);
                Rgb::default()
            }
        }
    };
    cosmic_text::Color::rgb(rgb.r, rgb.g, rgb.b)
}

type TabModel = segmented_button::Model<segmented_button::SingleSelect>;

pub struct TerminalPaneGrid {
    pub panes: pane_grid::State<TabModel>,
    pub panes_created: usize,
    focus: pane_grid::Pane,
}

impl TerminalPaneGrid {
    pub fn new(model: TabModel) -> Self {
        let (panes, pane) = pane_grid::State::new(model);
        let mut terminal_ids = HashMap::new();
        terminal_ids.insert(pane, cosmic::widget::Id::unique());

        Self {
            panes,
            panes_created: 1,
            focus: pane,
        }
    }
    pub fn active(&self) -> Option<&TabModel> {
        self.panes.get(self.focus)
    }
    pub fn active_mut(&mut self) -> Option<&mut TabModel> {
        self.panes.get_mut(self.focus)
    }
    pub fn set_focus(&mut self, pane: pane_grid::Pane) {
        self.focus = pane;
        self.update_terminal_focus();
    }
    pub fn focused(&self) -> pane_grid::Pane {
        self.focus
    }

    pub fn update_terminal_focus(&self) {
        for (pane, tab_model) in self.panes.panes.iter() {
            let entity = tab_model.active();
            if let Some(terminal) = tab_model.data::<Mutex<Terminal>>(entity) {
                let mut terminal = terminal.lock().unwrap();
                terminal.set_focused(self.focus == *pane);
                terminal.update();
            }
        }
    }
    pub fn unfocus_all_terminals(&self) {
        for tab_model in self.panes.panes.values() {
            let entity = tab_model.active();
            if let Some(terminal) = tab_model.data::<Mutex<Terminal>>(entity) {
                let mut terminal = terminal.lock().unwrap();
                terminal.set_focused(false);
                terminal.update();
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Metadata {
    pub bg: cosmic_text::Color,
    pub underline_color: cosmic_text::Color,
    pub flags: Flags,
}

/// A block element or box-drawing cell rendered as quads at exact cell geometry.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinGlyph {
    /// Buffer line, i.e. viewport row counted from the top.
    pub line: usize,
    /// Cell column.
    pub column: usize,
    /// The block element or box drawing character.
    pub c: char,
    /// Foreground color of the cell, after all color swaps.
    pub color: cosmic_text::Color,
}

/// Whether the character is drawn by the terminal itself, as rectangles at
/// exact cell geometry, instead of a font glyph.
///
/// Covers block elements and segment blocks, sextant mosaics, all box
/// drawing characters including the diagonals and rounded corners, and the
/// powerline symbols.
pub fn is_builtin_glyph(c: char) -> bool {
    is_block_element(c)
        || is_segment_block(c)
        || is_sextant(c)
        || is_box_drawing(c)
        || is_powerline(c)
}

pub fn is_block_element(c: char) -> bool {
    matches!(c, '\u{2580}'..='\u{259F}')
}

/// Symbols for Legacy Computing segment blocks, which complement the block
/// elements with the partial blocks anchored at the top and right edge that
/// have no Block Elements code point.
pub fn is_segment_block(c: char) -> bool {
    matches!(c, '\u{1FB82}'..='\u{1FB8B}')
}

/// Symbols for Legacy Computing sextant mosaics, which subdivide the cell
/// into a 2x3 grid.
pub fn is_sextant(c: char) -> bool {
    matches!(c, '\u{1FB00}'..='\u{1FB3B}')
}

/// Powerline symbols drawn by the terminal so that prompt segments join
/// seamlessly regardless of the configured font.
pub fn is_powerline(c: char) -> bool {
    matches!(c, '\u{E0B0}'..='\u{E0B3}')
}

pub fn is_box_drawing(c: char) -> bool {
    matches!(c, '\u{2500}'..='\u{257F}')
}

/// Rectangles covering the filled fraction of a block element or segment
/// block cell, as (x, y), (width, height) fractions of the cell with y
/// measured from the top.
///
/// Must only be called for characters where [`is_block_element`] or
/// [`is_segment_block`] is true.
pub fn block_element_rects(c: char) -> &'static [([f32; 2], [f32; 2])] {
    match c {
        '\u{2580}' => &[([0.0, 0.0], [1.0, 0.5])], // ▀ upper half
        '\u{2581}' => &[([0.0, 7.0 / 8.0], [1.0, 1.0 / 8.0])], // ▁ lower one eighth
        '\u{2582}' => &[([0.0, 3.0 / 4.0], [1.0, 1.0 / 4.0])], // ▂ lower one quarter
        '\u{2583}' => &[([0.0, 5.0 / 8.0], [1.0, 3.0 / 8.0])], // ▃ lower three eighths
        '\u{2584}' => &[([0.0, 0.5], [1.0, 0.5])], // ▄ lower half
        '\u{2585}' => &[([0.0, 3.0 / 8.0], [1.0, 5.0 / 8.0])], // ▅ lower five eighths
        '\u{2586}' => &[([0.0, 1.0 / 4.0], [1.0, 3.0 / 4.0])], // ▆ lower three quarters
        '\u{2587}' => &[([0.0, 1.0 / 8.0], [1.0, 7.0 / 8.0])], // ▇ lower seven eighths
        '\u{2588}' => &[([0.0, 0.0], [1.0, 1.0])], // █ full block
        '\u{2589}' => &[([0.0, 0.0], [7.0 / 8.0, 1.0])], // ▉ left seven eighths
        '\u{258A}' => &[([0.0, 0.0], [3.0 / 4.0, 1.0])], // ▊ left three quarters
        '\u{258B}' => &[([0.0, 0.0], [5.0 / 8.0, 1.0])], // ▋ left five eighths
        '\u{258C}' => &[([0.0, 0.0], [0.5, 1.0])], // ▌ left half
        '\u{258D}' => &[([0.0, 0.0], [3.0 / 8.0, 1.0])], // ▍ left three eighths
        '\u{258E}' => &[([0.0, 0.0], [1.0 / 4.0, 1.0])], // ▎ left one quarter
        '\u{258F}' => &[([0.0, 0.0], [1.0 / 8.0, 1.0])], // ▏ left one eighth
        '\u{2590}' => &[([0.5, 0.0], [0.5, 1.0])], // ▐ right half
        '\u{2591}' | '\u{2592}' | '\u{2593}' => &[([0.0, 0.0], [1.0, 1.0])], // ░▒▓ shades
        '\u{2594}' => &[([0.0, 0.0], [1.0, 1.0 / 8.0])], // ▴ upper one eighth
        '\u{2595}' => &[([7.0 / 8.0, 0.0], [1.0 / 8.0, 1.0])], // ▵ right one eighth
        '\u{2596}' => &[([0.0, 0.5], [0.5, 0.5])], // ▖ quadrant lower left
        '\u{2597}' => &[([0.5, 0.5], [0.5, 0.5])], // ▗ quadrant lower right
        '\u{2598}' => &[([0.0, 0.0], [0.5, 0.5])], // ▘ quadrant upper left
        '\u{2599}' => &[
            // ▙ quadrant upper left, lower left and lower right
            ([0.0, 0.0], [0.5, 1.0]),
            ([0.5, 0.5], [0.5, 0.5]),
        ],
        '\u{259A}' => &[
            // ▚ quadrant upper left and lower right
            ([0.0, 0.0], [0.5, 0.5]),
            ([0.5, 0.5], [0.5, 0.5]),
        ],
        '\u{259B}' => &[
            // ▛ quadrant upper left, upper right and lower left
            ([0.0, 0.0], [1.0, 0.5]),
            ([0.0, 0.5], [0.5, 0.5]),
        ],
        '\u{259C}' => &[
            // ▜ quadrant upper left, upper right and lower right
            ([0.0, 0.0], [1.0, 0.5]),
            ([0.5, 0.5], [0.5, 0.5]),
        ],
        '\u{259D}' => &[([0.5, 0.0], [0.5, 0.5])], // ▝ quadrant upper right
        '\u{259E}' => &[
            // ▞ quadrant upper right and lower left
            ([0.5, 0.0], [0.5, 0.5]),
            ([0.0, 0.5], [0.5, 0.5]),
        ],
        '\u{259F}' => &[
            // ▟ quadrant upper right, lower left and lower right
            ([0.5, 0.0], [0.5, 1.0]),
            ([0.0, 0.5], [0.5, 0.5]),
        ],
        // Segment blocks, which extend the partial blocks anchored at the
        // top and right edge to the fractions without a Block Elements
        // code point.
        '\u{1FB82}' => &[([0.0, 0.0], [1.0, 2.0 / 8.0])], // 🮂 upper one quarter
        '\u{1FB83}' => &[([0.0, 0.0], [1.0, 3.0 / 8.0])], // 🮃 upper three eighths
        '\u{1FB84}' => &[([0.0, 0.0], [1.0, 5.0 / 8.0])], // 🮄 upper five eighths
        '\u{1FB85}' => &[([0.0, 0.0], [1.0, 6.0 / 8.0])], // 🮅 upper three quarters
        '\u{1FB86}' => &[([0.0, 0.0], [1.0, 7.0 / 8.0])], // 🮆 upper seven eighths
        '\u{1FB87}' => &[([6.0 / 8.0, 0.0], [2.0 / 8.0, 1.0])], // 🮇 right one quarter
        '\u{1FB88}' => &[([5.0 / 8.0, 0.0], [3.0 / 8.0, 1.0])], // 🮈 right three eighths
        '\u{1FB89}' => &[([3.0 / 8.0, 0.0], [5.0 / 8.0, 1.0])], // 🮉 right five eighths
        '\u{1FB8A}' => &[([2.0 / 8.0, 0.0], [6.0 / 8.0, 1.0])], // 🮊 right three quarters
        '\u{1FB8B}' => &[([1.0 / 8.0, 0.0], [7.0 / 8.0, 1.0])], // 🮋 right seven eighths
        _ => &[],
    }
}

/// Alpha used to approximate the fill density of shade block elements.
///
/// Must only be called for characters where [`is_block_element`] is true.
pub fn block_element_alpha(c: char) -> f32 {
    match c {
        '\u{2591}' => 0.25, // ░ light shade
        '\u{2592}' => 0.5,  // ▒ medium shade
        '\u{2593}' => 0.75, // ▓ dark shade
        _ => 1.0,
    }
}

/// Rectangles covering the filled cells of a sextant mosaic, as fractions
/// of the cell.
///
/// The cell is divided into a 2x3 grid; the characters count through the
/// subsets of the six cells in binary order, with the cells weighted
/// upper-left = 1, upper-right = 2, middle-left = 4, middle-right = 8,
/// bottom-left = 16 and bottom-right = 32. The empty mosaic, the two
/// checkerboards and the full mosaic have no code point assigned and are
/// skipped by the count, as laid out by the Unicode names `BLOCK
/// SEXTANT-N`, whose digits enumerate the filled cells in reading order.
///
/// Must only be called for characters where [`is_sextant`] is true.
fn sextant_rects(c: char) -> Vec<([f32; 2], [f32; 2])> {
    let mut mask = c as u32 - 0x1FB00 + 1;
    for skipped in [21, 42, 63] {
        if mask >= skipped {
            mask += 1;
        }
    }

    const THIRD: f32 = 1.0 / 3.0;
    const CELLS: [([f32; 2], [f32; 2]); 6] = [
        ([0.0, 0.0], [0.5, THIRD]),         // upper left
        ([0.5, 0.0], [0.5, THIRD]),         // upper right
        ([0.0, THIRD], [0.5, THIRD]),       // middle left
        ([0.5, THIRD], [0.5, THIRD]),       // middle right
        ([0.0, 2.0 * THIRD], [0.5, THIRD]), // lower left
        ([0.5, 2.0 * THIRD], [0.5, THIRD]), // lower right
    ];

    CELLS
        .iter()
        .enumerate()
        .filter(|&(i, _)| (mask >> i) & 1 == 1)
        .map(|(_, &cell)| cell)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stroke {
    None,
    Light,
    Heavy,
    Double,
}

/// Arms of a box drawing line character, as (up, down, left, right) strokes
/// running from the cell center towards the named edge.
///
/// Returns `None` for dashed and rounded characters, which are handled
/// separately by [`box_drawing_rects`].
fn box_drawing_arms(c: char) -> Option<(Stroke, Stroke, Stroke, Stroke)> {
    use Stroke::{Double as D, Heavy as H, Light as L, None as N};
    Some(match c {
        '\u{2500}' => (N, N, L, L), // ─
        '\u{2501}' => (N, N, H, H), // ━
        '\u{2502}' => (L, L, N, N), // │
        '\u{2503}' => (H, H, N, N), // ┃
        '\u{250C}' => (N, L, N, L), // ┌
        '\u{250D}' => (N, L, N, H), // ┍
        '\u{250E}' => (N, H, N, L), // ┎
        '\u{250F}' => (N, H, N, H), // ┏
        '\u{2510}' => (N, L, L, N), // ┐
        '\u{2511}' => (N, L, H, N), // ┑
        '\u{2512}' => (N, H, L, N), // ┒
        '\u{2513}' => (N, H, H, N), // ┓
        '\u{2514}' => (L, N, N, L), // └
        '\u{2515}' => (L, N, N, H), // ┕
        '\u{2516}' => (H, N, N, L), // ┖
        '\u{2517}' => (H, N, N, H), // ┗
        '\u{2518}' => (L, N, L, N), // ┘
        '\u{2519}' => (L, N, H, N), // ┙
        '\u{251A}' => (H, N, L, N), // ┚
        '\u{251B}' => (H, N, H, N), // ┛
        '\u{251C}' => (L, L, N, L), // ├
        '\u{251D}' => (L, L, N, H), // ┝
        '\u{251E}' => (H, L, N, L), // ┞
        '\u{251F}' => (L, H, N, L), // ┟
        '\u{2520}' => (H, H, N, L), // ┠
        '\u{2521}' => (H, L, N, H), // ┡
        '\u{2522}' => (L, H, N, H), // ┢
        '\u{2523}' => (H, H, N, H), // ┣
        '\u{2524}' => (L, L, L, N), // ┤
        '\u{2525}' => (L, L, H, N), // ┥
        '\u{2526}' => (H, L, L, N), // ┦
        '\u{2527}' => (L, H, L, N), // ┧
        '\u{2528}' => (H, H, L, N), // ┨
        '\u{2529}' => (H, L, H, N), // ┩
        '\u{252A}' => (L, H, H, N), // ┪
        '\u{252B}' => (H, H, H, N), // ┫
        '\u{252C}' => (N, L, L, L), // ┬
        '\u{252D}' => (N, L, H, L), // ┭
        '\u{252E}' => (N, L, L, H), // ┮
        '\u{252F}' => (N, L, H, H), // ┯
        '\u{2530}' => (N, H, L, L), // ┰
        '\u{2531}' => (N, H, H, L), // ┱
        '\u{2532}' => (N, H, L, H), // ┲
        '\u{2533}' => (N, H, H, H), // ┳
        '\u{2534}' => (L, N, L, L), // ┴
        '\u{2535}' => (L, N, H, L), // ┵
        '\u{2536}' => (L, N, L, H), // ┶
        '\u{2537}' => (L, N, H, H), // ┷
        '\u{2538}' => (H, N, L, L), // ┸
        '\u{2539}' => (H, N, H, L), // ┹
        '\u{253A}' => (H, N, L, H), // ┺
        '\u{253B}' => (H, N, H, H), // ┻
        '\u{253C}' => (L, L, L, L), // ┼
        '\u{253D}' => (L, L, H, L), // ┽
        '\u{253E}' => (L, L, L, H), // ┾
        '\u{253F}' => (L, L, H, H), // ┿
        '\u{2540}' => (H, L, L, L), // ╀
        '\u{2541}' => (L, H, L, L), // ╁
        '\u{2542}' => (H, H, L, L), // ╂
        '\u{2543}' => (H, L, H, L), // ╃
        '\u{2544}' => (H, L, L, H), // ╄
        '\u{2545}' => (L, H, H, L), // ╅
        '\u{2546}' => (L, H, L, H), // ╆
        '\u{2547}' => (H, L, H, H), // ╇
        '\u{2548}' => (L, H, H, H), // ╈
        '\u{2549}' => (H, H, H, L), // ╉
        '\u{254A}' => (H, H, L, H), // ╊
        '\u{254B}' => (H, H, H, H), // ╋
        '\u{2550}' => (N, N, D, D), // ═
        '\u{2551}' => (D, D, N, N), // ║
        '\u{2552}' => (N, L, N, D), // ╒
        '\u{2553}' => (N, D, N, L), // ╓
        '\u{2554}' => (N, D, N, D), // ╔
        '\u{2555}' => (N, L, D, N), // ╕
        '\u{2556}' => (N, D, L, N), // ╖
        '\u{2557}' => (N, D, D, N), // ╗
        '\u{2558}' => (L, N, N, D), // ╘
        '\u{2559}' => (D, N, N, L), // ╙
        '\u{255A}' => (D, N, N, D), // ╚
        '\u{255B}' => (L, N, D, N), // ╛
        '\u{255C}' => (D, N, L, N), // ╜
        '\u{255D}' => (D, N, D, N), // ╝
        '\u{255E}' => (L, L, N, D), // ╞
        '\u{255F}' => (D, D, N, L), // ╟
        '\u{2560}' => (D, D, N, D), // ╠
        '\u{2561}' => (L, L, D, N), // ╡
        '\u{2562}' => (D, D, L, N), // ╢
        '\u{2563}' => (D, D, D, N), // ╣
        '\u{2564}' => (N, L, D, D), // ╤
        '\u{2565}' => (N, D, L, L), // ╥
        '\u{2566}' => (N, D, D, D), // ╦
        '\u{2567}' => (L, N, D, D), // ╧
        '\u{2568}' => (D, N, L, L), // ╨
        '\u{2569}' => (D, N, D, D), // ╩
        '\u{256A}' => (L, L, D, D), // ╪
        '\u{256B}' => (D, D, L, L), // ╫
        '\u{256C}' => (D, D, D, D), // ╬
        '\u{2574}' => (N, N, L, N), // ╴
        '\u{2575}' => (L, N, N, N), // ╵
        '\u{2576}' => (N, N, N, L), // ╶
        '\u{2577}' => (N, L, N, N), // ╷
        '\u{2578}' => (N, N, H, N), // ╸
        '\u{2579}' => (H, N, N, N), // ╹
        '\u{257A}' => (N, N, N, H), // ╺
        '\u{257B}' => (N, H, N, N), // ╻
        '\u{257C}' => (N, N, L, H), // ╼
        '\u{257D}' => (L, H, N, N), // ╽
        '\u{257E}' => (N, N, H, L), // ╾
        '\u{257F}' => (H, L, N, N), // ╿
        _ => return None,
    })
}

/// Rectangles covering the strokes of a box drawing character, as
/// (x, y), (width, height) fractions of the cell with y measured from the
/// top, plus the alpha with which each rectangle is drawn.
///
/// Must only be called for characters where [`is_box_drawing`] is true.
///
/// Strokes run between cell edge midpoints so that borders continue
/// seamlessly across cells, no matter which font is configured. The light
/// stroke thickness is one eighth of the cell width, like a hand-rasterized
/// reference implementation; heavy strokes are twice as thick. Diagonals run
/// corner to corner as antialiased rectangle coverage, so that consecutive
/// diagonal characters connect seamlessly as well.
pub fn box_drawing_rects(
    c: char,
    cell_width: f32,
    cell_height: f32,
) -> Vec<([f32; 2], [f32; 2], f32)> {
    use Stroke::{Double, None};

    let mut rects: Vec<(f32, f32, f32, f32, f32)> = Vec::new();

    let stroke = (cell_width / 8.0).round().max(1.0);
    let heavy = 2.0 * stroke;
    // Distance between the cell center and the center of each line of a
    // double stroke.
    let double_gap = stroke / 2.0 + 1.0;
    let xc = cell_width / 2.0;
    let yc = cell_height / 2.0;

    match c {
        '\u{2504}' | '\u{2505}' | '\u{2508}' | '\u{2509}' | '\u{254C}' | '\u{254D}' => {
            let (num_gaps, thickness) = match c {
                '\u{2505}' | '\u{2509}' | '\u{254D}' => (dash_num_gaps(c), heavy),
                _ => (dash_num_gaps(c), stroke),
            };
            for (x, len) in dash_segments(cell_width, num_gaps) {
                rects.push((x, yc - thickness / 2.0, len, thickness, 1.0));
            }
        }
        '\u{2506}' | '\u{2507}' | '\u{250A}' | '\u{250B}' | '\u{254E}' | '\u{254F}' => {
            let (num_gaps, thickness) = match c {
                '\u{2507}' | '\u{250B}' | '\u{254F}' => (dash_num_gaps(c), heavy),
                _ => (dash_num_gaps(c), stroke),
            };
            for (y, len) in dash_segments(cell_height, num_gaps) {
                rects.push((xc - thickness / 2.0, y, thickness, len, 1.0));
            }
        }
        // Diagonals: '╱', '╲', '╳'. Corner-to-corner antialiased lines, so
        // consecutive diagonals connect seamlessly at the shared corners.
        '\u{2571}'..='\u{2573}' => {
            // A slightly thicker band than the light stroke, so it does not
            // look anemic next to the solid axis-aligned strokes.
            let thickness = stroke + 0.5;
            if c != '\u{2571}' {
                // ╲
                aa_line_rects(
                    &mut rects,
                    (0.0, 0.0),
                    (cell_width, cell_height),
                    thickness,
                    cell_width,
                    cell_height,
                );
            }
            if c != '\u{2572}' {
                // ╱
                aa_line_rects(
                    &mut rects,
                    (0.0, cell_height),
                    (cell_width, 0.0),
                    thickness,
                    cell_width,
                    cell_height,
                );
            }
        }
        // Rounded corners: '╭', '╮', '╯', '╰'. A quarter circle rasterized
        // as a distance field with antialiased borders, so consecutive
        // corners and strokes join seamlessly.
        '\u{256D}'..='\u{2570}' => {
            rects.extend(rounded_corner_pixels(c, cell_width, cell_height, stroke));
        }
        _ => {
            let Some((up, down, left, right)) = box_drawing_arms(c) else {
                return Vec::new();
            };

            let v_double = up == Double || down == Double;
            let h_double = left == Double || right == Double;
            // At a corner, a stroke meeting a double stroke on the other axis
            // runs through the gap between its two lines; at a junction it
            // stops at the outer edge of the nearer line, like font glyphs.
            let v_corner = (up == None) != (down == None);
            let h_corner = (left == None) != (right == None);

            if up != None || down != None {
                // A lone vertical arm runs between the cell center and its
                // edge, or to (through) the lines of a double horizontal
                // stroke it meets.
                let top = if up != None {
                    0.0
                } else if h_double {
                    if h_corner {
                        yc - double_gap + stroke / 2.0
                    } else {
                        yc + double_gap + stroke / 2.0
                    }
                } else {
                    yc
                };
                let bottom = if down != None {
                    cell_height
                } else if h_double {
                    if h_corner {
                        yc + double_gap - stroke / 2.0
                    } else {
                        yc - double_gap - stroke / 2.0
                    }
                } else {
                    yc
                };
                if up == down {
                    for (pos, thickness) in stroke_bands(up, xc, stroke, double_gap) {
                        rects.push((pos, 0.0, thickness, cell_height, 1.0));
                    }
                } else {
                    for (pos, thickness) in stroke_bands(up, xc, stroke, double_gap) {
                        rects.push((pos, 0.0, thickness, bottom.min(cell_height), 1.0));
                    }
                    for (pos, thickness) in stroke_bands(down, xc, stroke, double_gap) {
                        rects.push((pos, top, thickness, cell_height - top, 1.0));
                    }
                }
            }

            if left != None || right != None {
                // A lone horizontal arm runs between the cell center and its
                // edge, or to (through) the lines of a double vertical stroke
                // it meets.
                let x0 = if left != None {
                    0.0
                } else if v_double {
                    if v_corner {
                        xc - double_gap + stroke / 2.0
                    } else {
                        xc + double_gap + stroke / 2.0
                    }
                } else {
                    xc
                };
                let x1 = if right != None {
                    cell_width
                } else if v_double {
                    if v_corner {
                        xc + double_gap - stroke / 2.0
                    } else {
                        xc - double_gap - stroke / 2.0
                    }
                } else {
                    xc
                };
                if left == right {
                    for (pos, thickness) in stroke_bands(left, yc, stroke, double_gap) {
                        rects.push((0.0, pos, cell_width, thickness, 1.0));
                    }
                } else {
                    for (pos, thickness) in stroke_bands(left, yc, stroke, double_gap) {
                        rects.push((0.0, pos, x1, thickness, 1.0));
                    }
                    for (pos, thickness) in stroke_bands(right, yc, stroke, double_gap) {
                        rects.push((x0, pos, cell_width - x0, thickness, 1.0));
                    }
                }
            }
        }
    }

    normalize_rects(rects, cell_width, cell_height)
}

/// Clamp pixel-unit rectangles to the cell and normalize them to cell
/// fractions with y measured from the top, dropping empty ones.
///
/// Clamping keeps float error on the rasterized glyphs from spilling
/// rectangles past the cell into its neighbors.
fn normalize_rects(
    rects: Vec<(f32, f32, f32, f32, f32)>,
    cell_width: f32,
    cell_height: f32,
) -> Vec<([f32; 2], [f32; 2], f32)> {
    rects
        .into_iter()
        .map(|(x, y, width, height, alpha)| {
            let x = x.clamp(0.0, cell_width);
            let y = y.clamp(0.0, cell_height);
            (
                [x / cell_width, y / cell_height],
                [
                    width.min(cell_width - x).max(0.0) / cell_width,
                    height.min(cell_height - y).max(0.0) / cell_height,
                ],
                alpha,
            )
        })
        .filter(|&(_, size, _)| size[0] > 0.0 && size[1] > 0.0)
        .collect()
}

/// Whether the powerline symbols keep their shape at the given cell
/// metrics. In cells much narrower than tall the diagonals are cut off too
/// hard, and the configured font is used instead.
pub fn powerline_fits(cell_width: f32, cell_height: f32) -> bool {
    let width = cell_width.max(1.0) as i32;
    let height = cell_height.max(1.0) as i32;
    (height + 1) / 2 - 1 - width <= 1
}

/// Rectangles covering a powerline symbol, as fractions of the cell with y
/// measured from the top.
///
/// Triangles fill the area between two diagonals stepped at one pixel per
/// row; arrows draw the two diagonals as bands of light stroke thickness,
/// connected by a vertical tip when the cell edge is reached before the
/// diagonals meet. The right-to-left variants are horizontal mirrors of the
/// left-to-right ones, like the hand-rasterized reference implementation,
/// so powerline segments join seamlessly across cells.
///
/// Must only be called for characters where [`is_powerline`] is true.
pub fn powerline_rects(
    c: char,
    cell_width: f32,
    cell_height: f32,
) -> Vec<([f32; 2], [f32; 2], f32)> {
    let width = cell_width.max(1.0) as usize;
    let height = cell_height.max(1.0) as usize;
    // Inner lines make the arrow bands as thick as the box drawing light
    // stroke.
    let extra_thickness = (width as f32 / 8.0).round().max(1.0) as i32 - 1;

    // The diagonals start one pixel inside the corners and meet one pixel
    // above the vertical center, leaving the outermost rows empty.
    let top_y = 1;
    let bottom_y = height as i32 - 2;
    let x_intersection = (height as i32 + 1) / 2 - 1;

    let triangle = matches!(c, '\u{E0B0}' | '\u{E0B2}');
    let right_to_left = matches!(c, '\u{E0B2}' | '\u{E0B3}');

    let mut rects: Vec<(f32, f32, f32, f32, f32)> = Vec::new();
    for x in 0..x_intersection {
        let top = top_y + x;
        let bottom = bottom_y - x;
        if triangle {
            // Runs grow with the distance from the corners until the
            // diagonals meet; wider runs are clamped to the cell.
            let run = (0.0, top as f32, (x + 1) as f32, 1.0, 1.0);
            rects.push(run);
            if bottom != top {
                rects.push((0.0, bottom as f32, (x + 1) as f32, 1.0, 1.0));
            }
        } else if x + 1 == width as i32 {
            // The cell ends before the diagonals meet; connect them with
            // the arrow tip.
            rects.push((x as f32, top as f32, 1.0, (bottom - top + 1) as f32, 1.0));
            break;
        } else {
            // The inner lines run ahead of the outer ones by the extra
            // thickness; past the intersection each band spans between the
            // two diagonals, merging the strokes at the tip.
            let inner_top = if x + extra_thickness < x_intersection {
                top + extra_thickness
            } else {
                bottom
            };
            let inner_bottom = if x + extra_thickness < x_intersection {
                bottom - extra_thickness
            } else {
                top
            };
            let upper_band = (x as f32, top as f32, 1.0, (inner_top - top + 1) as f32, 1.0);
            let lower_band = (
                x as f32,
                inner_bottom as f32,
                1.0,
                (bottom - inner_bottom + 1) as f32,
                1.0,
            );
            rects.push(upper_band);
            if lower_band != upper_band {
                rects.push(lower_band);
            }
        }
    }

    if right_to_left {
        for rect in &mut rects {
            rect.0 = width as f32 - rect.0 - rect.2;
        }
    }

    normalize_rects(rects, cell_width, cell_height)
}

/// A rectangle covering part of a cell, as (x, y), (width, height)
/// fractions of the cell with y measured from the top, and the alpha with
/// which it is drawn.
pub type CellRect = ([f32; 2], [f32; 2], f32);

/// Cached builtin glyph rectangles, keyed by character and cell metrics.
type RectCache = HashMap<(char, u32, u32), Arc<[CellRect]>>;

/// Rectangles covering a builtin glyph, cached per character and cell
/// metrics.
///
/// Shade block elements fade the whole glyph to approximate their fill
/// density; the antialiased box drawing glyphs and powerline symbols carry
/// their coverage per rectangle.
///
/// Must only be called for characters where [`is_builtin_glyph`] is true.
pub fn builtin_glyph_rects(c: char, cell_width: f32, cell_height: f32) -> Arc<[CellRect]> {
    // The geometry is cheap to build, but a full screen of TUI borders asks
    // for it every frame; cache it per character and cell metrics, which
    // only change with the font.
    static CACHE: OnceLock<Mutex<RectCache>> = OnceLock::new();
    let key = (c, cell_width.to_bits(), cell_height.to_bits());

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(rects) = map.get(&key) {
        return Arc::clone(rects);
    }

    let rects: Arc<[CellRect]> = if is_block_element(c) || is_segment_block(c) {
        Arc::from(
            block_element_rects(c)
                .iter()
                .map(|&(pos, size)| (pos, size, block_element_alpha(c)))
                .collect::<Vec<_>>(),
        )
    } else if is_sextant(c) {
        Arc::from(
            sextant_rects(c)
                .into_iter()
                .map(|(pos, size)| (pos, size, 1.0))
                .collect::<Vec<_>>(),
        )
    } else if is_powerline(c) {
        Arc::from(powerline_rects(c, cell_width, cell_height))
    } else {
        Arc::from(box_drawing_rects(c, cell_width, cell_height))
    };

    // Clear the cache if pathological metric churn ever grows it.
    if map.len() >= 4096 {
        map.clear();
    }
    map.insert(key, Arc::clone(&rects));
    rects
}

/// Push the antialiased coverage rectangles of a line segment from
/// (`x0`, `y0`) to (`x1`, `y1`) with the given band thickness, in pixel
/// units, clipped to the cell.
///
/// Each pixel-wide step along the major axis emits up to three rectangles —
/// the partial pixels at the band edges and the fully covered run between
/// them — with the exact coverage as their alpha, Xiaolin Wu style. The
/// rectangles of a step never overlap, and the pieces of neighboring cells at
/// a shared edge compose into the coverage of the unclipped band, so the line
/// stays seamless across cells.
fn aa_line_rects(
    rects: &mut Vec<(f32, f32, f32, f32, f32)>,
    (x0, y0): (f32, f32),
    (x1, y1): (f32, f32),
    thickness: f32,
    cell_width: f32,
    cell_height: f32,
) {
    let steep = (y1 - y0).abs() > (x1 - x0).abs();
    // Endpoints as (major, minor) coordinates, ordered along the major axis.
    let (mut a, mut a_minor, mut b, mut b_minor) = if steep {
        (y0, x0, y1, x1)
    } else {
        (x0, y0, x1, y1)
    };
    if a > b {
        std::mem::swap(&mut a, &mut b);
        std::mem::swap(&mut a_minor, &mut b_minor);
    }
    let slope = (b_minor - a_minor) / (b - a);
    let (major_max, minor_max) = if steep {
        (cell_height, cell_width)
    } else {
        (cell_width, cell_height)
    };

    for m in (a.floor() as i32)..(b.ceil() as i32) {
        let m = m as f32;
        if m + 1.0 <= 0.0 || m >= major_max {
            continue;
        }
        // Sample the line at the center of the major-axis pixel.
        let center = a_minor + (m + 0.5 - a) * slope;
        let (lo, hi) = (center - thickness / 2.0, center + thickness / 2.0);
        if hi <= 0.0 || lo >= minor_max {
            continue;
        }
        let first = lo.floor();
        let last = hi.floor();
        // Coverage pieces along the minor axis: partial, full run, partial.
        // Pieces fully outside the cell are skipped; the neighboring cell
        // draws the matching piece on its side of the shared edge.
        if first == last {
            add_aa_piece(rects, steep, m, first, 1.0, hi - lo);
        } else {
            if first + 1.0 > 0.0 {
                add_aa_piece(rects, steep, m, first, 1.0, first + 1.0 - lo);
            }
            let (s, e) = ((first + 1.0).max(0.0), last.min(minor_max));
            if e > s {
                add_aa_piece(rects, steep, m, s, e - s, 1.0);
            }
            if last < minor_max {
                add_aa_piece(rects, steep, m, last, 1.0, hi - last);
            }
        }
    }
}

/// Add one coverage rectangle of an antialiased line step: `span` along the
/// minor axis starting at `i`, one pixel along the major axis at `m`.
fn add_aa_piece(
    rects: &mut Vec<(f32, f32, f32, f32, f32)>,
    steep: bool,
    m: f32,
    i: f32,
    span: f32,
    alpha: f32,
) {
    if alpha <= 0.0 || span <= 0.0 {
        return;
    }
    if steep {
        rects.push((i, m, span, 1.0, alpha.min(1.0)));
    } else {
        rects.push((m, i, 1.0, span, alpha.min(1.0)));
    }
}

/// Coverage pixels of a rounded corner: '╭', '╮', '╯' or '╰', as
/// (x, y, width, height, alpha) rectangles in cell coordinates.
///
/// A quarter circle of radius `(min(width, height) + stroke) / 2` rasterized
/// as a distance field with linear ramps on both borders, plus the straight
/// segment connecting the arc to the cell edge it hangs from. The base arc
/// joins the top edge at the horizontal center with the left edge at the
/// vertical center (a '╯'); the other three corners are its mirrors. This
/// follows the hand-rasterized reference implementation, so corners come out
/// pixel-identical to it.
fn rounded_corner_pixels(
    c: char,
    cell_width: f32,
    cell_height: f32,
    stroke: f32,
) -> Vec<(f32, f32, f32, f32, f32)> {
    let width = cell_width.max(1.0) as usize;
    let height = cell_height.max(1.0) as usize;
    let stroke_size = stroke.max(1.0) as usize;
    let stroke_f = stroke_size as f32;

    let radius = (width.min(height) + stroke_size) as f32 / 2.0;
    // In a cell taller than wide the circle center slides along the left
    // edge; otherwise it slides along the top edge.
    let vertical = height > width;
    let (long_side, short_side) = if vertical {
        (height, width)
    } else {
        (width, height)
    };
    let distance_bias = if short_side % 2 == stroke_size % 2 {
        0.0
    } else {
        0.5
    };
    let mut offset = long_side as f32 / 2.0 - radius + stroke_f / 2.0;
    if (width % 2 != height % 2) && (long_side % 2 == stroke_size % 2) {
        offset += 1.0;
    }
    let (x_offset, y_offset) = if vertical {
        (0.0, offset)
    } else {
        (offset, 0.0)
    };

    let mut grid = vec![0.0f32; width * height];
    let radius_i = (short_side + stroke_size).div_ceil(2);
    for y in 0..radius_i {
        for x in 0..radius_i {
            let distance = (x as f32).hypot(y as f32) + distance_bias;
            let value = if distance < radius - stroke_f - 1.0 {
                // Inside the circle.
                0.0
            } else if distance < radius - stroke_f {
                // On the inner border.
                1.0 + distance - (radius - stroke_f)
            } else if distance < radius - 1.0 {
                // Inside the stroke.
                1.0
            } else if distance < radius {
                // On the outer border.
                radius - distance
            } else {
                // Outside of the circle.
                0.0
            };
            if value <= 0.0 {
                continue;
            }
            let px = x as f32 + x_offset;
            let py = y as f32 + y_offset;
            if px < 0.0 || py < 0.0 || px > width as f32 - 1.0 || py > height as f32 - 1.0 {
                continue;
            }
            let index = px as usize + py as usize * width;
            if value > grid[index] {
                grid[index] = value;
            }
        }
    }

    // Straight segment from the cell edge to where the arc begins.
    if vertical {
        let x = width as f32 / 2.0 - stroke_f * 0.5;
        let (start_x, end_x) = (x as usize, ((x + stroke_f) as usize).min(width));
        let end_y = (offset as usize).min(height);
        for y in 0..end_y {
            for x in start_x..end_x {
                grid[x + y * width] = 1.0;
            }
        }
    } else {
        let y = height as f32 / 2.0 - stroke_f * 0.5;
        let (start_y, end_y) = (y as usize, ((y + stroke_f) as usize).min(height));
        let end_x = (offset as usize).min(width);
        for y in start_y..end_y {
            for x in 0..end_x {
                grid[x + y * width] = 1.0;
            }
        }
    }

    // Mirror the base '╯' into the other three corners.
    if matches!(c, '\u{256D}' | '\u{2570}') {
        let center = width / 2;
        let extra_offset = usize::from(stroke_size % 2 != width % 2);
        for y in 1..height {
            let left = (y - 1) * width;
            let right = y * width - 1;
            if extra_offset != 0 {
                grid[right] = grid[left];
            }
            for o in 0..center {
                grid.swap(left + o, right - o - extra_offset);
            }
        }
    }
    if matches!(c, '\u{256D}' | '\u{256E}') {
        let center = height / 2;
        let extra_offset = usize::from(stroke_size % 2 != height % 2);
        if extra_offset != 0 {
            let bottom_row = (height - 1) * width;
            for index in 0..width {
                grid[bottom_row + index] = grid[index];
            }
        }
        for o in 1..=center {
            let top_row = (o - 1) * width;
            let bottom_row = (height - o - extra_offset) * width;
            for index in 0..width {
                grid.swap(top_row + index, bottom_row + index);
            }
        }
    }

    grid.into_iter()
        .enumerate()
        .filter(|&(_, value)| value > 0.0)
        .map(|(index, value)| {
            let x = (index % width) as f32;
            let y = (index / width) as f32;
            (x, y, 1.0, 1.0, value.min(1.0))
        })
        .collect()
}

fn dash_num_gaps(c: char) -> usize {
    match c {
        '\u{2504}' | '\u{2505}' | '\u{2506}' | '\u{2507}' => 2, // triple dash
        '\u{2508}' | '\u{2509}' | '\u{250A}' | '\u{250B}' => 3, // quadruple dash
        _ => 1,                                                 // double dash
    }
}

fn dash_segments(total: f32, num_gaps: usize) -> impl Iterator<Item = (f32, f32)> {
    let gap = (total / 8.0).floor().max(1.0);
    let dash = ((total - gap * num_gaps as f32) / (num_gaps as f32 + 1.0))
        .floor()
        .max(1.0);
    (0..=num_gaps).map(move |i| {
        let start = (i as f32 * (dash + gap)).min(total - dash).max(0.0);
        let len = dash.min((total - start).max(0.0));
        (start, len)
    })
}

fn stroke_bands(
    style: Stroke,
    center: f32,
    stroke: f32,
    double_gap: f32,
) -> impl Iterator<Item = (f32, f32)> {
    let bands: [(f32, f32); 2] = match style {
        Stroke::None => [(0.0, 0.0); 2],
        Stroke::Light => [(center - stroke / 2.0, stroke), (0.0, 0.0)],
        Stroke::Heavy => [(center - stroke, 2.0 * stroke), (0.0, 0.0)],
        Stroke::Double => [
            (center - double_gap - stroke / 2.0, stroke),
            (center + double_gap - stroke / 2.0, stroke),
        ],
    };
    bands.into_iter().filter(|&(_, thickness)| thickness > 0.0)
}

impl Metadata {
    fn new(bg: cosmic_text::Color, underline_color: cosmic_text::Color) -> Self {
        let flags = Flags::empty();
        Self {
            bg,
            underline_color,
            flags,
        }
    }

    fn with_underline_color(self, underline_color: cosmic_text::Color) -> Self {
        Self {
            underline_color,
            ..self
        }
    }

    fn with_flags(self, flags: Flags) -> Self {
        Self { flags, ..self }
    }
}

/// OpenType features for default attrs, disabling ligatures while preserving RTL and fallback shaping.
fn font_features_for(ligatures: bool) -> FontFeatures {
    let mut features = FontFeatures::new();
    if !ligatures {
        features
            .disable(FeatureTag::STANDARD_LIGATURES)
            .disable(FeatureTag::CONTEXTUAL_LIGATURES)
            .disable(FeatureTag::CONTEXTUAL_ALTERNATES)
            .disable(FeatureTag::DISCRETIONARY_LIGATURES);
    }
    features
}

/// Shift rendered lines to match the new buffer window.
///
/// Reuse cached lines when the window shifts within the current buffer.
fn slide_buffer_lines(
    buffer: &mut Buffer,
    old_start: i32,
    new_start: i32,
    default_attrs: &Attrs<'static>,
) {
    let line_count = buffer.lines.len();
    if line_count == 0 {
        return;
    }
    let shift = new_start - old_start;
    if shift == 0 {
        return;
    }
    if shift.unsigned_abs() as usize >= line_count {
        buffer.lines.clear();
        return;
    }
    let old_lines = mem::take(&mut buffer.lines);
    let mut new_lines = Vec::with_capacity(line_count);
    if shift > 0 {
        new_lines.extend(old_lines.into_iter().skip(shift as usize));
    } else {
        for _ in 0..shift.unsigned_abs() as usize {
            new_lines.push(BufferLine::new(
                "",
                LineEnding::default(),
                AttrsList::new(default_attrs),
                Shaping::Advanced,
            ));
        }
        new_lines.extend(old_lines);
    }
    buffer.lines = new_lines;
}

pub struct Terminal {
    pub context_menu: Option<MenuState>,
    pub metadata_set: IndexSet<Metadata>,
    pub needs_update: bool,
    pub builtin_glyphs: Vec<BuiltinGlyph>,
    pub profile_id_opt: Option<ProfileId>,
    pub tab_title_override: Option<String>,
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub regex_matches: Vec<alacritty_terminal::term::search::Match>,
    pub active_regex_match: Option<alacritty_terminal::term::search::Match>,
    pub active_hyperlink_id: Option<String>,
    bold_font_weight: Weight,
    buffer: Arc<Buffer>,
    is_focused: bool,
    colors: Colors,
    default_attrs: Attrs<'static>,
    dim_font_weight: Weight,
    buffer_start_line: i32,
    mouse_reporter: MouseReporter,
    notifier: Notifier,
    search_regex_opt: Option<RegexSearch>,
    search_value: String,
    shell_pid: Option<u32>,
    size: Size,
    font_ligatures: bool,
    use_bright_bold: bool,
    zoom_adj: i8,
}

impl Terminal {
    //TODO: error handling
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pane: pane_grid::Pane,
        entity: segmented_button::Entity,
        event_tx: mpsc::UnboundedSender<(pane_grid::Pane, segmented_button::Entity, Event)>,
        config: Config,
        options: Options,
        startup_pty: Option<(tty::Pty, Size)>,
        app_config: &AppConfig,
        colors: Colors,
        profile_id_opt: Option<ProfileId>,
        tab_title_override: Option<String>,
    ) -> Result<Self, io::Error> {
        let font_stretch = app_config.typed_font_stretch();
        let font_weight = app_config.font_weight;
        let dim_font_weight = app_config.dim_font_weight;
        let bold_font_weight = app_config.bold_font_weight;
        let use_bright_bold = app_config.use_bright_bold;
        let font_ligatures = app_config.font_ligatures;

        let metrics = app_config.metrics(0);

        let default_bg = convert_color(&colors, Color::Named(NamedColor::Background));
        let default_fg = convert_color(&colors, Color::Named(NamedColor::Foreground));

        let mut metadata_set = IndexSet::new();
        let default_metada = Metadata::new(default_bg, default_fg);
        let (default_metada_idx, _) = metadata_set.insert_full(default_metada);

        //TODO: set color to default fg
        let default_attrs = Attrs::new()
            .family(Family::Monospace)
            .weight(Weight(font_weight))
            .stretch(font_stretch)
            .font_features(font_features_for(font_ligatures))
            .color(default_fg)
            .metadata(default_metada_idx);

        let mut buffer = Buffer::new_empty(metrics);

        let (cell_width, cell_height) = {
            let mut font_system = font_system().write().unwrap();
            let font_system = font_system.raw();
            buffer.set_wrap(Wrap::None);

            // Use size of space to determine cell size
            buffer.set_text(" ", &default_attrs, Shaping::Advanced, None);
            let layout = buffer.line_layout(font_system, 0).unwrap();
            let w = layout[0].w;
            buffer.set_monospace_width(Some(w));
            (w, metrics.line_height)
        };

        let size = Size {
            width: (80.0 * cell_width).ceil() as u32,
            height: (24.0 * cell_height).ceil() as u32,
            cell_width,
            cell_height,
        };
        let event_proxy = EventProxy(pane, entity, event_tx);
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &size,
            event_proxy.clone(),
        )));

        let window_id = 0;
        let pty = match startup_pty {
            Some((mut pty, startup_size)) => {
                if startup_size.cell_width != size.cell_width
                    || startup_size.cell_height != size.cell_height
                {
                    pty.on_resize(size.into());
                }
                pty
            }
            None => tty::new(&options, size.into(), window_id)?,
        };
        #[cfg(not(windows))]
        let shell_pid = Some(pty.child().id());
        #[cfg(windows)]
        let shell_pid = pty.child_watcher().pid().map(|pid| pid.get());

        let pty_event_loop =
            EventLoop::new(term.clone(), event_proxy, pty, options.drain_on_exit, false)?;
        let notifier = Notifier(pty_event_loop.channel());
        let _pty_join_handle = pty_event_loop.spawn();

        Ok(Self {
            active_regex_match: None,
            active_hyperlink_id: None,
            regex_matches: Vec::new(),
            builtin_glyphs: Vec::new(),
            bold_font_weight: Weight(bold_font_weight),
            buffer: Arc::new(buffer),
            colors,
            context_menu: None,
            default_attrs,
            dim_font_weight: Weight(dim_font_weight),
            buffer_start_line: 0,
            metadata_set,
            mouse_reporter: Default::default(),
            needs_update: true,
            notifier,
            profile_id_opt,
            search_regex_opt: None,
            search_value: String::new(),
            shell_pid,
            size,
            tab_title_override,
            term,
            font_ligatures,
            use_bright_bold,
            zoom_adj: Default::default(),
            is_focused: true,
        })
    }

    pub fn buffer_weak(&self) -> Weak<Buffer> {
        Arc::downgrade(&self.buffer)
    }

    /// Get the internal [`Buffer`]
    pub fn with_buffer<F: FnOnce(&Buffer) -> T, T>(&self, f: F) -> T {
        f(&self.buffer)
    }

    /// Get the internal [`Buffer`], mutably
    pub fn with_buffer_mut<F: FnOnce(&mut Buffer) -> T, T>(&mut self, f: F) -> T {
        f(Arc::make_mut(&mut self.buffer))
    }

    pub fn colors(&self) -> &Colors {
        &self.colors
    }

    pub fn effective_color(&self, index: usize) -> Rgb {
        if index == NamedColor::Background as usize {
            self.colors[index].unwrap_or_else(|| {
                // Allow using an unset background
                let [r, g, b, _] =
                    cosmic_text::Color(WINDOW_BG_COLOR.load(Ordering::SeqCst)).as_rgba();
                Rgb { r, g, b }
            })
        } else {
            self.colors[index].unwrap_or_default()
        }
    }

    pub fn default_attrs(&self) -> &Attrs<'static> {
        &self.default_attrs
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn zoom_adj(&self) -> i8 {
        self.zoom_adj
    }

    pub fn set_zoom_adj(&mut self, value: i8) {
        self.zoom_adj = value;
    }

    pub fn set_term_options(&mut self, options: Config) {
        self.term.lock().set_options(options);
        self.needs_update = true;
    }

    fn set_focused(&mut self, is_focused: bool) {
        let focus_changed = self.is_focused != is_focused;
        self.is_focused = is_focused;

        if focus_changed {
            let report_focus = self.term.lock().mode().contains(TermMode::FOCUS_IN_OUT);
            if report_focus {
                const FOCUS_IN: &[u8] = b"\x1b[I";
                const FOCUS_OUT: &[u8] = b"\x1b[O";

                let input = if is_focused { FOCUS_IN } else { FOCUS_OUT };
                self.input_no_scroll(input);
            }
        }
    }

    pub fn redraw(&self) -> bool {
        self.buffer.redraw()
    }

    pub fn set_redraw(&mut self, redraw: bool) {
        self.with_buffer_mut(|buffer| buffer.set_redraw(redraw));
    }

    pub fn input_no_scroll<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        self.notifier.notify(input);
    }

    pub fn working_directory(&self) -> Option<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            let shell_pid = self.shell_pid?;
            fs::read_link(format!("/proc/{shell_pid}/cwd")).ok()
        }

        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    pub fn input_scroll<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        self.input_no_scroll(input);
        self.scroll(TerminalScroll::Bottom);
    }

    pub fn paste(&self, value: String) {
        // This code is ported from alacritty
        let bracketed_paste = {
            let term = self.term.lock();
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };
        if bracketed_paste {
            self.input_no_scroll(&b"\x1b[200~"[..]);
            self.input_no_scroll(value.replace('\x1b', "").into_bytes());
            self.input_scroll(&b"\x1b[201~"[..]);
        } else {
            // In non-bracketed (ie: normal) mode, terminal applications cannot distinguish
            // pasted data from keystrokes.
            // In theory, we should construct the keystrokes needed to produce the data we are
            // pasting... since that's neither practical nor sensible (and probably an impossible
            // task to solve in a general way), we'll just replace line breaks (windows and unix
            // style) with a single carriage return (\r, which is what the Enter key produces).
            self.input_scroll(value.replace("\r\n", "\r").replace('\n', "\r").into_bytes());
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width != self.size.width || height != self.size.height {
            let instant = Instant::now();

            // Clamp dimensions to ensure at least 1 row and 1 column,
            // preventing index-out-of-bounds panics in alacritty_terminal.
            let min_width = self.size.cell_width.ceil() as u32;
            let min_height = self.size.cell_height.ceil() as u32;
            self.size.width = width.max(min_width);
            self.size.height = height.max(min_height);

            self.notifier.on_resize(self.size.into());
            self.term.lock().resize(self.size);

            self.with_buffer_mut(|buffer| {
                buffer.set_size(Some(width as f32), Some(height as f32));
            });

            self.needs_update = true;

            log::debug!("resize {:?}", instant.elapsed());
        }
    }

    pub fn scroll(&self, scroll: TerminalScroll) {
        self.term.lock().scroll_display(scroll);
    }

    pub fn display_offset(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    pub fn history_size(&self) -> usize {
        self.term.lock().grid().history_size()
    }

    pub fn buffer_start_line(&self) -> i32 {
        self.buffer_start_line
    }

    pub fn scroll_window_covers(&self, offset: f32) -> bool {
        let screen_lines = self.term.lock().grid().screen_lines() as f32;
        let top = -offset - self.buffer_start_line as f32;
        let len = self.buffer.lines.len() as f32;
        top >= 0.0 && top + screen_lines <= len
    }

    pub fn scroll_smooth(&mut self, delta: i32) {
        if delta != 0 {
            self.term
                .lock()
                .grid_mut()
                .scroll_display(TerminalScroll::Delta(delta));
        }
    }

    pub fn rebuild_scroll_window(&mut self) {
        if self.needs_update || !self.refill_scroll_window() {
            self.update();
        }
        self.needs_update = false;
    }

    fn refill_scroll_window(&mut self) -> bool {
        if self.metadata_set.len() > METADATA_COMPACT_THRESHOLD {
            return false;
        }
        let display_offset = self.display_offset() as i32;
        let (topmost, screen_lines, bottommost) = {
            let term = self.term.lock();
            let grid = term.grid();
            (
                grid.topmost_line().0,
                grid.screen_lines() as i32,
                grid.bottommost_line().0,
            )
        };
        let first_line = (-display_offset - SCROLL_BUFFER_MARGIN).max(topmost);
        let shift = first_line - self.buffer_start_line;
        let line_count = self.buffer.lines.len();
        if shift == 0 || shift.unsigned_abs() as usize >= line_count {
            return false;
        }
        let end_line = (screen_lines - 1 - display_offset + SCROLL_BUFFER_MARGIN).min(bottommost);
        if end_line < first_line {
            return false;
        }
        self.update_inner(true);
        true
    }

    pub fn set_scroll_position(&mut self, offset: f32) {
        let len = self.buffer.lines.len();
        if len == 0 {
            return;
        }
        let screen_lines = self.term.lock().grid().screen_lines() as f32;
        let mut top = -offset - self.buffer_start_line as f32;
        top = top.clamp(0.0, (len as f32 - screen_lines).max(0.0));
        let line = top.floor() as usize;
        let vertical = (top - line as f32) * self.size.cell_height;
        let scroll = Scroll {
            line,
            vertical,
            horizontal: 0.0,
        };
        let needs_shaping = self.with_buffer(|buffer| {
            buffer.scroll() != scroll
                || buffer
                    .lines
                    .get(scroll.line)
                    .is_none_or(|line| line.shape_opt().is_none() || line.needs_reshaping())
        });
        if !needs_shaping {
            return;
        }
        self.with_buffer_mut(|buffer| buffer.set_scroll(scroll));
        let mut font_system = font_system().write().unwrap();
        self.with_buffer_mut(|buffer| buffer.shape_until_scroll(font_system.raw(), false));
    }

    pub fn scroll_to(&self, ratio: f32) {
        let mut term = self.term.lock();
        let grid = term.grid();
        let total = grid.history_size() + grid.screen_lines();
        let old_display_offset = grid.display_offset() as i32;
        let new_display_offset =
            ((total as f32) * (1.0 - ratio)) as i32 - grid.screen_lines() as i32;
        term.scroll_display(TerminalScroll::Delta(
            new_display_offset - old_display_offset,
        ));
    }

    pub fn scrollbar(&self) -> Option<(f32, f32)> {
        let term = self.term.lock();
        let grid = term.grid();
        if grid.history_size() > 0 {
            let total = grid.history_size() + grid.screen_lines();
            let start = total - grid.display_offset() - grid.screen_lines();
            let end = total - grid.display_offset();
            Some((
                (start as f32) / (total as f32),
                (end as f32) / (total as f32),
            ))
        } else {
            None
        }
    }

    pub fn search(&mut self, value: &str, forwards: bool) {
        //TODO: set max lines, run in thread?
        {
            let mut term = self.term.lock();

            if self.search_value != value {
                match RegexSearch::new(value) {
                    Ok(search_regex) => {
                        self.search_regex_opt = Some(search_regex);
                        self.search_value = value.to_string();
                        term.selection = None;
                    }
                    Err(err) => {
                        log::warn!("failed to parse regex {:?}: {}", value, err);
                        return;
                    }
                }
            }

            let Some(search_regex) = &mut self.search_regex_opt else {
                return;
            };

            // Determine search origin
            let grid = term.grid();
            let search_origin = match term
                .selection
                .as_ref()
                .and_then(|selection| selection.to_range(&term))
            {
                Some(range) => {
                    //TODO: determine correct search_origin, along with side below
                    if forwards {
                        range.end.add(grid, Boundary::Grid, 1)
                    } else {
                        range.start.sub(grid, Boundary::Grid, 1)
                    }
                }
                None => {
                    if forwards {
                        Point::new(Line(-(grid.history_size() as i32)), Column(0))
                    } else {
                        Point::new(
                            Line(grid.screen_lines() as i32 - 1),
                            Column(grid.columns() - 1),
                        )
                    }
                }
            };

            // Find next search match
            if let Some(search_match) = term.search_next(
                search_regex,
                search_origin,
                if forwards {
                    Direction::Right
                } else {
                    Direction::Left
                },
                //TODO: determine correct side, along with search_origin above
                if forwards { Side::Left } else { Side::Right },
                None,
            ) {
                // Scroll to match
                if forwards {
                    term.scroll_to_point(*search_match.end());
                } else {
                    term.scroll_to_point(*search_match.start());
                }

                // Set selection to match
                let mut selection =
                    Selection::new(SelectionType::Simple, *search_match.start(), Side::Left);
                selection.update(*search_match.end(), Side::Right);
                term.selection = Some(selection);
            }
        }

        self.update();
    }

    pub fn select_all(&mut self) {
        {
            let mut term = self.term.lock();
            let grid = term.grid();
            let start = Point::new(Line(-(grid.history_size() as i32)), Column(0));
            let mut end_line = grid.bottommost_line();
            while end_line.0 > 0 {
                if !grid[end_line].is_clear() {
                    break;
                }
                end_line.0 -= 1;
            }
            let end = Point::new(end_line, Column(grid.columns() - 1));
            let mut selection = Selection::new(SelectionType::Lines, start, Side::Left);
            selection.update(end, Side::Right);
            term.selection = Some(selection);
        }
        self.update();
    }

    pub fn set_config(
        &mut self,
        config: &AppConfig,
        color_scheme_kind: ColorSchemeKind,
        themes: &HashMap<(String, ColorSchemeKind), Colors>,
    ) {
        let mut update_cell_size = false;
        let mut update = false;
        let zoom_adj = self.zoom_adj;
        if self.default_attrs.stretch != config.typed_font_stretch() {
            self.default_attrs = self
                .default_attrs
                .clone()
                .stretch(config.typed_font_stretch());
            update_cell_size = true;
        }

        if self.default_attrs.weight.0 != config.font_weight {
            self.default_attrs = self
                .default_attrs
                .clone()
                .weight(Weight(config.font_weight));
            update_cell_size = true;
        }

        if self.dim_font_weight.0 != config.dim_font_weight {
            self.dim_font_weight = Weight(config.dim_font_weight);
            update_cell_size = true;
        }

        if self.bold_font_weight.0 != config.bold_font_weight {
            self.bold_font_weight = Weight(config.bold_font_weight);
            update_cell_size = true;
        }

        if self.font_ligatures != config.font_ligatures {
            self.font_ligatures = config.font_ligatures;
            self.default_attrs = self
                .default_attrs
                .clone()
                .font_features(font_features_for(config.font_ligatures));
            update = true;
        }

        if self.use_bright_bold != config.use_bright_bold {
            self.use_bright_bold = config.use_bright_bold;
            update_cell_size = true;
        }

        let metrics = config.metrics(zoom_adj);
        if metrics != self.buffer.metrics() {
            self.with_buffer_mut(|buffer| buffer.set_metrics(metrics));
            update_cell_size = true;
        }

        if let Some(colors) =
            themes.get(&config.syntax_theme(color_scheme_kind, self.profile_id_opt))
        {
            let mut changed = false;
            for i in 0..color::COUNT {
                if self.colors[i] != colors[i] {
                    self.colors[i] = colors[i];
                    changed = true;
                }
            }
            if changed {
                update = true;
            }
        }

        // NOTE: this is done on every set_config because the changed boolean above does not capture
        // WINDOW_BG changes
        let default_colors_updated = self.update_default_colors(config);

        if update_cell_size {
            self.update_cell_size();
        } else if update || default_colors_updated {
            self.update();
        }
    }

    pub fn update_default_colors(&mut self, config: &AppConfig) -> bool {
        let default_bg = convert_color(&self.colors, Color::Named(NamedColor::Background));
        let default_fg = convert_color(&self.colors, Color::Named(NamedColor::Foreground));

        let new_default_metadata = Metadata::new(default_bg, default_fg);
        let curr_metada_idx = self.default_attrs().metadata;

        let updated = new_default_metadata != self.metadata_set[curr_metada_idx];

        if updated {
            self.metadata_set.clear();
            let (default_metadata_idx, _) = self.metadata_set.insert_full(new_default_metadata);

            self.default_attrs = Attrs::new()
                .family(Family::Monospace)
                .weight(Weight(config.font_weight))
                .stretch(config.typed_font_stretch())
                .font_features(font_features_for(self.font_ligatures))
                .color(default_fg)
                .metadata(default_metadata_idx);
        }

        updated
    }

    pub fn update_cell_size(&mut self) {
        let default_attrs = self.default_attrs.clone();
        let (cell_width, cell_height) = {
            let mut font_system = font_system().write().unwrap();
            self.with_buffer_mut(|buffer| {
                buffer.set_wrap(Wrap::None);

                // Use size of space to determine cell size
                buffer.set_text(" ", &default_attrs, Shaping::Advanced, None);
                let layout = buffer.line_layout(font_system.raw(), 0).unwrap();
                let w = layout[0].w;
                buffer.set_monospace_width(Some(w));
                (w, buffer.metrics().line_height)
            })
        };

        let old_size = self.size;
        self.size = Size {
            width: 0,
            height: 0,
            cell_width,
            cell_height,
        };
        self.resize(old_size.width, old_size.height);

        self.update();
    }

    pub fn update(&mut self) -> bool {
        self.update_inner(false)
    }

    fn update_inner(&mut self, scroll_only: bool) -> bool {
        // LEFT‑TO‑RIGHT ISOLATE character.
        // This will be added to the beginning of lines to force the shaper to treat detected RTL
        // lines as LTR. RTL text would still be rendered correctly. But this fixes the wrong
        // behavior of it being aligned to the right.
        const LRI: char = '\u{2066}';

        let instant = Instant::now();

        // Keep metadata stable during scrolling; compact it when it grows too large.
        let compact = self.metadata_set.len() > METADATA_COMPACT_THRESHOLD;
        let scroll_only = scroll_only && !compact;
        if compact {
            self.metadata_set.truncate(1);
        }
        if !scroll_only {
            self.builtin_glyphs.clear();
        }

        // Powerline symbols are only drawn by the terminal when they keep
        // their shape at the cell metrics; otherwise the font's glyphs are
        // used, like the hand-rasterized reference implementation.
        let size = self.size();
        let builtin = |c: char| {
            is_builtin_glyph(c)
                && (!is_powerline(c) || powerline_fits(size.cell_width, size.cell_height))
        };

        //TODO: is redraw needed after all events?
        //TODO: use LineDamageBounds
        {
            let buffer = Arc::make_mut(&mut self.buffer);

            if compact {
                buffer.lines.clear();
            }

            let mut line_i = 0;
            let mut render_line;
            let mut render_range;
            let mut last_point = None;
            let mut text = String::from(LRI);
            let mut last_visible = text.len();
            let mut attrs_list = AttrsList::new(&self.default_attrs);
            {
                let mut term = self.term.lock();
                //TODO: use damage?
                match term.damage() {
                    TermDamage::Full => {}
                    TermDamage::Partial(_damage_lines) => {}
                }
                term.reset_damage();

                self.regex_matches.clear();
                {
                    let mut url_regex_search = url_regex_search();
                    let mut regex_matches: Vec<_> =
                        visible_regex_match_iter(&term, &mut url_regex_search).collect();
                    self.regex_matches
                        .extend(regex_matches.drain(..).flat_map(|rm| -> Vec<_> {
                            HintPostProcessor::new(&term, &mut url_regex_search, rm).collect()
                        }));
                }

                let grid = term.grid();
                let display_offset = grid.display_offset() as i32;
                let first_line =
                    (-display_offset - SCROLL_BUFFER_MARGIN).max(grid.topmost_line().0);
                let end_line = (grid.screen_lines() as i32 - 1 - display_offset
                    + SCROLL_BUFFER_MARGIN)
                    .min(grid.bottommost_line().0);
                let old_start = self.buffer_start_line;
                let old_len = buffer.lines.len();
                if !compact {
                    slide_buffer_lines(buffer, old_start, first_line, &self.default_attrs);
                }
                self.buffer_start_line = first_line;
                let window_len = (end_line - first_line + 1).max(0) as usize;
                render_range = (0, window_len);
                if scroll_only {
                    if buffer.lines.is_empty() {
                        self.builtin_glyphs.clear();
                    } else {
                        let shift = first_line - old_start;
                        render_range = if shift < 0 {
                            (0, ((-shift) as usize).min(window_len))
                        } else {
                            (
                                ((old_len as i32 - shift).max(0) as usize).min(window_len),
                                window_len,
                            )
                        };
                        self.builtin_glyphs.retain_mut(|glyph| {
                            let new_line = glyph.line as i64 - shift as i64;
                            if new_line < 0 || new_line >= window_len as i64 {
                                return false;
                            }
                            glyph.line = new_line as usize;
                            true
                        });
                    }
                }
                render_line = !scroll_only || (render_range.0 == 0);
                let columns = grid.columns();
                let mut line = first_line;
                let mut column = 0;
                let display_iter = std::iter::from_fn(|| loop {
                    if line > end_line {
                        return None;
                    }
                    if column >= columns {
                        line += 1;
                        column = 0;
                        continue;
                    }
                    let point = Point::new(Line(line), Column(column));
                    column += 1;
                    return Some(Indexed {
                        point,
                        cell: &grid[point],
                    });
                });
                for indexed in display_iter {
                    if indexed.point.line != last_point.unwrap_or(indexed.point).line {
                        while line_i >= buffer.lines.len() {
                            buffer.lines.push(BufferLine::new(
                                "",
                                LineEnding::default(),
                                AttrsList::new(&self.default_attrs),
                                Shaping::Advanced,
                            ));
                            buffer.set_redraw(true);
                        }

                        if render_line {
                            text.truncate(last_visible);
                            if buffer.lines[line_i].set_text(
                                &text,
                                LineEnding::default(),
                                attrs_list.clone(),
                            ) {
                                buffer.set_redraw(true);
                            }
                        }
                        line_i += 1;
                        render_line = !scroll_only
                            || (line_i >= render_range.0 && line_i < render_range.1);

                        text.clear();
                        text.push(LRI);
                        last_visible = text.len();
                        attrs_list.clear_spans();
                    }
                    if !render_line {
                        last_point = Some(indexed.point);
                        continue;
                    }
                    //TODO: use indexed.point.column?

                    //TODO: skip leading spacer?
                    if indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        // Skip wide spacers (cells after wide characters)
                        last_point = Some(indexed.point);
                        continue;
                    }

                    let start = text.len();
                    // Tab skip/stop is handled by alacritty_terminal
                    // Builtin glyphs are drawn as quads by terminal_box, so
                    // replace them with spaces to keep the buffer layout
                    // unchanged
                    text.push(match indexed.cell.c {
                        '\t' => ' ',
                        c if builtin(c) => ' ',
                        c => c,
                    });
                    if let Some(zerowidth) = indexed.cell.zerowidth() {
                        for &c in zerowidth {
                            text.push(c);
                        }
                    }
                    let end = text.len();

                    let mut attrs = self.default_attrs.clone();

                    let cell_fg = if indexed.cell.flags.contains(Flags::DIM) {
                        as_dim(indexed.cell.fg)
                    } else if self.use_bright_bold && indexed.cell.flags.contains(Flags::BOLD) {
                        as_bright(indexed.cell.fg)
                    } else {
                        indexed.cell.fg
                    };

                    let (mut fg, mut bg) = if indexed.cell.flags.contains(Flags::INVERSE) {
                        (
                            convert_color(&self.colors, indexed.cell.bg),
                            convert_color(&self.colors, cell_fg),
                        )
                    } else {
                        (
                            convert_color(&self.colors, cell_fg),
                            convert_color(&self.colors, indexed.cell.bg),
                        )
                    };

                    if indexed.cell.flags.contains(Flags::HIDDEN) {
                        fg = bg;
                    }

                    // Change color if cursor
                    if indexed.point == grid.cursor.point
                        && term.renderable_content().cursor.shape == CursorShape::Block
                        && self.is_focused
                    {
                        //Use specific cursor color if requested
                        if term.colors()[NamedColor::Cursor].is_some() {
                            fg = bg;
                            bg = convert_color(term.colors(), Color::Named(NamedColor::Cursor));
                        } else if self.colors[NamedColor::Cursor].is_some() {
                            //Use specific theme cursor color if exists
                            fg = bg;
                            bg = convert_color(&self.colors, Color::Named(NamedColor::Cursor));
                        } else {
                            mem::swap(&mut fg, &mut bg);
                        }
                        let fg_rgb = Rgb {
                            r: fg.r(),
                            g: fg.g(),
                            b: fg.b(),
                        };
                        let bg_rgb = Rgb {
                            r: bg.r(),
                            g: bg.g(),
                            b: bg.b(),
                        };
                        let contrast = fg_rgb.contrast(bg_rgb);
                        if contrast < MIN_CURSOR_CONTRAST {
                            fg = convert_color(&self.colors, Color::Named(NamedColor::Background));
                            bg = convert_color(&self.colors, Color::Named(NamedColor::Foreground));
                        }
                    }

                    // Change color if selected
                    if let Some(selection) = &term.selection
                        && let Some(range) = selection.to_range(&term)
                        && range.contains(indexed.point)
                    {
                        //TODO: better handling of selection
                        mem::swap(&mut fg, &mut bg);
                    }

                    // Convert foreground to linear
                    attrs = attrs.color(fg);

                    let underline_color = indexed
                        .cell
                        .underline_color()
                        .map(|c| convert_color(&self.colors, c))
                        .unwrap_or(fg);

                    let mut flags = indexed.cell.flags;

                    if let Some(active_match) = &self.active_regex_match
                        && active_match.contains(&indexed.point)
                    {
                        flags |= Flags::UNDERLINE;
                    }
                    if let Some(active_id) = &self.active_hyperlink_id {
                        let mut matches_active = indexed
                            .cell
                            .hyperlink()
                            .is_some_and(|link| link.id() == active_id);
                        if !matches_active
                            && indexed.cell.flags.intersects(
                                Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
                            )
                            && indexed.point.column.0 > 0
                        {
                            matches_active = grid[Point::new(
                                indexed.point.line,
                                Column(indexed.point.column.0 - 1),
                            )]
                            .hyperlink()
                            .is_some_and(|link| link.id() == active_id);
                        }
                        if matches_active {
                            flags |= Flags::UNDERLINE;
                        }
                    }

                    let metadata = Metadata::new(bg, fg)
                        .with_flags(flags)
                        .with_underline_color(underline_color);
                    let (meta_idx, _) = self.metadata_set.insert_full(metadata);
                    attrs = attrs.metadata(meta_idx);

                    if builtin(indexed.cell.c) {
                        self.builtin_glyphs.push(BuiltinGlyph {
                            line: line_i,
                            column: indexed.point.column.0,
                            c: indexed.cell.c,
                            color: fg,
                        });
                    }

                    //TODO: more flags
                    if indexed.cell.flags.contains(Flags::BOLD) {
                        attrs = attrs.weight(self.bold_font_weight);
                    } else if indexed.cell.flags.contains(Flags::DIM) {
                        // if DIM and !BOLD
                        attrs = attrs.weight(self.dim_font_weight);
                    }
                    if indexed.cell.flags.contains(Flags::ITALIC) {
                        //TODO: automatically use fake italic
                        attrs = attrs.cache_key_flags(CacheKeyFlags::FAKE_ITALIC);
                    }
                    let is_default_attrs = attrs == attrs_list.defaults();
                    if !is_default_attrs {
                        attrs_list.add_span(start..end, &attrs);
                    }

                    let is_blank_cell = (indexed.cell.c == ' ' || indexed.cell.c == '\t')
                        && indexed.cell.zerowidth().is_none();
                    if !is_default_attrs || !is_blank_cell {
                        last_visible = end;
                    }

                    last_point = Some(indexed.point);
                }
            }

            //TODO: do not repeat!
            while line_i >= buffer.lines.len() {
                buffer.lines.push(BufferLine::new(
                    "",
                    LineEnding::default(),
                    AttrsList::new(&self.default_attrs),
                    Shaping::Advanced,
                ));
                buffer.set_redraw(true);
            }

            if render_line {
                text.truncate(last_visible);
                if buffer.lines[line_i].set_text(text, LineEnding::default(), attrs_list) {
                    buffer.set_redraw(true);
                }
            }
            line_i += 1;

            if buffer.lines.len() != line_i {
                buffer.lines.truncate(line_i);
                buffer.set_redraw(true);
            }

            // Shape and trim shape run cache
            {
                let mut font_system = font_system().write().unwrap();
                buffer.shape_until_scroll(font_system.raw(), false);
                font_system.raw().shape_run_cache.trim(1);
            }
        }

        log::debug!("buffer update {:?}", instant.elapsed());

        self.buffer.redraw()
    }

    pub fn viewport_to_point(&self, point: Point<usize>) -> Point {
        let term = self.term.lock();
        viewport_to_point(term.grid().display_offset(), point)
    }

    pub fn report_mouse(
        &mut self,
        event: cosmic::iced::Event,
        modifiers: &cosmic::iced::keyboard::Modifiers,
        x: u32,
        y: u32,
    ) {
        let term_lock = self.term.lock();
        let mode = term_lock.mode();

        #[allow(clippy::collapsible_else_if)]
        if mode.contains(TermMode::SGR_MOUSE) {
            if let Some(code) = self.mouse_reporter.sgr_mouse_code(event, modifiers, x, y) {
                self.input_no_scroll(code)
            }
        } else {
            if let Some(code) = self.mouse_reporter.normal_mouse_code(
                event,
                modifiers,
                mode.contains(TermMode::UTF8_MOUSE),
                x,
                y,
            ) {
                self.input_no_scroll(code)
            }
        }
    }

    pub fn scroll_mouse(
        &mut self,
        delta: ScrollDelta,
        modifiers: &cosmic::iced::keyboard::Modifiers,
        x: u32,
        y: u32,
    ) {
        let is_sgr = self.term.lock().mode().contains(TermMode::SGR_MOUSE);

        if is_sgr {
            let codes = self.mouse_reporter.sgr_mouse_wheel_scroll(
                self.size().cell_width,
                self.size().cell_height,
                delta,
                modifiers,
                x,
                y,
            );

            for code in codes {
                self.notifier.notify(code);
            }
        } else {
            self.scroll_as_arrows(delta);
        }
    }

    pub fn scroll_as_arrows(&mut self, delta: ScrollDelta) {
        let cell_width = self.size().cell_width;
        let cell_height = self.size().cell_height;
        let (_, lines_y) = self
            .mouse_reporter
            .accumulate_scroll(delta, cell_width, cell_height);
        let is_app_cursor = self.term.lock().mode().contains(TermMode::APP_CURSOR);
        let (up, down) = if is_app_cursor {
            (&b"\x1BOA"[..], &b"\x1BOB"[..])
        } else {
            (&b"\x1B[A"[..], &b"\x1B[B"[..])
        };
        const SCROLL_SPEED: u32 = 3;
        for _ in 0..(lines_y.unsigned_abs() * SCROLL_SPEED) {
            if lines_y > 0 {
                self.input_no_scroll(up)
            } else if lines_y < 0 {
                self.input_no_scroll(down)
            }
        }
    }
}
/// Iterate over all visible regex matches.
/// This includes the screen +- 100 lines (MAX_SEARCH_LINES).
/// display/hint.rs
pub fn visible_regex_match_iter<'a, T>(
    term: &'a Term<T>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = alacritty_terminal::term::search::Match> + 'a {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start = term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - MAX_SEARCH_LINES);
    end.line = end.line.min(viewport_end + MAX_SEARCH_LINES);

    alacritty_terminal::term::search::RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}
/** Copy of <https://github.com/alacritty/alacritty/blob/4a7728bf7fac06a35f27f6c4f31e0d9214e5152b/alacritty/src/display/hint.rs#L433C1-L572C1> */
/// Iterator over all post-processed matches inside an existing hint match.
struct HintPostProcessor<'a, T> {
    /// Regex search DFAs.
    regex: &'a mut RegexSearch,

    /// Terminal reference.
    term: &'a Term<T>,

    /// Next hint match in the iterator.
    next_match: Option<alacritty_terminal::term::search::Match>,

    /// Start point for the next search.
    start: Point,

    /// End point for the hint match iterator.
    end: Point,
}

impl<'a, T> HintPostProcessor<'a, T> {
    /// Create a new iterator for an unprocessed match.
    fn new(
        term: &'a Term<T>,
        regex: &'a mut RegexSearch,
        regex_match: alacritty_terminal::term::search::Match,
    ) -> Self {
        let mut post_processor = Self {
            next_match: None,
            start: *regex_match.start(),
            end: *regex_match.end(),
            term,
            regex,
        };

        // Post-process the first hint match.
        post_processor.next_processed_match(regex_match);

        post_processor
    }

    /// Apply some hint post processing heuristics.
    ///
    /// This will check the end of the hint and make it shorter if certain characters are determined
    /// to be unlikely to be intentionally part of the hint.
    ///
    /// This is most useful for identifying URLs appropriately.
    fn hint_post_processing(
        &self,
        regex_match: &alacritty_terminal::term::search::Match,
    ) -> Option<alacritty_terminal::term::search::Match> {
        let mut iter = self.term.grid().iter_from(*regex_match.start());

        let mut c = iter.cell().c;

        // Truncate uneven number of brackets.
        let end = *regex_match.end();
        let mut open_parents = 0;
        let mut open_brackets = 0;
        loop {
            match c {
                '(' => open_parents += 1,
                '[' => open_brackets += 1,
                ')' => {
                    if open_parents == 0 {
                        alacritty_terminal::grid::BidirectionalIterator::prev(&mut iter);
                        break;
                    } else {
                        open_parents -= 1;
                    }
                }
                ']' => {
                    if open_brackets == 0 {
                        alacritty_terminal::grid::BidirectionalIterator::prev(&mut iter);
                        break;
                    } else {
                        open_brackets -= 1;
                    }
                }
                _ => (),
            }

            if iter.point() == end {
                break;
            }

            match iter.next() {
                Some(indexed) => c = indexed.cell.c,
                None => break,
            }
        }

        // Truncate trailing characters which are likely to be delimiters.
        let start = *regex_match.start();
        while iter.point() != start {
            if !matches!(c, '.' | ',' | ':' | ';' | '?' | '!' | '(' | '[' | '\'') {
                break;
            }

            match alacritty_terminal::grid::BidirectionalIterator::prev(&mut iter) {
                Some(indexed) => c = indexed.cell.c,
                None => break,
            }
        }

        if start > iter.point() {
            None
        } else {
            Some(start..=iter.point())
        }
    }

    /// Loop over submatches until a non-empty post-processed match is found.
    fn next_processed_match(&mut self, mut regex_match: alacritty_terminal::term::search::Match) {
        self.next_match = loop {
            if let Some(next_match) = self.hint_post_processing(&regex_match) {
                self.start = next_match.end().add(self.term, Boundary::Grid, 1);
                break Some(next_match);
            }

            self.start = regex_match.start().add(self.term, Boundary::Grid, 1);
            if self.start > self.end {
                return;
            }

            match self
                .term
                .regex_search_right(self.regex, self.start, self.end)
            {
                Some(rm) => regex_match = rm,
                None => return,
            }
        };
    }
}

impl<'a, T> Iterator for HintPostProcessor<'a, T> {
    type Item = alacritty_terminal::term::search::Match;

    fn next(&mut self) -> Option<Self::Item> {
        let next_match = self.next_match.take()?;

        if self.start <= self.end
            && let Some(rm) = self
                .term
                .regex_search_right(self.regex, self.start, self.end)
        {
            self.next_processed_match(rm);
        }

        Some(next_match)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Ensure shutdown on terminal drop
        if let Err(err) = self.notifier.0.send(Msg::Shutdown) {
            log::warn!("Failed to send shutdown message on dropped terminal: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_block_elements_have_valid_rects() {
        for cp in 0x2580..=0x259F {
            let c = char::from_u32(cp).unwrap();
            assert!(is_block_element(c), "{c:?} should be a block element");

            for &(pos, size) in block_element_rects(c) {
                assert!(size[0] > 0.0 && size[1] > 0.0, "{c:?} rect not empty");
                assert!(pos[0] >= 0.0 && pos[1] >= 0.0, "{c:?} rect starts in cell");
                assert!(
                    pos[0] + size[0] <= 1.0 && pos[1] + size[1] <= 1.0,
                    "{c:?} rect ends in cell"
                );
            }

            match c {
                '\u{2591}' | '\u{2592}' | '\u{2593}' => {
                    assert_eq!(block_element_rects(c), &[([0.0, 0.0], [1.0, 1.0])]);
                }
                _ => assert_eq!(block_element_alpha(c), 1.0),
            }
        }

        let area = |c: char| -> f32 {
            block_element_rects(c)
                .iter()
                .map(|&(_, size)| size[0] * size[1])
                .sum()
        };
        assert_eq!(area('\u{2580}'), 0.5); // ▀
        assert_eq!(area('\u{2588}'), 1.0); // █
        assert_eq!(area('\u{258C}'), 0.5); // ▌
        assert_eq!(area('\u{2598}'), 0.25); // ▘
        assert_eq!(area('\u{259F}'), 0.75); // ▟
        // Vertical eighths from ▁ up to █ fill the cell in steps
        for (i, c) in ('\u{2581}'..='\u{2588}').enumerate() {
            assert_eq!(area(c), (i + 1) as f32 / 8.0);
        }
    }

    #[test]
    fn half_blocks_tile_seamlessly() {
        // The bottom edge of an upper half block must meet the top edge of a
        // lower half block in the row below exactly at the cell boundary
        let (upper_pos, upper_size) = block_element_rects('\u{2580}')[0];
        assert_eq!((upper_pos, upper_size), ([0.0, 0.0], [1.0, 0.5]));
        let (lower_pos, lower_size) = block_element_rects('\u{2584}')[0];
        assert_eq!((lower_pos, lower_size), ([0.0, 0.5], [1.0, 0.5]));
        assert_eq!(upper_pos[1] + upper_size[1], lower_pos[1]);

        assert_eq!(block_element_rects('\u{2588}')[0], ([0.0, 0.0], [1.0, 1.0]));
        assert_eq!(block_element_alpha('\u{2591}'), 0.25);
        assert_eq!(block_element_alpha('\u{2592}'), 0.5);
        assert_eq!(block_element_alpha('\u{2593}'), 0.75);
    }

    #[test]
    fn builtin_glyph_coverage() {
        // All box drawing characters, including the diagonals, all block
        // elements and segment blocks, the sextant mosaics and the powerline
        // symbols are drawn by the terminal itself
        for cp in (0x2500..=0x259F)
            .chain(0x1FB00..=0x1FB3B)
            .chain(0x1FB82..=0x1FB8B)
        {
            let c = char::from_u32(cp).unwrap();
            assert!(is_builtin_glyph(c), "{c:?} should be a builtin glyph");
        }
        for c in '\u{E0B0}'..='\u{E0B3}' {
            assert!(is_builtin_glyph(c), "{c:?} should be a builtin glyph");
        }
        assert!(!is_builtin_glyph('a'));
        assert!(!is_builtin_glyph('✓'));
        // Neighboring Symbols for Legacy Computing and powerline code points
        // stay font glyphs
        for cp in [0x1FB3C, 0x1FB70, 0x1FB8C, 0xE0AF, 0xE0B4] {
            let c = char::from_u32(cp).unwrap();
            assert!(!is_builtin_glyph(c), "{c:?} should be a font glyph");
        }
    }

    #[test]
    fn box_drawing_rects_stay_in_cell() {
        for cp in 0x2500..=0x257F {
            let c = char::from_u32(cp).unwrap();
            for &(pos, size, alpha) in &box_drawing_rects(c, 9.0, 21.0) {
                assert!(size[0] > 0.0 && size[1] > 0.0, "{c:?} rect not empty");
                assert!(pos[0] >= 0.0 && pos[1] >= 0.0, "{c:?} rect starts in cell");
                assert!(
                    pos[0] + size[0] <= 1.0 + 1e-5 && pos[1] + size[1] <= 1.0 + 1e-5,
                    "{c:?} rect ends in cell"
                );
                assert!(
                    (0.0..=1.0).contains(&alpha),
                    "{c:?} alpha {alpha} out of range"
                );
            }
        }
    }

    #[test]
    fn box_drawing_lines_reach_cell_edges() {
        // With a 9x21 cell the light stroke is 1px and the heavy stroke 2px.
        let rects = |c: char| box_drawing_rects(c, 9.0, 21.0);

        assert_eq!(
            rects('\u{2503}'),
            vec![([3.5 / 9.0, 0.0], [2.0 / 9.0, 1.0], 1.0)]
        );
        assert_eq!(
            rects('\u{2502}'),
            vec![([4.0 / 9.0, 0.0], [1.0 / 9.0, 1.0], 1.0)]
        );
        assert_eq!(
            rects('\u{2500}'),
            vec![([0.0, 10.0 / 21.0], [1.0, 1.0 / 21.0], 1.0)]
        );
        assert_eq!(
            rects('\u{2501}'),
            vec![([0.0, 9.5 / 21.0], [1.0, 2.0 / 21.0], 1.0)]
        );
        assert_eq!(
            rects('\u{2551}'),
            vec![
                ([2.5 / 9.0, 0.0], [1.0 / 9.0, 1.0], 1.0),
                ([5.5 / 9.0, 0.0], [1.0 / 9.0, 1.0], 1.0),
            ]
        );

        // Corners and half lines meet the edges they point at: ┌ reaches the
        // bottom and the right edge, ╵ only the top half of the vertical
        let corner = rects('\u{250C}');
        assert_eq!(corner.len(), 2);
        assert!(corner.iter().any(|&(_, size, _)| size == [1.0 / 9.0, 0.5]));
        assert!(corner.iter().any(|&(_, size, _)| size == [0.5, 1.0 / 21.0]));
        assert_eq!(
            rects('\u{2575}'),
            vec![([4.0 / 9.0, 0.0], [1.0 / 9.0, 0.5], 1.0)]
        );
    }

    #[test]
    fn box_drawing_lines_tile_seamlessly() {
        let cw = 9.0;
        let ch = 21.0;

        assert_eq!(box_drawing_rects('\u{2504}', cw, ch).len(), 3); // ┄ triple
        assert_eq!(box_drawing_rects('\u{2508}', cw, ch).len(), 4); // ┈ quadruple
        assert_eq!(box_drawing_rects('\u{254C}', cw, ch).len(), 2); // ╌ double

        // Every vertical stroke of the vertical line and dash characters is
        // centered on the cell's horizontal center line, so borders mixing
        // them keep their columns aligned.
        for c in [
            '\u{2502}', '\u{2503}', '\u{2506}', '\u{2507}', '\u{250A}', '\u{250B}', '\u{254E}',
            '\u{254F}', '\u{2551}',
        ] {
            let centers: Vec<f32> = box_drawing_rects(c, cw, ch)
                .iter()
                .map(|&(pos, size, _)| pos[0] + size[0] / 2.0)
                .collect();
            let mid = centers.iter().sum::<f32>() / centers.len() as f32;
            assert!((mid - 0.5).abs() < 1e-5, "{c:?} strokes centered");
        }
        for c in [
            '\u{2500}', '\u{2501}', '\u{2504}', '\u{2505}', '\u{2508}', '\u{2509}', '\u{254C}',
            '\u{254D}', '\u{2550}',
        ] {
            let centers: Vec<f32> = box_drawing_rects(c, cw, ch)
                .iter()
                .map(|&(pos, size, _)| pos[1] + size[1] / 2.0)
                .collect();
            let mid = centers.iter().sum::<f32>() / centers.len() as f32;
            assert!((mid - 0.5).abs() < 1e-5, "{c:?} strokes centered");
        }

        // A single stroke meeting a double stroke stops at its outer edge:
        // ╤'s stem starts below the lower line of the double horizontal and
        // runs to the bottom edge, leaving the gap between the lines open.
        let stem = box_drawing_rects('\u{2564}', cw, ch)
            .into_iter()
            .find(|&(_, size, _)| size[0] < 0.2)
            .unwrap();
        assert!(stem.0[1] > 0.5, "stem starts below the cell center");
        assert_eq!(stem.0[1] + stem.1[1], 1.0, "stem runs to the bottom edge");
    }

    #[test]
    fn box_drawing_arcs_connect_edge_midpoints() {
        // ╰ connects the vertical stroke of the cell above (at the horizontal
        // center) with the horizontal stroke of the cell to its right (at the
        // vertical center).
        let rects = box_drawing_rects('\u{2570}', 9.0, 21.0);
        assert!(rects.iter().any(|&(pos, size, _)| {
            let cx = pos[0] + size[0] / 2.0;
            pos[1] == 0.0 && (cx - 0.5).abs() < 0.01 && size[1] > 0.0
        }));
        assert!(rects.iter().any(|&(pos, size, _)| {
            let cy = pos[1] + size[1] / 2.0;
            pos[0] + size[0] > 0.99 && (cy - 0.5).abs() < 0.01 && size[0] > 0.0
        }));
    }

    #[test]
    fn box_drawing_arcs_match_hand_rasterized_reference() {
        // '╯' in a 5x7 cell with a 1px stroke: a quarter circle of radius 3
        // centered at (0, 1), drawn as a distance field with antialiased
        // borders, plus the straight connector from the top edge to the arc.
        let pixels: Vec<(usize, usize, f32)> = box_drawing_rects('\u{256F}', 5.0, 7.0)
            .into_iter()
            .map(|(pos, size, alpha)| {
                assert!((size[0] * 5.0 - 1.0).abs() < 1e-5, "{pos:?} {size:?}");
                assert!((size[1] * 7.0 - 1.0).abs() < 1e-5, "{pos:?} {size:?}");
                (
                    (pos[0] * 5.0).round() as usize,
                    (pos[1] * 7.0).round() as usize,
                    alpha,
                )
            })
            .collect();
        let expected = [
            ((2, 0), 1.0), // connector from the top edge
            ((2, 1), 1.0), // stroke peak, one stroke inside the outer radius
            ((1, 2), 0.414_214),
            ((2, 2), 0.763_932),
            ((0, 3), 1.0), // meets the horizontal stroke at the vertical center
            ((1, 3), 0.763_932),
            ((2, 3), 0.171_573),
        ];
        assert_eq!(pixels.len(), expected.len());
        for (pixel, (at, alpha)) in pixels.iter().zip(expected) {
            assert_eq!((pixel.0, pixel.1), at, "alpha {alpha} vs {pixel:?}");
            assert!(
                (pixel.2 - alpha).abs() < 1e-5,
                "alpha at {at:?}: {} vs {alpha}",
                pixel.2
            );
        }
    }

    #[test]
    fn box_drawing_arcs_are_mirrors_of_each_other() {
        // Pixel coverage of a corner, optionally mirrored on an axis the way
        // `rounded_corner_pixels` mirrors its base '╯': like reflecting an
        // image, an axis whose pixel count and stroke disagree in parity
        // duplicates the outermost column or row. Alphas are compared
        // quantized to 1/255 steps.
        let coverage = |c: char, w: f32, h: f32| -> Vec<(usize, usize, u32)> {
            let mut pixels: Vec<(usize, usize, u32)> = box_drawing_rects(c, w, h)
                .into_iter()
                .map(|(pos, _, alpha)| {
                    (
                        (pos[0] * w).round() as usize,
                        (pos[1] * h).round() as usize,
                        (alpha * 255.0).round() as u32,
                    )
                })
                .collect();
            pixels.sort_unstable();
            pixels
        };
        let mirrored = |c: char, w: f32, h: f32, fx: bool, fy: bool| -> Vec<(usize, usize, u32)> {
            let (iw, ih) = (w as usize, h as usize);
            let stroke = (w / 8.0).round().max(1.0) as usize;
            let extra_x = usize::from(stroke % 2 != iw % 2);
            let extra_y = usize::from(stroke % 2 != ih % 2);
            let mut pixels: Vec<(usize, usize, u32)> = coverage(c, w, h)
                .into_iter()
                .flat_map(|(x, y, a)| {
                    let mut mapped = vec![(x, y, a)];
                    if fx && x == 0 && extra_x == 1 {
                        mapped.push((iw - 1, y, a));
                    }
                    if fy && y == 0 && extra_y == 1 {
                        mapped.push((x, ih - 1, a));
                    }
                    if fx {
                        mapped[0].0 = iw - 1 - x - extra_x;
                    }
                    if fy {
                        mapped[0].1 = ih - 1 - y - extra_y;
                    }
                    mapped
                })
                .collect();
            pixels.sort_unstable();
            pixels
        };
        let close = |a: &[(usize, usize, u32)], b: &[(usize, usize, u32)]| {
            assert_eq!(a.len(), b.len(), "{a:?} vs {b:?}");
            for (p, q) in a.iter().zip(b) {
                assert_eq!(p, q, "mismatched pixel");
            }
        };
        // Cell sizes exercising odd/even width, height and stroke parity.
        for (w, h) in [(9.0, 21.0), (10.0, 21.0), (21.0, 9.0), (10.0, 20.0)] {
            // ╰ is the base '╯' mirrored on the X axis, ╮ on the Y axis, and
            // ╭ on both.
            close(
                &mirrored('\u{256F}', w, h, true, false),
                &coverage('\u{2570}', w, h),
            );
            close(
                &mirrored('\u{256F}', w, h, false, true),
                &coverage('\u{256E}', w, h),
            );
            close(
                &mirrored('\u{256F}', w, h, true, true),
                &coverage('\u{256D}', w, h),
            );
        }
    }

    #[test]
    fn box_drawing_arc_stem_has_no_jog() {
        // The curve of '╰' must leave the vertical stem without a sideways
        // step: the stem column keeps full coverage down to where the arc
        // starts, and nothing pokes out beside it above that point.
        let rects = box_drawing_rects('\u{2570}', 9.0, 21.0);
        let at = |x: usize, y: usize| {
            rects
                .iter()
                .find(|&&(pos, _, _)| {
                    (pos[0] * 9.0).round() as usize == x && (pos[1] * 21.0).round() as usize == y
                })
                .map(|&(_, _, alpha)| alpha)
                .unwrap_or(0.0)
        };
        for y in 0..=6 {
            assert_eq!(at(4, y), 1.0, "stem pixel (4, {y}) fully covered");
            assert_eq!(at(3, y), 0.0, "nothing left of the stem at row {y}");
            assert_eq!(at(5, y), 0.0, "nothing right of the stem at row {y}");
        }
        // From there the arc sweeps right, reaching the vertical center at
        // the right edge, where the horizontal stroke of the next cell
        // attaches.
        assert_eq!(at(8, 10), 1.0);
        assert_eq!(at(8, 9), 0.0);
        assert_eq!(at(8, 11), 0.0);
    }

    #[test]
    fn box_drawing_diagonals_run_corner_to_corner_without_holes() {
        // The regression this test guards against: font-rendered diagonals do
        // not fill the cell height, leaving holes at every line boundary and
        // sawtoothing back to the glyph's left bearing.
        for c in ['\u{2571}', '\u{2572}'] {
            let rects = box_drawing_rects(c, 9.0, 21.0);
            assert!(!rects.is_empty(), "{c:?} has rects");

            // Antialiased: partial coverage pieces besides full ones.
            assert!(rects.iter().any(|&(_, _, alpha)| alpha < 1.0));
            assert!(
                rects
                    .iter()
                    .all(|&(_, _, alpha)| (0.0..=1.0).contains(&alpha))
            );

            // The coverage reaches all four cell edges, so consecutive
            // diagonal characters connect at the shared corners.
            type Rect = ([f32; 2], [f32; 2], f32);
            type EdgeTest = fn(&Rect) -> bool;
            let touches = |edge: EdgeTest| rects.iter().any(edge);
            assert!(touches(|&(pos, _, _)| pos[1] <= 1e-5), "{c:?} top edge");
            assert!(
                touches(|&(pos, size, _)| pos[1] + size[1] >= 1.0 - 1e-5),
                "{c:?} bottom edge"
            );
            assert!(touches(|&(pos, _, _)| pos[0] <= 1e-5), "{c:?} left edge");
            assert!(
                touches(|&(pos, size, _)| pos[0] + size[0] >= 1.0 - 1e-5),
                "{c:?} right edge"
            );

            // No holes: every pixel row of the cell carries coverage.
            let mut rows = [false; 21];
            for &(pos, size, alpha) in &rects {
                if alpha > 0.0 {
                    let first = (pos[1] * 21.0).round() as usize;
                    let last = ((pos[1] + size[1]) * 21.0).round() as usize;
                    rows[first..last.min(21)].fill(true);
                }
            }
            assert!(rows.iter().all(|&row| row), "{c:?} covers every row");
        }
    }

    #[test]
    fn box_drawing_cross_has_both_diagonals() {
        // ╳ diverges from the top-left and the top-right corner at once
        let rects = box_drawing_rects('\u{2573}', 9.0, 21.0);
        let starts_left = rects
            .iter()
            .any(|&(pos, _, _)| pos[0] <= 1e-5 && pos[1] <= 1e-5);
        let starts_right = rects
            .iter()
            .any(|&(pos, size, _)| pos[0] + size[0] >= 1.0 - 1e-5 && pos[1] <= 1e-5);
        assert!(starts_left, "╳ reaches the top left corner");
        assert!(starts_right, "╳ reaches the top right corner");

        // And it is the union of both single diagonals
        let single = |c: char| box_drawing_rects(c, 9.0, 21.0).len();
        assert_eq!(rects.len(), single('\u{2571}') + single('\u{2572}'));
    }

    #[test]
    fn segment_blocks_fill_their_namesakes() {
        let area = |c: char| -> f32 {
            block_element_rects(c)
                .iter()
                .map(|&(_, size)| size[0] * size[1])
                .sum()
        };

        for cp in 0x1FB82..=0x1FB8B {
            let c = char::from_u32(cp).unwrap();
            assert!(is_segment_block(c), "{c:?} should be a segment block");
            assert_eq!(block_element_rects(c).len(), 1, "{c:?} is a single rect");
            let &(pos, size) = &block_element_rects(c)[0];
            assert!(
                pos[0] >= 0.0
                    && pos[1] >= 0.0
                    && pos[0] + size[0] <= 1.0
                    && pos[1] + size[1] <= 1.0,
                "{c:?} rect stays in cell"
            );
            assert_eq!(block_element_alpha(c), 1.0);
        }

        // Upper blocks grow from a quarter to seven eighths, anchored at
        // the top edge of the cell
        for (i, c) in ('\u{1FB82}'..='\u{1FB86}').enumerate() {
            let height = [2.0 / 8.0, 3.0 / 8.0, 5.0 / 8.0, 6.0 / 8.0, 7.0 / 8.0][i];
            assert_eq!(
                block_element_rects(c)[0],
                ([0.0, 0.0], [1.0, height]),
                "{c:?}"
            );
        }
        // Right blocks are anchored at the right edge of the cell
        for (i, c) in ('\u{1FB87}'..='\u{1FB8B}').enumerate() {
            let width = [2.0 / 8.0, 3.0 / 8.0, 5.0 / 8.0, 6.0 / 8.0, 7.0 / 8.0][i];
            let &(pos, size) = &block_element_rects(c)[0];
            assert_eq!((size[0], size[1]), (width, 1.0), "{c:?}");
            assert_eq!(pos[0] + size[0], 1.0, "{c:?} touches the right edge");
        }

        // Upper blocks complement the Block Elements lower blocks exactly,
        // so stacks of them tile the cell without gaps or overlap
        assert!((area('\u{1FB82}') + area('\u{2586}') - 1.0).abs() < 1e-6); // 🮂 + ▆
        assert!((area('\u{1FB83}') + area('\u{2585}') - 1.0).abs() < 1e-6); // 🮃 + ▅
        assert!((area('\u{1FB84}') + area('\u{2583}') - 1.0).abs() < 1e-6); // 🮄 + ▃
        assert!((area('\u{1FB85}') + area('\u{2582}') - 1.0).abs() < 1e-6); // 🮅 + ▂
        assert!((area('\u{1FB86}') + area('\u{2581}') - 1.0).abs() < 1e-6); // 🮆 + ▁
    }

    #[test]
    fn sextants_match_their_unicode_names() {
        // Reference grids derived from the Unicode names BLOCK SEXTANT-N,
        // whose digits 1-6 enumerate the grid cells of the 2x3 mosaic in
        // reading order
        let cell = |digits: &str| {
            const CELLS: [([f32; 2], [f32; 2]); 6] = [
                ([0.0, 0.0], [0.5, 1.0 / 3.0]),
                ([0.5, 0.0], [0.5, 1.0 / 3.0]),
                ([0.0, 1.0 / 3.0], [0.5, 1.0 / 3.0]),
                ([0.5, 1.0 / 3.0], [0.5, 1.0 / 3.0]),
                ([0.0, 2.0 / 3.0], [0.5, 1.0 / 3.0]),
                ([0.5, 2.0 / 3.0], [0.5, 1.0 / 3.0]),
            ];
            digits
                .chars()
                .map(|d| CELLS[d.to_digit(10).unwrap() as usize - 1])
                .collect::<Vec<_>>()
        };

        assert_eq!(sextant_rects('\u{1FB00}'), cell("1")); // 🬀
        assert_eq!(sextant_rects('\u{1FB01}'), cell("2")); // 🬁
        assert_eq!(sextant_rects('\u{1FB02}'), cell("12")); // 🬂
        assert_eq!(sextant_rects('\u{1FB03}'), cell("3")); // 🬃
        assert_eq!(sextant_rects('\u{1FB07}'), cell("4")); // 🬇
        assert_eq!(sextant_rects('\u{1FB0F}'), cell("5")); // 🬏
        assert_eq!(sextant_rects('\u{1FB1E}'), cell("6")); // 🬞
        assert_eq!(sextant_rects('\u{1FB14}'), cell("235")); // 🬔
        assert_eq!(sextant_rects('\u{1FB1D}'), cell("12345")); // 🬝
        assert_eq!(sextant_rects('\u{1FB3B}'), cell("23456")); // 🬻
    }

    #[test]
    fn sextants_fill_whole_grid_cells() {
        for cp in 0x1FB00..=0x1FB3B {
            let c = char::from_u32(cp).unwrap();
            let rects = sextant_rects(c);
            // The empty, checkerboard and full mosaics have no code point,
            // so every character fills between one and five cells
            assert!(!rects.is_empty(), "{c:?} fills no cell");
            assert!(rects.len() <= 5, "{c:?} fills more than five cells");

            for &(pos, size) in &rects {
                assert_eq!(size, [0.5, 1.0 / 3.0], "{c:?} fills a whole cell");
                let column = (pos[0] * 2.0).round() as i32;
                let row = (pos[1] * 3.0).round() as i32;
                assert!(
                    (0..2).contains(&column) && (0..3).contains(&row),
                    "{c:?} rect {pos:?} snaps to the 2x3 grid"
                );
            }

            // No cell is filled twice
            let mut corners: Vec<_> = rects.iter().map(|&(pos, _)| pos).collect();
            corners.sort_by(|a, b| a.partial_cmp(b).unwrap());
            corners.dedup_by(|a, b| a == b);
            assert_eq!(corners.len(), rects.len(), "{c:?} fills a cell twice");

            let area: f32 = rects.iter().map(|&(_, size)| size[0] * size[1]).sum();
            assert!(
                (area - rects.len() as f32 / 6.0).abs() < 1e-6,
                "{c:?} covers {} sixths of the cell",
                rects.len()
            );
        }
    }

    #[test]
    fn powerline_triangles_match_hand_rasterized_reference() {
        // At 5x7 the diagonals start one pixel inside the corners, the runs
        // grow with the distance from them and meet in the middle row
        let mut rects = powerline_rects('\u{E0B0}', 5.0, 7.0);
        rects.sort_by(|a, b| (a.0[1], a.0[0]).partial_cmp(&(b.0[1], b.0[0])).unwrap());
        let expected: Vec<([f32; 2], [f32; 2], f32)> =
            [(1.0, 1.0), (2.0, 2.0), (3.0, 3.0), (2.0, 4.0), (1.0, 5.0)]
                .iter()
                .map(|&(w, y)| ([0.0, y / 7.0], [w / 5.0, 1.0 / 7.0], 1.0))
                .collect();
        assert_eq!(rects, expected);

        // At 3x7 the widest runs are clamped to the cell width
        let mut rects = powerline_rects('\u{E0B0}', 3.0, 7.0);
        rects.sort_by(|a, b| (a.0[1], a.0[0]).partial_cmp(&(b.0[1], b.0[0])).unwrap());
        assert_eq!(rects[2].1[0], 1.0, "middle row spans the whole cell");
    }

    #[test]
    fn powerline_arrows_match_hand_rasterized_reference() {
        // At 5x7 with stroke 1 the arrow is a dotted chevron meeting at the
        // center pixel
        let mut rects = powerline_rects('\u{E0B1}', 5.0, 7.0);
        rects.sort_by(|a, b| (a.0[1], a.0[0]).partial_cmp(&(b.0[1], b.0[0])).unwrap());
        let expected: Vec<([f32; 2], [f32; 2], f32)> =
            [(0.0, 1.0), (1.0, 2.0), (2.0, 3.0), (1.0, 4.0), (0.0, 5.0)]
                .iter()
                .map(|&(x, y)| ([x / 5.0, y / 7.0], [1.0 / 5.0, 1.0 / 7.0], 1.0))
                .collect();
        assert_eq!(rects, expected);

        // At 9x21 the diagonals reach the right edge before they meet and
        // are connected by a vertical tip on the last column
        let rects = powerline_rects('\u{E0B1}', 9.0, 21.0);
        assert_eq!(rects.len(), 17);
        assert!(
            rects.iter().any(|&(pos, size, _)| {
                (pos[0] - 8.0 / 9.0).abs() < 1e-6
                    && (pos[1] - 9.0 / 21.0).abs() < 1e-6
                    && (size[1] - 3.0 / 21.0).abs() < 1e-6
            }),
            "arrow tip connects the strokes on the last column"
        );
    }

    #[test]
    fn powerline_right_variants_mirror_left() {
        // Mirrored normalized coordinates can differ in the last ulp from
        // the ones mirrored in pixel space, so compare with a tolerance
        let close = |a: &[([f32; 2], [f32; 2], f32)], b: &[([f32; 2], [f32; 2], f32)]| {
            a.len() == b.len()
                && a.iter().zip(b.iter()).all(|(a, b)| {
                    let e = |x: f32, y: f32| (x - y).abs() < 1e-5;
                    e(a.0[0], b.0[0])
                        && e(a.0[1], b.0[1])
                        && e(a.1[0], b.1[0])
                        && e(a.1[1], b.1[1])
                        && e(a.2, b.2)
                })
        };
        for &(w, h) in &[(5.0, 7.0), (9.0, 21.0), (10.0, 20.0), (8.0, 16.0)] {
            for &(ltr, rtl) in &[('\u{E0B0}', '\u{E0B2}'), ('\u{E0B1}', '\u{E0B3}')] {
                let mut mirrored: Vec<([f32; 2], [f32; 2], f32)> = powerline_rects(ltr, w, h)
                    .iter()
                    .map(|&(pos, size, alpha)| ([1.0 - pos[0] - size[0], pos[1]], size, alpha))
                    .collect();
                mirrored.sort_by(|a, b| (a.0[1], a.0[0]).partial_cmp(&(b.0[1], b.0[0])).unwrap());
                let mut right = powerline_rects(rtl, w, h);
                right.sort_by(|a, b| (a.0[1], a.0[0]).partial_cmp(&(b.0[1], b.0[0])).unwrap());
                assert!(
                    close(&right, &mirrored),
                    "{ltr:?} mirrored is {rtl:?} at {w}x{h}:\n{right:?}\n{mirrored:?}"
                );
            }
        }
    }

    #[test]
    fn powerline_fits_only_wide_enough_cells() {
        assert!(powerline_fits(9.0, 21.0));
        assert!(powerline_fits(10.0, 21.0));
        assert!(powerline_fits(8.0, 16.0));
        assert!(!powerline_fits(8.0, 21.0), "the tip is cut off too hard");
        assert!(
            !powerline_fits(5.0, 21.0),
            "the tip is cut off way too hard"
        );
    }

    #[test]
    fn powerline_rects_stay_in_cell() {
        for &(w, h) in &[(9.0, 21.0), (5.79, 21.3), (10.5, 20.0), (6.0, 12.0)] {
            for c in '\u{E0B0}'..='\u{E0B3}' {
                for &(pos, size, alpha) in &powerline_rects(c, w, h) {
                    assert!(size[0] > 0.0 && size[1] > 0.0, "{c:?} rect not empty");
                    assert!(
                        pos[0] >= 0.0 && pos[1] >= 0.0 && pos[0] + size[0] <= 1.0 + 1e-5,
                        "{c:?} rect starts in cell"
                    );
                    assert!(pos[1] + size[1] <= 1.0 + 1e-5, "{c:?} rect ends in cell");
                    assert!((0.0..=1.0).contains(&alpha), "{c:?} alpha {alpha} in range");
                }
            }
        }
    }

    #[test]
    fn builtin_glyph_rects_dispatch_and_cache() {
        // The dispatcher routes every builtin family to its geometry
        // unchanged, with shade alphas folded per rectangle
        for &(c, w, h) in &[
            ('\u{2500}', 9.0, 21.0),
            ('\u{256E}', 9.0, 21.0),
            ('\u{2591}', 9.0, 21.0),
            ('\u{2588}', 9.0, 21.0),
            ('\u{1FB00}', 9.0, 21.0),
            ('\u{1FB3B}', 9.0, 21.0),
            ('\u{1FB87}', 9.0, 21.0),
            ('\u{E0B0}', 9.0, 21.0),
            ('\u{E0B1}', 9.0, 21.0),
            ('\u{256D}', 5.79, 21.3),
        ] {
            let expected: Vec<([f32; 2], [f32; 2], f32)> = if is_block_element(c) {
                block_element_rects(c)
                    .iter()
                    .map(|&(pos, size)| (pos, size, block_element_alpha(c)))
                    .collect()
            } else if is_segment_block(c) {
                block_element_rects(c)
                    .iter()
                    .map(|&(pos, size)| (pos, size, 1.0))
                    .collect()
            } else if is_sextant(c) {
                sextant_rects(c)
                    .into_iter()
                    .map(|(pos, size)| (pos, size, 1.0))
                    .collect()
            } else if is_powerline(c) {
                powerline_rects(c, w, h)
            } else {
                box_drawing_rects(c, w, h)
            };
            assert_eq!(builtin_glyph_rects(c, w, h).as_ref(), expected, "{c:?}");
        }

        // Repeats at the same metrics are served from the cache
        let first = builtin_glyph_rects('\u{256D}', 9.0, 21.0);
        let second = builtin_glyph_rects('\u{256D}', 9.0, 21.0);
        assert!(Arc::ptr_eq(&first, &second));
    }
}
