# Keybindings

All Roost shortcuts use Control (Mac and Linux). Cmd is also bound on macOS via GTK's `<primary>` alias, but its delivery through GTK on macOS is unreliable, so Ctrl is the canonical modifier.

## Tab management

| Shortcut          | Action                                                |
|-------------------|-------------------------------------------------------|
| `Ctrl-T`          | New tab in the active project                         |
| `Ctrl-W`          | Close the active tab                                  |
| `Ctrl-Shift-]`    | Cycle to the next tab in the active project           |
| `Ctrl-Shift-[`    | Cycle to the previous tab in the active project       |

If you close the last tab in a project, Roost auto-creates a fresh tab so you never end up with a project that has zero tabs.

## Project switching

| Shortcut          | Action                                                |
|-------------------|-------------------------------------------------------|
| `Ctrl-1` … `Ctrl-9` | Switch to the project at sidebar position 1 .. 9    |
| `Ctrl-Shift-T`    | Create a new project (auto-named `untitled`, `untitled 2`, …) |

The active project's tab strip occupies the right pane. Switching projects swaps the tab strip and shows that project's last-selected tab.

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
| `Cmd-letter` / `Super` | **Not** sent to the shell; reserved for app shortcuts             |

## How the shortcut controller is wired

Shortcuts run in GTK's *capture* phase, which means they fire before the focused widget — including the terminal surface — sees the event. That's why `Ctrl-T` works while the terminal is focused: the window's controller catches it first and dispatches the action, and the byte never reaches the shell. Anything not bound at the window level (everything except the shortcuts above) falls through to the terminal as usual.

## Mouse

Mouse forwarding to the shell is not implemented yet. Selection, copy, and scroll-wheel scrollback are deferred. Click on the terminal area to give it focus.
