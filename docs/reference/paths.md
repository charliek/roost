# Paths and Environment

Roost resolves all of its filesystem state once at startup. Other components read the paths from this resolution; nothing should derive its own.

## File locations

### macOS

All Roost state lives under `~/Library/Application Support/Roost/`:

| Path                                                | Purpose                                           |
|-----------------------------------------------------|---------------------------------------------------|
| `~/Library/Application Support/Roost/`              | Config + data + runtime root                      |
| `~/Library/Application Support/Roost/roost.db`      | SQLite database (projects, tabs, scrollback off)  |
| `~/Library/Application Support/Roost/roost.db-wal`  | SQLite write-ahead log (auto-created)             |
| `~/Library/Application Support/Roost/roost.db-shm`  | SQLite shared memory (auto-created)               |
| `~/Library/Application Support/Roost/roost.sock`    | Unix socket the GUI listens on                    |
| `~/Library/Application Support/Roost/config.toml`   | User-editable config (Phase 4 — not used yet)     |

### Linux

Linux follows XDG conventions:

| Path                                  | Purpose                                                    |
|---------------------------------------|------------------------------------------------------------|
| `$XDG_CONFIG_HOME/roost/config.toml`  | User-editable config; defaults to `~/.config/roost/`        |
| `$XDG_DATA_HOME/roost/roost.db`       | SQLite database; defaults to `~/.local/share/roost/`        |
| `$XDG_RUNTIME_DIR/roost/roost.sock`   | Unix socket; falls back to data dir when `XDG_RUNTIME_DIR` is unset |

The directories are created at first launch with mode `0700`.

## Environment variables Roost sets

When Roost spawns a tab's shell, it injects:

| Variable        | Purpose                                                              |
|-----------------|----------------------------------------------------------------------|
| `TERM`          | Set to `xterm-256color`                                              |
| `COLORTERM`     | Set to `truecolor`                                                   |
| `ROOST_TAB_ID`  | Integer tab id (used by `roost-cli` to route notifications)          |
| `ROOST_SOCKET`  | Absolute path to the Unix socket                                     |

Existing environment is inherited verbatim before these are set.

## Environment variables Roost reads

`roost-cli` reads:

| Variable        | Effect                                                               |
|-----------------|----------------------------------------------------------------------|
| `ROOST_SOCKET`  | Override the socket the CLI dials                                    |
| `ROOST_TAB_ID`  | Default tab id when `--tab` is not given                             |

The GUI does not currently honour any environment override for paths; if you need a different location, modify `internal/config` and rebuild.

## Inspecting the database

Use any SQLite client. The schema is small:

```bash
sqlite3 "$HOME/Library/Application Support/Roost/roost.db"
```

```sql
.schema project
.schema tab
SELECT id, name, cwd FROM project ORDER BY position;
SELECT id, project_id, cwd, title FROM tab ORDER BY project_id, position;
```

The `command` column on `tab` is reserved for future "task tabs" (auto-launched commands) and is always NULL in the current build.

## Resetting state

To wipe Roost's persistent state and start fresh:

```bash
# macOS
rm "$HOME/Library/Application Support/Roost/roost.db"*

# Linux (uses XDG_DATA_HOME with the spec-default fallback)
rm "${XDG_DATA_HOME:-$HOME/.local/share}/roost/roost.db"*
```

Relaunch `roost`. It will recreate the schema and a `default` project + tab.
