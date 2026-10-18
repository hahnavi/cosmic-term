cosmic-terminal = COSMIC Terminal
comment = Terminal emulator for the COSMIC desktop
keywords = Command;Shell;Terminal;CLI;
new-terminal = New terminal
sessions = Sessions
local-shell = Local shell
new-ssh-session = New SSH session...
edit-ssh-session = Edit SSH session
connect-ssh-session = Connect
edit-session = Edit
delete-session = Delete
save = Save
done = Done
session-name = Session name
ssh-host = Host or SSH config alias
ssh-username = Username (optional)
ssh-port = Port
ssh-connection = Connection
ssh-identity-file = Private key file (optional)
ssh-jump-host = SSH gateway / jump host (optional)
ssh-execute-command = Execute command after connecting (optional)
ssh-options = Options
ssh-compression = Compress connection
ssh-send-locale = Send local locale variables
ssh-keep-open = Open an interactive shell after the command
import-ssh-sessions = Import SSH sessions...
export-ssh-sessions = Export SSH sessions...
ssh-session-manager = SSH session manager
ssh-sessions-exported = Exported { $count } SSH sessions.
ssh-sessions-export-failed = Could not export SSH sessions: { $error }
ssh-sessions-import-summary = Imported { $imported } SSH sessions; skipped { $skipped }.
ssh-host-required = Enter a host or SSH config alias.
ssh-host-invalid = Enter a host or alias without spaces, @, or a leading hyphen.
ssh-username-invalid = Enter a username without spaces, @, or a leading hyphen.
ssh-jump-host-invalid = Enter a jump host without spaces, commas, or a leading hyphen.
ssh-port-invalid = Port must be a number from 1 to 65535.
ssh-session-limit = Could not allocate an ID for this SSH session.
terminal-spawn-failed = Failed to start the terminal process: { $error }
retry = Retry
toggle-sidebar = Toggle sidebar
confirm-delete-ssh-session = Delete SSH session “{ $name }”?
ssh-session-save-failed = Could not save SSH sessions: { $error }

# Context Pages

## About
repository = Repository
support = Support

## Color schemes
color-schemes = Color schemes
rename = Rename
export = Export
delete = Delete
import = Import
import-errors = Import errors

## Profiles
profiles = Profiles
name = Name
command-line = Command line
tab-title = Tab title
tab-title-description = Override the default tab title
add-profile = Add profile
new-profile = New profile
make-default = Make default
working-directory = Working directory
hold = Hold
remain-open = Remain open after child process exits.

## Settings
settings = Settings

### Appearance
appearance = Appearance
theme = Theme
match-desktop = Match desktop
dark = Dark
light = Light
syntax-dark = Color scheme dark
syntax-light = Color scheme light
default-zoom-step = Zoom steps
opacity = Background opacity

### Font
font = Font
advanced-font-settings = Advanced font settings
default-font = Font
default-font-size = Font size
default-font-stretch = Font stretch
default-font-weight = Normal font weight
default-dim-font-weight = Dim font weight
default-bold-font-weight = Bold font weight
use-bright-bold = Make bold text brighter
font-ligatures = Font ligatures

### Splits
splits = Splits
focus-follow-mouse = Typing focus follows mouse
show-pane-borders = Show pane borders

### Advanced
advanced = Advanced
graphics-mode = Graphics mode
graphics-mode-description = Restart COSMIC Terminal to apply changes.
graphics-mode-low-power = Low power GPU
graphics-mode-high-performance = High performance GPU
graphics-mode-software = Software (CPU)
show-headerbar = Show header
window-header = Window header
show-header-description = Reveal the header from the right-click menu
tab-new-inherit-working-directory = New tabs and windows use current directory
tab-new-inherit-working-directory-description = Open new tabs and windows in the active tab's working directory
scrollback-lines = Scrollback lines
scrollback-lines-description = Lines of output kept per terminal for scrolling back. Zero disables scrollback; higher values use more memory.

### Keyboard shortcuts
add-another-keybinding = Add another keybinding
cancel = Cancel
close-window = Close window
copy-or-sigint = Copy or SIGINT
disable = Disable
focus-pane-down = Focus pane down
focus-pane-left = Focus pane left
focus-pane-right = Focus pane right
focus-pane-up = Focus pane up
keyboard-shortcuts = Keyboard shortcuts
menu-keyboard-shortcuts = Keyboard shortcuts...
no-shortcuts = No shortcuts
password-manager = Password manager
paste-primary = Paste primary
replace = Replace
reset-to-default = Reset to default
shortcut-capture-hint = Press the key combination
shortcut-group-clipboard = Clipboard
shortcut-group-other = Other
shortcut-group-tabs = Tabs
shortcut-group-window = Window
shortcut-group-zoom = Zoom
shortcut-replace-body = { $binding } is already assigned to { $existing }. Replace it with { $new_action }?
shortcut-replace-title = Replace shortcut?
tab-activate = Activate tab { $number }
toggle-fullscreen = Toggle fullscreen
type-to-search = Type to search...

# Find
find-placeholder = Find...
find-previous = Find previous
find-next = Find next

# Menu

## File
file = File
new-tab = New tab
new-window = New window
profile = Profile
menu-profiles = Profiles...
close-tab = Close tab
quit = Quit

## Edit
edit = Edit
copy = Copy
paste = Paste
select-all = Select all
find = Find
clear-scrollback = Clear scrollback

## Open
open-link = Open Link
copy-link = Copy Link

## View
view = View
zoom-in = Larger text
zoom-reset = Default text size
zoom-out = Smaller text
next-tab = Next tab
previous-tab = Previous tab
split-horizontal = Split horizontal
split-vertical = Split vertical
pane-toggle-maximize = Maximize or restore pane
menu-color-schemes = Color schemes...
menu-settings = Settings...
menu-about = About COSMIC Terminal...

# Password Manager
menu-password-manager = Passwords...
passwords-title = Passwords
add-password = Add Password
password-input = Password
password-input-description = Description
