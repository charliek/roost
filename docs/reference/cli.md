# `roost-cli`

Companion CLI for the running Roost GUI. Talks to the GUI over a Unix socket using newline-delimited JSON-RPC. Intended to be invoked from inside a Roost tab (typically by Claude Code hooks), but works from any shell that can reach the socket.

Built on [cobra](https://github.com/spf13/cobra). Run `roost-cli --help` for an auto-generated overview, `roost-cli <command> --help` for per-command flags, and `roost-cli completion bash|zsh|fish|powershell` for shell completions.

## Usage

```text
roost-cli <command> [flags]
```

| Command            | Purpose                                                            |
|--------------------|--------------------------------------------------------------------|
| `notify`           | Fire a notification on a tab                                       |
| `identify`         | Print info about the running GUI (socket path, active tab, PID)    |
| `tab list`         | List every tab grouped by project                                  |
| `tab focus`        | Focus a tab (raises window, switches project, selects tab)         |
| `tab set-title`    | Set a tab's title                                                  |
| `tab set-state`    | Set the per-tab agent state (running / needs_input / idle / none)  |
| `claude install`   | Generate a Claude Code settings file and print a bash alias        |
| `claude hook`      | Bridge Claude Code hook events into Roost (reads JSON on stdin)    |
| `version`          | Print the CLI version                                              |
| `completion`       | Generate shell completion scripts                                  |
| `help`             | Print usage                                                        |

## Persistent flags

These apply to every command:

| Flag        | Type     | Default        | Description                                                 |
|-------------|----------|----------------|-------------------------------------------------------------|
| `--socket`  | path     | platform default | Override the IPC socket path (also via `$ROOST_SOCKET`)   |
| `--json`    | bool     | false          | Emit machine-readable JSON output where applicable          |
| `--timeout` | duration | `3s`           | IPC dial+request timeout                                    |
| `-v`, `--verbose` | count | 0           | Increase verbosity (-v, -vv); same effect as `ROOST_DEBUG`  |

Socket precedence: explicit `--socket` > `$ROOST_SOCKET` > platform default.

## `notify`

Fire a notification. The target tab is resolved from `--tab` first, then `$ROOST_TAB_ID`. `TITLE` and `BODY` are positional.

```bash
roost-cli notify "Build done" "tests pass"
roost-cli notify "From CI" "deploy ready" --tab 3
```

Titles starting with a dash require `--`:

```bash
roost-cli notify -- "-leading-dash" "body"
```

| Position / flag | Type   | Default            | Description                                              |
|-----------------|--------|--------------------|----------------------------------------------------------|
| `TITLE` (1st)   | string | required           | Notification title                                       |
| `BODY` (2nd)    | string | empty              | Notification body                                        |
| `--tab`         | int    | `$ROOST_TAB_ID`    | Target tab id; required if env var is unset              |

## `identify`

Print metadata about the running app. Default output is a key-value list; `--json` emits the typed payload.

```bash
roost-cli identify
```

```text
socket:            /Users/charliek/Library/Application Support/Roost/roost.sock
pid:               14138
active_project_id: 1
active_tab_id:     5
```

Useful for verifying the socket is reachable and the env vars are wired correctly.

## `tab list`

List every tab grouped by project, in display order. Default output is a human-readable tree; pass `--json` for the typed `TabListResult`.

```bash
roost-cli tab list
roost-cli --json tab list
```

Each tab carries `id`, `title`, `agent_state`, `has_notification`, and `is_active` flags.

## `tab focus`

Switch the GUI to a specific tab — raises the window, switches the active project (if needed), and selects the tab. Used as the click-through target for desktop banners.

```bash
roost-cli tab focus           # focus the calling shell's tab ($ROOST_TAB_ID)
roost-cli tab focus 7         # focus tab 7 (positional)
```

Returns the previously focused tab in the response so callers can implement "focus back" without state.

## `tab set-title`

Set a tab's display title. Persists across restarts and locks the tab against subsequent OSC 1/2 escapes from the shell — once set via `tab set-title` (or via `Cmd-R` / `Alt-R` in the GUI), prompt-frame title-rewrites stop overwriting it. v1 has no in-app way to clear the lock; the workaround is to delete and recreate the tab.

```bash
roost-cli tab set-title "build-watcher"
roost-cli tab set-title "deploy" --tab 3
```

| Position / flag | Type   | Default            | Description                                              |
|-----------------|--------|--------------------|----------------------------------------------------------|
| `TITLE` (1st)   | string | required           | New tab title                                            |
| `--tab`         | int    | `$ROOST_TAB_ID`    | Target tab id; required if env var is unset              |

## `tab set-state`

Drive the sticky per-tab agent-state indicator. Useful for non-Claude agents and shell scripts.

```bash
roost-cli tab set-state running
roost-cli tab set-state idle --tab 3
```

| Position / flag | Type   | Default            | Description                                                                          |
|-----------------|--------|--------------------|--------------------------------------------------------------------------------------|
| `STATE` (1st)   | string | required           | One of `none`, `running`, `needs_input`, `idle`. Anything else returns `bad_request`. |
| `--tab`         | int    | `$ROOST_TAB_ID`    | Target tab id                                                                        |

## `claude install`

Generate `~/.config/roost/claude-settings.json` and print a bash alias snippet to stdout. See the [Claude Code Hooks](../guides/claude-code.md) guide for the full workflow.

```bash
roost-cli claude install >> ~/.bashrc
roost-cli claude install --force   # overwrite an existing file
```

The alias snippet goes to **stdout**; the "Wrote ..." status messages go to **stderr**, so `>> ~/.bashrc` appends only the alias. `--json` is rejected on this command — its product is a shell snippet, not data.

## `claude hook`

Internal: invoked by Claude Code via the generated settings file. Reads the hook payload from stdin, looks up `$ROOST_TAB_ID`, and translates lifecycle events into IPC calls.

```text
roost-cli claude hook session-start | prompt-submit | notification | stop | session-end
```

Always exits 0 with `{}` on stdout (Claude treats nonzero hooks as failures). Silently no-ops when run outside a Roost tab. Errors are written to stderr only when `ROOST_DEBUG` is set or `-v` is passed.

## Environment

| Variable           | Effect                                                              |
|--------------------|---------------------------------------------------------------------|
| `ROOST_SOCKET`     | Override the socket path the CLI dials into                         |
| `ROOST_TAB_ID`     | Default tab id when `--tab` is not given                            |
| `ROOST_PROJECT_ID` | The project id this tab lives in (auto-set, available to scripts)   |
| `ROOST_DEBUG`      | If set, `claude hook` writes failure messages to stderr             |

The first three are auto-set by the GUI when it spawns a tab's shell. You only need to set them by hand if you're invoking the CLI from outside Roost (e.g. from a CI runner).

## Exit codes

| Code | Meaning                                              |
|------|------------------------------------------------------|
| 0    | Success (and `--help`)                               |
| 1    | Runtime error (RPC failure, connection failure, etc.) |
| 2    | Usage error (missing argument, invalid flag, etc.)   |

## Wire format

The CLI sends one JSON request and reads one JSON response. The server enforces a 1-line-per-request framing on the wire:

```json
{"id":"1","method":"notification.create","params":{"tab_id":5,"title":"hi","body":""}}
{"id":"1","ok":true,"result":{"delivered":true}}
```

Method names match `internal/ipc` constants:

| Method                       | Purpose                                       |
|------------------------------|-----------------------------------------------|
| `notification.create`        | Used by `notify` and `claude hook`            |
| `tab.set_title`              | Used by `tab set-title`                       |
| `tab.focus`                  | Used by `tab focus`                           |
| `tab.list`                   | Used by `tab list`                            |
| `tab.set_state`              | Used by `tab set-state` and `claude hook`     |
| `tab.clear_notification`     | Used by `claude hook prompt-submit`           |
| `system.identify`            | Used by `identify`                            |
| `system.set_hook_active`     | Used by `claude hook session-start/end`       |

If you need to drive Roost from a language other than Go, this protocol is small enough to implement against directly — open the socket, encode JSON, write `\n`-terminated.
