# First Run

The first launch of Roost creates its data directory, opens a single window, and gives you one project with one tab. Subsequent launches restore that state.

## Window layout

Top to bottom, left to right:

| Area | Contents |
|---|---|
| Title bar | Window title (active tab's cwd) |
| Sidebar (left) | List of projects, with a **+ New Project** button at the bottom |
| Tab strip (right) | Tabs for the currently selected project, with a trailing **+** button |
| Terminal surface | One libghostty-vt terminal per tab (Core Graphics on macOS, iced + wgpu on Linux) |

Click a project in the sidebar to switch its tab strip into the right pane. Click a tab to swap the terminal surface to that tab's session.

With no [host sessions](../guides/host-sessions.md) saved, the sidebar looks exactly like this — one "PROJECTS" header, no dots. The first saved host splits it into per-host sections instead: a "LOCAL" band for the projects you see above, then one band per saved host, each with a small connection dot and a right-aligned agent count. This is opt-in. Hosting a session on *this* machine is Linux-only for now; a Mac can still connect to a remote Linux session over an SSH-forwarded socket, and its host sections look the same (see the guide's macOS note).

## Default state

On first launch Roost creates a project named `Untitled 1` with one tab. The tab's working directory is your home directory and the shell is whatever `$SHELL` is set to (falling back to `/bin/sh`).

## Roost-Iced: a fresh install starts on a session

**Roost-Iced only**, on both platforms (Linux's `roost` and macOS's
`Roost-Iced.app`) — the Swift `Roost.app` never reads this key and always
runs in-process (see below).

One config key decides where local tabs run. If `config.conf` — a
single file shared by every profile, not a per-profile one — already has
a `local-backend` key, that value wins, however it got there.

With no such key, and no `state.json` or `config.conf` on disk at all,
the launch is a genuinely fresh install and local tabs start on a
`localhost` **session**: they live in a small headless `roost-session`
daemon rather than inside the app, the way an
[added host](../guides/host-sessions.md) does, under an implicit
**LOCALHOST** band in the sidebar. The practical effect is that closing
the window doesn't end your shells — reopen Roost and they're still
there. That launch then tries to write `local-backend = session` so
later launches don't decide again; if the write fails, it runs
in-process instead.

Anything else — no key, but a `state.json` or `config.conf` already
there — keeps in-process local tabs exactly as before. The default fires
only on a launch with nothing yet on disk.

Either way, you can switch anytime from the command palette:
**Use a session for local tabs** moves your in-process layout onto the
session, and **Use in-process local tabs** flips back without copying
anything back (your session-side work stays put, one click away under
**LOCALHOST**). Only one of the two rows shows at a time. See [Host
Sessions → Switching the local backend](../guides/host-sessions.md#switching-the-local-backend)
for the full mechanics, including what a mid-switch failure does.

The Swift `Roost.app` is unaffected by all of this: it always runs
local tabs in-process, never reads `local-backend`, and has no session
backend at all.

## Agent hooks: the first-launch consent card

The first time either UI starts with no answer on file yet and at least
one supported coding agent (Claude Code, Codex, grok/gx, cursor-agent,
OpenCode) installed on the machine, a consent dialog opens — the iced
**Agent Hooks…** card, the Mac **Agent Hooks…** sheet — naming what it
found and letting you choose which agents Roost wires notifications for,
or none. Nothing is written into any agent's config file until you
answer it, and it reopens on demand from the command palette (or, on
Mac, the View menu). See [Agent Hooks → Roost asks
once](../guides/agents.md#roost-asks-once) for the full behavior,
including what each `agent-hooks` value (a list, `off`, or absent)
means and how a startup that leaves an agent unwired is surfaced.

## Persistence

Every project, tab, working directory, and tab title is persisted to a small `state.json` file written atomically by the UI. When you relaunch Roost, all of those come back. Relaunching an in-process tab spawns a fresh shell at its saved working directory; a tab on a still-running session instead reconnects to the shell that's already there. Either way, Roost never re-runs your last command on its own.

| State                | Persisted? | Notes                                                  |
|----------------------|------------|--------------------------------------------------------|
| Project name + cwd   | Yes        | Sidebar order is preserved                             |
| Tab order, cwd       | Yes        | Tab strip order is preserved per project               |
| Tab title (OSC 0/1/2)| Yes        | Updated live as the shell sets it; locked once you rename via `Cmd-R` / `Alt-R` or `roostctl set-title` |
| Scrollback           | No         | Lost on shell exit; surface restart is fresh           |
| Last command         | No (not auto-restarted) | Use shell history (`up arrow`) to re-run    |

## Where state lives

The user-editable config file lives under XDG on both platforms (`~/.config/roost/config.conf`); state files (`state.json` and the IPC socket) follow each platform's native convention:

- macOS: `~/.config/roost/config.conf` (config), `~/Library/Application Support/Roost/state.json` (data), and `~/Library/Caches/Roost/roost.sock` (socket)
- Linux: `~/.config/roost/config.conf` (config), `~/.local/share/roost/state.json` (data), and `$XDG_RUNTIME_DIR/roost/roost.sock` (socket)

See [Paths & Environment](../reference/paths.md) for the full layout.

## What you should see

- The default tab title shows the working directory if the shell hasn't set its own title yet (a one-line shell snippet makes this follow `cd` — see [Working Directory Tracking](../guides/cwd-tracking.md))
- Resizing the window reflows the terminal — `vim` and `htop` adjust correctly
- Output renders with full 24-bit color and basic styles (bold, italic, inverse)
- Two-finger scroll (or wheel) navigates the scrollback; pressing any input-producing key snaps the viewport back to the bottom
- Click + drag selects cells with a translucent accent overlay; selection clears on PTY output, on resize, and on the next click. See [Keybindings → Mouse](keybindings.md#mouse) for the full table including pass-through to `vim` / `htop` / `tmux` and the Shift-bypass convention.
- `Cmd-V` (macOS) / `Alt-V` (Linux) / `Ctrl-Shift-V` (both) pastes the system clipboard. Multi-line pastes into modern shells are wrapped in bracketed-paste sequences so they don't auto-execute. Pastes are sanitized (NUL/ESC/DEL stripped) and capped at 4 MiB.

## Common first-launch behaviors

- macOS Notification Center may prompt for permission the first time `roost` (or `osascript`, the macOS notification fallback) tries to surface a notification.
- On Linux the UI logs to `$XDG_STATE_HOME/roost/roost.log` (default `~/.local/state/roost/roost.log`) **and** tees the same lines to stdout, so launching from a shell shows startup warnings (a missing font family, for instance) directly. `RUST_LOG=debug` turns the volume up. On macOS the Swift app logs to `~/Library/Logs/Roost/roost.log`.

## Next

- [Keybindings](keybindings.md) — how to drive Roost without the mouse
- [Working Directory Tracking](../guides/cwd-tracking.md) — make the header subtitle and tab labels follow `cd`
- [Notifications](../guides/notifications.md) — how `roostctl` and OSC sequences surface in the UI
