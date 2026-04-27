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

## Mouse

The sidebar still supports mouse-driven rename: double-click a project row to rename it inline, or right-click for a Rename / Close menu.

If you close the last tab in a project, Roost closes that project too. The "Are you sure?" confirmation dialog only appears for explicit close-project actions (the sidebar X button or the right-click menu); `Cmd-W` / `Ctrl-W` on the final tab closes the project silently.

Tab titles set via `Cmd-R` / `Alt-R` are persisted, but a subsequent OSC 1/2 escape from the shell (`\e]2;new-title\a`, common in shell prompts) will overwrite the manual rename. Locking against OSC overwrites is a planned follow-up.

## Terminal keys

Anything not bound as an app shortcut flows to the focused terminal:

| Key                    | Effect                                                            |
|------------------------|-------------------------------------------------------------------|
| Printable characters   | Sent to the shell as typed                                        |
| Enter, Backspace, Tab  | Sent to the shell                                                 |
| Arrow keys, Home, End  | Sent as standard CSI sequences                                    |
| Page Up / Page Down    | Sent as standard CSI sequences                                    |
| Esc                    | Sent to the shell                                                 |
| `Ctrl-letter`          | Sent as the corresponding control byte (`Ctrl-C` → SIGINT, etc.)  |

`Ctrl-1` … `Ctrl-9` are reserved for tab switching and do not reach the shell. On macOS, all `Cmd-*` and `Super-*` combinations are reserved for app shortcuts and never reach the shell.

## How the shortcut controller is wired

Shortcuts run in GTK's *capture* phase, which means they fire before the focused widget — including the terminal surface — sees the event. That's why `Cmd-T` (or `Ctrl-T` on Linux) works while the terminal is focused: the window's controller catches it first and dispatches the action, and the keystroke never reaches the shell. Anything not bound at the window level falls through to the terminal as usual.

## Mouse forwarding

Mouse forwarding to the shell is not implemented yet. Selection, copy, and scroll-wheel scrollback are deferred. Click on the terminal area to give it focus.
