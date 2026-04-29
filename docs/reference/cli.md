# `roost-cli`

Companion CLI for the running Roost GUI. Talks to the GUI over a Unix socket using newline-delimited JSON-RPC. Intended to be invoked from inside a Roost tab (typically by Claude Code hooks), but works from any shell that can reach the socket.

## Usage

```text
roost-cli <command> [flags]
```

| Command           | Purpose                                                         |
|-------------------|-----------------------------------------------------------------|
| `notify`          | Fire a notification on a tab                                    |
| `set-title`       | Set a tab's title from the CLI                                  |
| `identify`        | Print info about the running GUI (socket path, active tab, PID) |
| `tab focus`       | Focus a tab (raises window, switches project, selects tab)      |
| `tab list`        | List every tab grouped by project                               |
| `tab set-state`   | Set the per-tab agent state (running / needs-input / idle / none) |
| `claude install`  | Generate a Claude Code settings file and print a bash alias     |
| `claude-hook`     | Bridge Claude Code hook events into Roost (reads JSON on stdin) |
| `help`            | Print usage                                                     |

## `notify`

Fire a notification. The target tab is resolved from `--tab` first, then `$ROOST_TAB_ID`.

```bash
roost-cli notify --title "Build done" --body "tests pass"
roost-cli notify --tab 3 --title "From CI" --body "deploy ready"
```

| Flag      | Type   | Default            | Description                                              |
|-----------|--------|--------------------|----------------------------------------------------------|
| `--title` | string | required           | Notification title                                       |
| `--body`  | string | empty              | Notification body                                        |
| `--tab`   | int    | `$ROOST_TAB_ID`    | Target tab id; required if env var is unset              |

## `set-title`

Set a tab's display title. Persists across restarts and locks the tab against subsequent OSC 1/2 escapes from the shell — once set via `set-title` (or via `Cmd-R` / `Alt-R` in the GUI), prompt-frame title-rewrites stop overwriting it. v1 has no in-app way to clear the lock; the workaround is to delete and recreate the tab.

```bash
roost-cli set-title --title "build-watcher"
roost-cli set-title --title "deploy" --tab 3
```

| Flag      | Type   | Default            | Description                                              |
|-----------|--------|--------------------|----------------------------------------------------------|
| `--title` | string | required           | New tab title (also accepted as a positional argument)   |
| `--tab`   | int    | `$ROOST_TAB_ID`    | Target tab id; required if env var is unset              |

## `identify`

Print metadata about the running app:

```bash
roost-cli identify
```

```json
{
  "active_project_id": 1,
  "active_tab_id": 5,
  "pid": 14138,
  "socket": "/Users/charliek/Library/Application Support/Roost/roost.sock"
}
```

Useful for verifying the socket is reachable and the env vars are wired correctly.

## `tab focus`

Switch the GUI to a specific tab — raises the window, switches the active project (if needed), and selects the tab. Used as the click-through target for desktop banners.

```bash
roost-cli tab focus               # focus the calling shell's tab
roost-cli tab focus --tab 7       # focus tab 7
```

Returns the previously focused tab in the response so callers can implement "focus back" without state.

## `tab list`

List every tab grouped by project, in display order. Default output is a human-readable tree; pass `--json` for the raw IPC response (a `TabListResult`).

```bash
roost-cli tab list
roost-cli tab list --json
```

Each tab carries `id`, `title`, `agent_state`, `has_notification`, and `is_active` flags.

## `tab set-state`

Drive the sticky per-tab agent-state indicator. Useful for non-Claude agents and shell scripts.

```bash
roost-cli tab set-state --state running
roost-cli tab set-state --tab 3 --state idle
```

| Flag      | Type   | Default            | Description                                                          |
|-----------|--------|--------------------|----------------------------------------------------------------------|
| `--state` | string | required           | One of `none`, `running`, `needs_input`, `idle`. Anything else returns `bad_request`. |
| `--tab`   | int    | `$ROOST_TAB_ID`    | Target tab id                                                        |

## `claude install`

Generate `~/.config/roost/claude-settings.json` and print a bash alias snippet to stdout. See the [Claude Code Hooks](../guides/claude-code.md) guide for the full workflow.

```bash
roost-cli claude install >> ~/.bashrc
roost-cli claude install --force   # overwrite an existing file
```

## `claude-hook`

Internal: invoked by Claude Code via the generated settings file. Reads the hook payload from stdin, looks up `$ROOST_TAB_ID`, and translates lifecycle events into IPC calls.

```text
roost-cli claude-hook session-start | prompt-submit | notification | stop | session-end
```

Always exits 0 with `{}` on stdout (Claude treats nonzero hooks as failures). Silently no-ops when run outside a Roost tab.

## Environment

| Variable           | Effect                                                              |
|--------------------|---------------------------------------------------------------------|
| `ROOST_SOCKET`     | Override the socket path the CLI dials into                          |
| `ROOST_TAB_ID`     | Default tab id when `--tab` is not given                             |
| `ROOST_PROJECT_ID` | The project id this tab lives in (auto-set, available to scripts)   |
| `ROOST_DEBUG`      | If set, `claude-hook` writes failure messages to stderr               |

The first three are auto-set by the GUI when it spawns a tab's shell. You only need to set them by hand if you're invoking the CLI from outside Roost (e.g. from a CI runner).

## Exit codes

| Code | Meaning                                              |
|------|------------------------------------------------------|
| 0    | Success                                              |
| 1    | RPC error or connection failure                      |
| 2    | Bad command-line input (missing `--title`, etc.)     |

## Wire format

The CLI sends one JSON request and reads one JSON response. The server enforces a 1-line-per-request framing on the wire:

```json
{"id":"1","method":"notification.create","params":{"tab_id":5,"title":"hi","body":""}}
{"id":"1","ok":true,"result":{"delivered":true}}
```

Method names match `internal/ipc` constants:

| Method                       | Purpose                                  |
|------------------------------|------------------------------------------|
| `notification.create`        | Used by `notify` and `claude-hook`       |
| `tab.set_title`              | Used by `set-title`                      |
| `tab.focus`                  | Used by `tab focus`                      |
| `tab.list`                   | Used by `tab list`                       |
| `tab.set_state`              | Used by `tab set-state` and `claude-hook` |
| `tab.clear_notification`     | Used by `claude-hook prompt-submit`      |
| `system.identify`            | Used by `identify`                       |
| `system.set_hook_active`     | Used by `claude-hook session-start/end`  |

If you need to drive Roost from a language other than Go, this protocol is small enough to implement against directly — open the socket, encode JSON, write `\n`-terminated.
