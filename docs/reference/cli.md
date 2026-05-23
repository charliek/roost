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
| `identify` | Print daemon identity (socket, PID, active tab, version) |
| `tab focus` | Focus a tab (raises window, switches project, selects tab) |
| `tab list` | List every tab grouped by project |
| `tab set-state` | Set the per-tab agent state |
| `tab clear-notification` | Clear a tab's pending-attention flag |
| `tab open` / `close` / `send` / `resize` / `reorder` | Tab lifecycle + I/O |
| `project list` / `create` / `rename` / `delete` / `reorder` | Project lifecycle |
| `claude install` | Generate Claude Code hook settings + print the alias snippet |
| `claude-hook` | Internal: invoked by Claude on each hook event |

`--socket` overrides `ROOST_SOCKET`; one of the two must resolve to a reachable daemon socket.

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
  "socket": "/Users/charliek/Library/Caches/roost/roost.sock",
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

## `tab open` / `close` / `send` / `resize` / `reorder`

Tab lifecycle and I/O for automation. `tab send` needs an existing live PTY (a UI must have already attached); errors with `NotFound` otherwise. `--bytes` accepts Rust string-escape sequences (`\n`, `\r`, `\x1b`, …); pass `--raw` to disable escape decoding.

```bash
roostctl tab open --project-id 1 --cwd ~/projects/roost
roostctl tab close --tab 5
roostctl tab send --tab 5 --bytes 'ls -la\n'
roostctl tab resize --tab 5 --cols 120 --rows 40
roostctl tab reorder --project-id 1 --order 3,5,7
```

## `project` subcommands

```bash
roostctl project list
roostctl project create --name "scratch" --cwd ~
roostctl project rename --project-id 1 --name "main"
roostctl project delete --project-id 2
roostctl project reorder --order 1,3,2
```

`project delete` cascades to the project's tabs. `project reorder` is the same shape as `tab reorder` — any id not in `--order` keeps its prior position.

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
| `ROOST_SOCKET` | Override the daemon socket the CLI dials |
| `ROOST_TAB_ID` | Default tab id when `--tab` is not given |
| `ROOST_PROJECT_ID` | The project id this tab lives in (auto-set, available to scripts) |
| `ROOST_DEBUG` | If set, `claude-hook` writes failure messages to stderr |

The first three are auto-set by the UI when it spawns a tab's shell. Set them by hand only when invoking the CLI from outside a Roost tab (e.g. a CI runner).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | RPC error or connection failure |
| 2 | Bad command-line input |
