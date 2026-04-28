# First Run

The first launch of Roost creates its data directory, opens a single window, and gives you one project with one tab. Subsequent launches restore that state.

## Window layout

Top to bottom, left to right:

| Area              | Contents                                                          |
|-------------------|-------------------------------------------------------------------|
| Header bar        | Window title and the **+ tab** button                              |
| Sidebar (left)    | List of projects, with a **+ Project** button at the bottom        |
| Tab bar (right)   | Tabs for the currently selected project                            |
| Terminal surface  | One libghostty-vt terminal hosted on a `GtkDrawingArea`            |

Click a project in the sidebar to switch its tab strip into the right pane. Click a tab to swap the terminal surface to that tab's session.

## Default state

On first launch Roost creates a project named `default` with one tab. The tab's working directory is your home directory and the shell is whatever `$SHELL` is set to (falling back to `/bin/sh`).

## Persistence

Every project, tab, working directory, and tab title is persisted to a SQLite database. When you relaunch Roost, all of those come back. Each tab spawns a fresh shell at its saved working directory — Roost never re-runs your last command on its own.

| State                | Persisted? | Notes                                                  |
|----------------------|------------|--------------------------------------------------------|
| Project name + cwd   | Yes        | Sidebar order is preserved                             |
| Tab order, cwd       | Yes        | Tab strip order is preserved per project               |
| Tab title (OSC 0/1/2)| Yes        | Updated live as the shell sets it; locked once you rename via `Cmd-R` / `Alt-R` or `roost-cli set-title` |
| Scrollback           | No         | Lost on shell exit; surface restart is fresh           |
| Last command         | No (not auto-restarted) | Use shell history (`up arrow`) to re-run    |

## Where state lives

The database, socket, and (later) configuration files live under a platform path:

- macOS: `~/Library/Application Support/Roost/`
- Linux: `~/.local/share/roost/` (data) and `~/.config/roost/` (config); the socket lives under `$XDG_RUNTIME_DIR/roost/`

See [Paths & Environment](../reference/paths.md) for the full layout.

## What you should see

- The default tab title shows the working directory if the shell hasn't set its own title yet
- Resizing the window reflows the terminal — `vim` and `htop` adjust correctly
- Output renders with full 24-bit color and basic styles (bold, italic, inverse)
- Two-finger scroll (or wheel) navigates the scrollback; pressing any input-producing key snaps the viewport back to the bottom
- Click + drag selects cells with a translucent accent overlay; selection clears on PTY output, on resize, and on the next click. See [Keybindings → Mouse](keybindings.md#mouse) for the full table including pass-through to `vim` / `htop` / `tmux` and the Shift-bypass convention.
- `Cmd-V` (macOS) / `Alt-V` (Linux) / `Ctrl-Shift-V` (both) pastes the system clipboard. Multi-line pastes into modern shells are wrapped in bracketed-paste sequences so they don't auto-execute. Pastes are sanitized (NUL/ESC/DEL stripped) and capped at 4 MiB.

## Common first-launch behaviors

- A few harmless GLib warnings on stderr (`g_settings_schema_source_lookup` etc.) on macOS — they come from libraries Roost links against and don't affect anything. Roost filters out the GTK theme-parser noise on its own; the rest is silent on a real desktop session.
- macOS Notification Center may prompt for permission the first time `roost` (or `osascript`, the macOS notification fallback) tries to surface a notification.

## Next

- [Keybindings](keybindings.md) — how to drive Roost without the mouse
- [Notifications](../guides/notifications.md) — how `roost-cli` and OSC sequences surface in the UI
