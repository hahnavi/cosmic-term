# cosmic-term
COSMIC terminal emulator, built using [alacritty\_terminal](https://docs.rs/alacritty_terminal) that is provided by the [alacritty](https://github.com/alacritty/alacritty) project. `cosmic-term` provides bidirectional rendering and ligatures with a custom renderer based on [cosmic-text](https://github.com/pop-os/cosmic-text).

GPU rendering uses `wgpu` and is selected by default with a low-power adapter
preference. The renderer can be changed in **Settings → Advanced → Graphics
mode** to prefer a high-performance GPU or use software rendering with
`softbuffer` and `tiny-skia`. Changes take effect after restarting COSMIC
Terminal. The software renderer is also used automatically if GPU rendering
cannot initialize.

## Color Schemes

Custom color schemes can be imported from the `View -> Color schemes...` menu item.
You can find templates for color schemes in the [color-schemes](color-schemes) folder.
