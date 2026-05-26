# Keybindings

Roost uses platform-native modifiers: **Cmd** on macOS, **Ctrl** plus **Alt** on Linux. The same actions are available on both platforms — only the modifier differs.

## macOS

### Tab management (active project)

| Shortcut         | Action                                          |
|------------------|-------------------------------------------------|
| `Cmd-T`          | New tab                                         |
| `Cmd-W`          | Close the active tab                            |
| `Cmd-R`          | Rename the active tab                           |
| `Cmd-Shift-]`    | Cycle to the next tab (stops at the last tab; no wrap-around) |
| `Cmd-Shift-[`    | Cycle to the previous tab (stops at the first tab; no wrap-around) |
| `Ctrl-1` … `Ctrl-9` | Switch to tab at position 1 .. 9             |

### Project management

| Shortcut             | Action                                       |
|----------------------|----------------------------------------------|
| `Cmd-N`              | Create a new project (`untitled`, `untitled 2`, …) |
| `Cmd-Shift-R`        | Rename the active project                   |
| `Cmd-Shift-W`        | Close the active project (confirms when the project has 2+ tabs) |
| `Cmd-B`              | Toggle the projects sidebar                 |
| `Cmd-Shift-U`        | Jump to the next tab with a pending notification (active project first, then others) |
| `Cmd-1` … `Cmd-9`    | Switch to the project at sidebar position 1 .. 9 |

### Commands

| Shortcut         | Action                                          |
|------------------|-------------------------------------------------|
| `Cmd-Shift-P`    | Open the command palette (fuzzy-find + run any command) |
| `Cmd-Shift-T`    | Open the command launcher (run a configured `command =` entry in a new tab) |

### Clipboard

| Shortcut         | Action                                              |
|------------------|-----------------------------------------------------|
| `Cmd-C`          | Copy the current terminal selection                 |
| `Cmd-V`          | Paste the system clipboard into the active terminal |
| `Ctrl-Shift-C`   | Same as `Cmd-C` (terminal-convention alternate)     |
| `Ctrl-Shift-V`   | Same as `Cmd-V` (terminal-convention alternate)     |

Bare `Ctrl-C` is left as SIGINT — it's not overloaded for copy.

### Font sizing

| Shortcut         | Action                                          |
|------------------|-------------------------------------------------|
| `Cmd-+` / `Cmd-=` | Increase font size for the active tab          |
| `Cmd--`          | Decrease font size for the active tab           |
| `Cmd-0`          | Reset font size to the `font_size` from config  |

Font size adjustments are per-tab and held in memory only. They do not persist across restarts, and new tabs always start at `font_size` from `config.conf`. The size is clamped to 6 .. 72 points; out-of-range steps saturate.

## Linux

### Tab management (active project)

| Shortcut         | Action                                          |
|------------------|-------------------------------------------------|
| `Ctrl-T`         | New tab                                         |
| `Ctrl-W`         | Close the active tab                            |
| `Alt-R`          | Rename the active tab                           |
| `Ctrl-Shift-]`   | Cycle to the next tab (stops at the last tab; no wrap-around) |
| `Ctrl-Shift-[`   | Cycle to the previous tab (stops at the first tab; no wrap-around) |
| `Ctrl-1` … `Ctrl-9` | Switch to tab at position 1 .. 9             |

### Project management

| Shortcut             | Action                                       |
|----------------------|----------------------------------------------|
| `Alt-N`              | Create a new project (`untitled`, `untitled 2`, …) |
| `Alt-Shift-R`        | Rename the active project                   |
| `Alt-Shift-W`        | Close the active project (confirms when the project has 2+ tabs) |
| `Alt-B`              | Toggle the projects sidebar                 |
| `Ctrl-Shift-U`       | Jump to the next tab with a pending notification (active project first, then others) |
| `Alt-1` … `Alt-9`    | Switch to the project at sidebar position 1 .. 9 |

### Commands

| Shortcut         | Action                                          |
|------------------|-------------------------------------------------|
| `Alt-Shift-P`    | Open the command palette (fuzzy-find + run any command) |
| `Alt-Shift-T`    | Open the command launcher (run a configured `command =` entry in a new tab) |

### Clipboard

| Shortcut         | Action                                              |
|------------------|-----------------------------------------------------|
| `Alt-C`          | Copy the current terminal selection                 |
| `Alt-V`          | Paste the system clipboard into the active terminal |
| `Ctrl-Shift-C`   | Same as `Alt-C` (terminal-convention alternate)     |
| `Ctrl-Shift-V`   | Same as `Alt-V` (terminal-convention alternate)     |
| Middle-click     | Paste the X11/Wayland PRIMARY selection into the focused terminal |

Bare `Ctrl-C` is left as SIGINT — it's not overloaded for copy. Copying (or simply finishing a drag-selection) also writes to the X11/Wayland PRIMARY clipboard, so middle-click paste in other apps — and into Roost — works.

### Font sizing

| Shortcut         | Action                                          |
|------------------|-------------------------------------------------|
| `Ctrl-+` / `Ctrl-=` | Increase font size for the active tab        |
| `Ctrl--`         | Decrease font size for the active tab           |
| `Ctrl-0`         | Reset font size to the `font_size` from config  |

Font size adjustments are per-tab and held in memory only. They do not persist across restarts, and new tabs always start at `font_size` from `config.conf`. The size is clamped to 6 .. 72 points; out-of-range steps saturate.

## Terminal keys

Anything not bound as an app shortcut flows to the focused terminal through libghostty-vt's key encoder, which produces the right escape sequence for the foreground app's current modes (legacy xterm, application-cursor, Kitty keyboard protocol, etc.):

| Key                    | Effect                                                                              |
|------------------------|-------------------------------------------------------------------------------------|
| Printable characters   | Sent to the shell as typed                                                          |
| Enter, Backspace, Tab  | Sent to the shell                                                                   |
| Shift-Tab              | Sent as `\x1b[Z` (back-tab) — used by Claude Code to cycle modes                    |
| Shift-Enter            | Disambiguated from Enter (xterm modifyOtherKeys form, or Kitty CSI-u when the app opts in) — used by Claude Code for newlines in its prompt |
| Arrow keys, Home, End  | Standard CSI in normal mode; SS3 (`\x1bOA/B/C/D`) in application-cursor mode (e.g. inside `vim`'s `:` prompt) |
| Page Up / Page Down    | Sent as standard CSI sequences                                                      |
| Esc                    | Sent to the shell                                                                   |
| `Ctrl-letter`          | Sent as the corresponding control byte (`Ctrl-C` → SIGINT, etc.)                    |
| macOS `Option-letter`  | Composes Unicode (Option+e e → `é`); does **not** behave as Alt                     |

`Ctrl-1` … `Ctrl-9` are reserved for tab switching and do not reach the shell. On macOS, all `Cmd-*` and `Super-*` combinations are reserved for app shortcuts and never reach the shell.

## How the shortcut controller is wired

Shortcuts run in GTK's *capture* phase, which means they fire before the focused widget — including the terminal surface — sees the event. That's why `Cmd-T` (or `Ctrl-T` on Linux) works while the terminal is focused: the window's controller catches it first and dispatches the action, and the keystroke never reaches the shell. Anything not bound at the window level falls through to the terminal as usual.

The terminal's own key controller is also in capture phase — that's what stops GTK's default focus-traversal from consuming Tab and Shift-Tab before the shell (or Claude Code) sees them.

## Mouse

| Action                                | Effect                                                                                       |
|---------------------------------------|----------------------------------------------------------------------------------------------|
| Click                                 | Focus the terminal                                                                           |
| Click + drag                          | Select cells; a translucent accent overlay highlights the selection ribbon                   |
| Wheel / two-finger scroll             | Scroll the terminal scrollback (smooth-scroll on macOS trackpads)                            |
| Wheel on alt-screen apps (vim, less, jed) | Translated to `ArrowUp` / `ArrowDown` keystrokes so trackpad navigation works                |
| Click in a mouse-tracking app (vim with `:set mouse=a`, htop, tmux) | Forwarded to the app as encoded mouse-event escape sequences                                 |
| Wheel in a mouse-tracking app         | Forwarded as button-4 / button-5 press+release pairs                                         |
| **Shift-click / Shift-drag / Shift-wheel** | Bypasses mouse-tracking (xterm convention) so you can always select / scroll locally even when the app is grabbing the mouse |

Selection clears automatically on any PTY output, on resize, and on a new click. (Most terminals clear-on-any-output rather than tracking which rows changed; matches user expectation and avoids the bookkeeping.)

Pressing any input-producing key when the viewport is scrolled back snaps the viewport to the bottom before delivering the keystroke — same behavior as every other terminal multiplexer.

## Sidebar mouse

The sidebar still supports mouse-driven rename: double-click a project row to rename it inline, or right-click for a Rename / Close menu.

If you close the last tab in a project, Roost closes that project too. The "Are you sure?" confirmation dialog only appears for explicit close-project actions (the sidebar X button or the right-click menu); `Cmd-W` / `Ctrl-W` on the final tab closes the project silently.

Tab titles set via `Cmd-R` / `Alt-R` are persisted and locked: subsequent OSC 1/2 escapes from the shell (`\e]2;new-title\a`, common in shell prompts) are silently ignored on a renamed tab. The same lock applies to titles set via `roostctl set-title --tab <id> --title "..."`. v1 has no in-app way to clear the lock; renaming again with `Cmd-R` / `Alt-R` updates the displayed label but the lock stays on. To revert to shell-driven titles, delete and recreate the tab.

## Custom keybindings

Roost reads `~/.config/roost/config.conf` (more precisely `$XDG_CONFIG_HOME/roost/config.conf`) on both platforms. The keybinding syntax mirrors [Ghostty](https://ghostty.org/docs/config/keybind):

```conf
keybind = trigger = action
```

Each `keybind` line either binds a trigger to an action, or unbinds one. Multiple `keybind` lines accumulate; later lines override earlier ones for the same trigger (last-wins per trigger).

### Modifiers

Combine with `+`. Aliases are accepted on both sides:

| Canonical | Aliases             |
|-----------|---------------------|
| `shift`   |                     |
| `ctrl`    | `control`           |
| `alt`     | `opt`, `option`     |
| `super`   | `cmd`, `command`    |

The key segment (last token) passes through to GTK's keyval lookup unchanged.

### Examples

```conf
# Add Cmd-J as a second trigger for new_tab. Cmd-T (the default) still works.
keybind = super+j = new_tab

# Disable the default rename-project shortcut.
keybind = super+shift+r = unbind

# Reassign Cmd-T to close the active tab. Cmd-W still also closes (default).
keybind = super+t = close_tab
```

Use only leading-line `#` comments. A `#` after a `keybind` value is treated as part of the action string, not as an inline comment.

### Available actions

| Action                | Default (macOS / Linux)                                |
|-----------------------|--------------------------------------------------------|
| `new_tab`             | `super+t` / `ctrl+t`                                   |
| `close_tab`           | `super+w` / `ctrl+w`                                   |
| `rename_tab`          | `super+r` / `alt+r`                                    |
| `cycle_tab_prev`      | `super+shift+bracketleft` / `ctrl+shift+bracketleft`   |
| `cycle_tab_next`      | `super+shift+bracketright` / `ctrl+shift+bracketright` |
| `paste`               | `super+v`, `ctrl+shift+v` / `alt+v`, `ctrl+shift+v`    |
| `copy`                | `super+c`, `ctrl+shift+c` / `alt+c`, `ctrl+shift+c`    |
| `new_project`         | `super+n` / `alt+n`                                    |
| `rename_project`      | `super+shift+r` / `alt+shift+r`                        |
| `close_project`       | `super+shift+w` / `alt+shift+w`                        |
| `toggle_sidebar`      | `super+b` / `alt+b`                                    |
| `jump_to_unread`      | `super+shift+u` / `ctrl+shift+u`                       |
| `command_palette`     | `super+shift+p` / `alt+shift+p`                        |
| `command_launcher`    | `super+shift+t` / `alt+shift+t`                        |
| `switch_project_1..9` | `super+1..9` / `alt+1..9`                              |
| `switch_tab_1..9`     | `ctrl+1..9` / `ctrl+1..9`                              |
| `font_increase`       | `super+plus`, `super+equal` / `ctrl+plus`, `ctrl+equal` |
| `font_decrease`       | `super+minus` / `ctrl+minus`                           |
| `font_reset`          | `super+0` / `ctrl+0`                                   |

Defaults with multiple triggers (`cycle_tab_*`, `paste`, `copy`) keep both triggers; an `unbind` line removes only the listed one.

Triggers using Ghostty prefixes (`global:`, `all:`, `unconsumed:`, `performable:`) and unknown action names are logged and skipped — they're out of scope for v1.

The config file is read once at startup; restart Roost to pick up edits.
