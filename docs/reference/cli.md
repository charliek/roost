# `roostctl`

Shell-integration CLI for the running Roost UI. Talks JSON over
a Unix-domain socket directly to the UI process — no daemon.
Intended to be invoked from inside a Roost tab (typically by
Claude Code hooks) but works from any shell that can reach the
socket. See [`docs/reference/ipc.md`](ipc.md) for the wire
format.

Crate: `crates/roost-cli` (binary `roostctl`). For the legacy
Go CLI that ships from `main`, see
[Legacy → CLI](legacy-go/cli.md).

## Usage

```text
roostctl [--socket <PATH>] <COMMAND>
```

| Command | Purpose |
|---|---|
| `notify` | Fire a notification on a tab |
| `set-title` | Rename a tab (locks it from OSC overwrites) |
| `identify` | Print the running UI's identity (socket, PID, active tab, version) |
| `tab focus` | Focus a tab (raises window, switches project, selects tab) |
| `tab list` | List every tab grouped by project |
| `tab set-state` | Set the per-tab agent state |
| `tab clear-notification` | Clear a tab's pending-attention flag |
| `tab open` / `close` / `send` / `resize` / `reorder` | Tab lifecycle + I/O |
| `project list` / `create` / `rename` / `delete` / `reorder` | Project lifecycle |
| `claude install` | Generate Claude Code hook settings + print the alias snippet |
| `claude-hook` | Internal: invoked by Claude on each hook event |

`--socket` overrides `ROOST_SOCKET`; one of the two must resolve to the running UI's socket.

## `notify`

```bash
roostctl notify --title "Build done" --body "tests pass"
roostctl notify --tab 3 --title "From CI" --body "deploy ready"
```

| Flag | Type | Default | Description |
|---|---|---|---|
| `--title` | string | required | Notification title |
| `--body` | string | empty | Notification body |
| `--tab` | int | `$ROOST_TAB_ID` | Target tab id; required if env var is unset |

## `set-title`

Set a tab's display title. Persists across restarts and locks the tab against subsequent OSC 1/2 escapes from the shell.

```bash
roostctl set-title --title "build-watcher"
roostctl set-title --title "deploy" --tab 3
```

| Flag | Type | Default | Description |
|---|---|---|---|
| `--title` | string | required | New tab title |
| `--tab` | int | `$ROOST_TAB_ID` | Target tab id |

## `identify`

```bash
roostctl identify
```

```json
{
  "socket": "/Users/charliek/Library/Caches/Roost/roost.sock",
  "pid": 14138,
  "version": "0.1.0",
  "active_project_id": 1,
  "active_tab_id": 5
}
```

Useful for verifying the socket is reachable and the env vars are wired correctly.

## `tab focus`

```bash
roostctl tab focus               # focus the calling shell's tab
roostctl tab focus --tab 7
```

Raises the window, switches the active project, selects the tab. Used as the click-through target for desktop banners.

## `tab list`

```bash
roostctl tab list
roostctl tab list --json
```

Default output is a human-readable tree; `--json` prints the raw response. Each tab carries `id`, `title`, `agent_state`, `has_notification`, and `is_active`.

## `tab set-state`

```bash
roostctl tab set-state --state running
roostctl tab set-state --tab 3 --state idle
```

| Flag | Type | Default | Description |
|---|---|---|---|
| `--state` | string | required | One of `none`, `running`, `needs_input`, `idle` |
| `--tab` | int | `$ROOST_TAB_ID` | Target tab id |

## `tab open` / `close` / `send` / `resize` / `reorder` / `dump`

Tab lifecycle and I/O for automation. `tab send` needs an existing live PTY (a UI must have already attached); errors with `NotFound` otherwise. `--bytes` accepts Rust string-escape sequences (`\n`, `\r`, `\x1b`, …); pass `--raw` to disable escape decoding.

```bash
roostctl tab open --project-id 1 --cwd ~/projects/roost
roostctl tab close --tab 5
roostctl tab send --tab 5 --bytes 'ls -la\n'
roostctl tab resize --tab 5 --cols 120 --rows 40
roostctl tab reorder --project-id 1 --order 3,5,7
roostctl tab dump --tab 5          # the visible viewport as text
roostctl tab dump --tab 5 --json   # full result: dims + cursor + rows
```

`tab dump` reads the tab's live terminal viewport as text — the determinism backbone for tests: assert on exact content instead of matching pixels. Plain output is one line per visible row (trailing blanks trimmed); `--json` adds dimensions and cursor. Backed by the `tab.dump` IPC op — see [ipc.md](ipc.md).

## `wait`

Block until a tab reaches a condition, then exit `0` — the no-`sleep` synchronization primitive for scripts and tests. Polls the running UI on an interval; exits non-zero if `--timeout` elapses first. At least one condition is required; when several are given, all must hold.

```bash
roostctl wait --tab 5 --state idle            # until the agent state is idle
roostctl wait --tab 5 --text 'BUILD OK'       # until the viewport contains a string
roostctl wait --tab 5 --gone                  # until the tab is closed
```

| Flag | Type | Default | Description |
|---|---|---|---|
| `--state` | string | — | Wait until the tab's agent state equals this (`none`/`running`/`needs_input`/`idle`) |
| `--text` | string | — | Wait until the viewport (via `tab.dump`) contains this substring. Pick a needle from command *output*, not the echoed command |
| `--gone` | flag | `false` | Wait until the tab no longer exists |
| `--timeout` | float | `5.0` | Give up after this many seconds |
| `--interval-ms` | int | `100` | Poll interval |
| `--tab` | int | `$ROOST_TAB_ID` | Target tab id |

## `project` subcommands

```bash
roostctl project list
roostctl project create --name "scratch" --cwd ~
roostctl project rename --project-id 1 --name "main"
roostctl project delete --project-id 2
roostctl project reorder --order 1,3,2
```

`project delete` cascades to the project's tabs. `project reorder` is the same shape as `tab reorder` — any id not in `--order` keeps its prior position.

## `screenshot`

Capture a PNG of the running UI's whole window (sidebar + tab bar + active terminal), rendered **in-process** by the UI itself. Because it re-draws the view tree rather than grabbing screen pixels, it needs no screen-recording permission and works even when the window is unfocused, behind other windows, or offscreen — handy for confirming a UI change without OS screen capture.

```bash
roostctl screenshot --out shot.png        # write a file
roostctl screenshot --scale 2 --out shot.png   # 2x super-sampled
roostctl screenshot > shot.png            # raw PNG bytes to stdout
```

`--scale` is `1` (default, logical window size) or `2`. With `--out` the CLI writes the file and prints the dimensions + byte count to stderr; without it, the raw PNG bytes go to stdout (nothing else is printed, so the stream stays binary-clean). Backed by the `app.screenshot` IPC op — see [ipc.md](ipc.md).

## `palette` subcommands

Drive the command-palette overlay: open it, inspect its rows, filter, activate a row, dismiss. Activating a row runs the **same** command its keybind would (a command row's id is its keybind action), so this is a command-dispatch surface, not just a UI poke. Each subcommand prints the resulting palette state (a `>` marks the highlighted row); `--json` emits the structured result.

```bash
roostctl palette open                      # the command palette (or: --kind launcher)
roostctl palette state                     # current rows / filter / selection
roostctl palette query theme               # set the filter
roostctl palette activate new_tab          # confirm the row (runs its command)
roostctl palette dismiss
```

`palette activate <id>` errors `not-found` if no palette is open or no visible row has that id. Backed by the `palette.*` IPC ops — see [ipc.md](ipc.md).

## `claude install`

Writes `~/.config/roost/claude-settings.json` pointing at this binary's `claude-hook` subcommand for each Claude Code lifecycle event, then prints a bash alias snippet (`alias claude='claude --settings ...'`) to stdout. See the [Claude Code Hooks](../guides/claude-code.md) guide for the full workflow.

```bash
roostctl claude install >> ~/.bashrc
roostctl claude install --force   # overwrite an existing file
```

## `claude-hook`

Internal: invoked by Claude Code via the generated settings file. Reads the hook payload from stdin, looks up `$ROOST_TAB_ID`, and translates lifecycle events into IPC calls. Always exits 0 with `{}` on stdout (Claude treats nonzero hooks as failures). Silently no-ops when run outside a Roost tab.

## Environment

| Variable | Effect |
|---|---|
| `ROOST_SOCKET` | Override the UI socket the CLI dials |
| `ROOST_TAB_ID` | Default tab id when `--tab` is not given |
| `ROOST_PROJECT_ID` | The project id this tab lives in (auto-set, available to scripts) |
| `ROOST_DEBUG` | If set, `claude-hook` writes failure messages to stderr |

`ROOST_SOCKET` / `ROOST_TAB_ID` / `ROOST_PROJECT_ID` are auto-set by the UI when it spawns a tab's shell. Set them by hand only when invoking the CLI from outside a Roost tab (e.g. a CI runner). The UI side also honors `ROOST_CONFIG` (config path) and `ROOST_BUNDLE_PROFILE` (`mac`/`gtk`) — see [Paths & Environment](paths.md).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | RPC error or connection failure |
| 2 | Bad command-line input |
