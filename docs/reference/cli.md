# `roost-cli`

Companion CLI for the running Roost GUI. Talks to the GUI over a Unix socket using newline-delimited JSON-RPC. Intended to be invoked from inside a Roost tab (typically by Claude Code hooks), but works from any shell that can reach the socket.

## Usage

```text
roost-cli <command> [flags]
```

| Command       | Purpose                                                         |
|---------------|-----------------------------------------------------------------|
| `notify`      | Fire a notification on a tab                                    |
| `set-title`   | Set a tab's title from the CLI                                  |
| `identify`    | Print info about the running GUI (socket path, active tab, PID) |
| `help`        | Print usage                                                     |

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

Set a tab's display title. Persists across restarts.

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

## Environment

| Variable        | Effect                                                              |
|-----------------|---------------------------------------------------------------------|
| `ROOST_SOCKET`  | Override the socket path the CLI dials into                          |
| `ROOST_TAB_ID`  | Default tab id when `--tab` is not given                             |

Both are auto-set by the GUI when it spawns a tab's shell. You only need to set them by hand if you're invoking the CLI from outside Roost (e.g. from a CI runner).

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

| Method                   | Purpose                  |
|--------------------------|--------------------------|
| `notification.create`    | Used by `notify`         |
| `tab.set_title`          | Used by `set-title`      |
| `system.identify`        | Used by `identify`       |

If you need to drive Roost from a language other than Go, this protocol is small enough to implement against directly — open the socket, encode JSON, write `\n`-terminated.
