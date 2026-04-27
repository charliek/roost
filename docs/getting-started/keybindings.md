# Keybindings

Roost uses platform-native modifiers: **Cmd** on macOS, **Ctrl** plus **Alt** on Linux. The same actions are available on both platforms — only the modifier differs.

## macOS

### Tab management (active project)

| Shortcut         | Action                                          |
|------------------|-------------------------------------------------|
| `Cmd-T`          | New tab                                         |
| `Cmd-W`          | Close the active tab                            |
| `Cmd-R`          | Rename the active tab                           |
| `Cmd-Shift-]`    | Cycle to the next tab                           |
| `Cmd-Shift-[`    | Cycle to the previous tab                       |
| `Ctrl-1` … `Ctrl-9` | Switch to tab at position 1 .. 9             |

### Project management

| Shortcut             | Action                                       |
|----------------------|----------------------------------------------|
| `Cmd-N`              | Create a new project (`untitled`, `untitled 2`, …) |
| `Cmd-Shift-R`        | Rename the active project                   |
| `Cmd-1` … `Cmd-9`    | Switch to the project at sidebar position 1 .. 9 |

### Clipboard

| Shortcut         | Action                                              |
|------------------|-----------------------------------------------------|
| `Cmd-C`          | Copy the current terminal selection                 |
| `Cmd-V`          | Paste the system clipboard into the active terminal |
| `Ctrl-Shift-C`   | Same as `Cmd-C` (terminal-convention alternate)     |
| `Ctrl-Shift-V`   | Same as `Cmd-V` (terminal-convention alternate)     |

Bare `Ctrl-C` is left as SIGINT — it's not overloaded for copy.

## Linux

### Tab management (active project)

| Shortcut         | Action                                          |
|------------------|-------------------------------------------------|
| `Ctrl-T`         | New tab                                         |
| `Ctrl-W`         | Close the active tab                            |
| `Alt-R`          | Rename the active tab                           |
| `Ctrl-Shift-]`   | Cycle to the next tab                           |
| `Ctrl-Shift-[`   | Cycle to the previous tab                       |
| `Ctrl-1` … `Ctrl-9` | Switch to tab at position 1 .. 9             |

### Project management

| Shortcut             | Action                                       |
|----------------------|----------------------------------------------|
| `Alt-N`              | Create a new project (`untitled`, `untitled 2`, …) |
| `Alt-Shift-R`        | Rename the active project                   |
| `Alt-1` … `Alt-9`    | Switch to the project at sidebar position 1 .. 9 |

### Clipboard

| Shortcut         | Action                                              |
|------------------|-----------------------------------------------------|
| `Alt-C`          | Copy the current terminal selection                 |
| `Alt-V`          | Paste the system clipboard into the active terminal |
| `Ctrl-Shift-C`   | Same as `Alt-C` (terminal-convention alternate)     |
| `Ctrl-Shift-V`   | Same as `Alt-V` (terminal-convention alternate)     |

Bare `Ctrl-C` is left as SIGINT — it's not overloaded for copy. Copying also writes to the X11/Wayland PRIMARY clipboard so middle-click paste in other apps works.

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

Selection clears automatically on PTY output that touches the selected rows, on resize, and on a new click.

Pressing any input-producing key when the viewport is scrolled back snaps the viewport to the bottom before delivering the keystroke — same behavior as every other terminal multiplexer.

## Sidebar mouse

The sidebar still supports mouse-driven rename: double-click a project row to rename it inline, or right-click for a Rename / Close menu.

If you close the last tab in a project, Roost closes that project too. The "Are you sure?" confirmation dialog only appears for explicit close-project actions (the sidebar X button or the right-click menu); `Cmd-W` / `Ctrl-W` on the final tab closes the project silently.

Tab titles set via `Cmd-R` / `Alt-R` are persisted, but a subsequent OSC 1/2 escape from the shell (`\e]2;new-title\a`, common in shell prompts) will overwrite the manual rename. Locking against OSC overwrites is a planned follow-up.
