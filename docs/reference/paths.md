# Paths and Environment

Roost resolves all of its filesystem state once at startup. Other components read the paths from this resolution; nothing should derive its own.

## File locations

The user-editable config file lives under XDG on **both** platforms — `~/.config/roost/config.conf` (or `$XDG_CONFIG_HOME/roost/config.conf` if set). The state files (database, socket) follow each platform's native convention.

This is a deliberate divergence from Apple's HIG on macOS: Roost matches the convention used by Ghostty, nvim, fish, and most CLI-adjacent tools, which keeps user-edited config alongside the rest of one's dotfiles. State files (which the user does not edit) stay in `~/Library/Application Support/roost/` and the socket lives in `~/Library/Caches/roost/`.

### macOS

| Path | Purpose |
|---|---|
| `~/.config/roost/config.conf` | User-editable config; see [Config keys](#config-keys) below |
| `~/Library/Application Support/roost/roost.db` | SQLite database (projects, tabs) |
| `~/Library/Application Support/roost/roost.db-wal` | SQLite write-ahead log (auto-created) |
| `~/Library/Application Support/roost/roost.db-shm` | SQLite shared memory (auto-created) |
| `~/Library/Caches/roost/roost.sock` | Unix socket the daemon listens on |

The legacy Go binary used `~/Library/Application Support/Roost/` (capital `R`) for both state and socket. The Rust path lowercases the directory and moves the socket to `~/Library/Caches/roost/` to follow Apple's convention that sockets are caches, not user data.

### Linux

Linux follows XDG conventions for everything:

| Path | Purpose |
|---|---|
| `$XDG_CONFIG_HOME/roost/config.conf` | User-editable config; defaults to `~/.config/roost/` |
| `$XDG_DATA_HOME/roost/roost.db` | SQLite database; defaults to `~/.local/share/roost/` |
| `$XDG_RUNTIME_DIR/roost/roost.sock` | Unix socket; falls back to `/tmp/roost-<uid>/roost.sock` when `XDG_RUNTIME_DIR` is unset |

The directories are created at first launch with mode `0700`.

### Migrating from a pre-cutover macOS install

If you previously ran the Go binary, its state at `~/Library/Application Support/Roost/` (capital `R`) is untouched by the Rust binary. The Rust binary writes a fresh SQLite database at `~/Library/Application Support/roost/roost.db` (lowercase `r`) on first launch.

To carry over your projects and tabs from the Go binary, copy the database before launching the Rust binary for the first time — the schema ports byte-for-byte (see [Vision → DL-7](../development/vision.md#dl-7-sqlite-migrations-port-byte-for-byte)):

```bash
mkdir -p ~/Library/Application\ Support/roost
cp ~/Library/Application\ Support/Roost/roost.db ~/Library/Application\ Support/roost/roost.db
```

Roost does not auto-move the file — moving user state across binaries silently is the kind of thing that loses work.

## Config keys

`config.conf` is a tiny `key = value` file (no sections, no nesting). Lines starting with `#` are comments. Missing file → built-in defaults; unknown keys are ignored. Keybindings use Ghostty's `keybind = trigger=action` syntax — see [Keybindings](../getting-started/keybindings.md#custom-keybindings) for the full action list.

| Key           | Default                              | Effect                                                 |
|---------------|--------------------------------------|--------------------------------------------------------|
| `font_family` | `JetBrains Mono, Monaco, monospace`  | Comma-separated list. The first installed family wins. |
| `font_size`   | `12`                                 | Pango points.                                          |
| `theme`       | `roost-dark`                         | Bundled color theme name. See [Themes](themes.md).     |
| `keybind`     | (built-in defaults; see Keybindings) | Repeatable. `trigger=action`; later lines override.    |

Roost probes the system at startup for each candidate in `font_family` (left-to-right) and picks the first that's installed. Pango's own comma-separated fallback is unreliable on macOS — when the head of the list is missing it can silently fall through to a *proportional* font (Verdana), which produces wide cells with narrow glyphs and huge gaps between letters. The probe avoids that.

If none of the requested families exist, Roost falls back to `monospace` and logs a warning at startup:

```bash
./roost 2>&1 | grep -i 'font:'
```

Successful family selection is logged at debug level only (silent on a normal launch); the surface signal is the absence of a warning.

Example `config.conf`:

```conf
font_family = Iosevka, JetBrains Mono, Monaco, monospace
font_size   = 13

# Add a second trigger for new_tab without removing the default Cmd-T.
keybind = super+j = new_tab

# Disable the default rename-project shortcut.
keybind = super+shift+r = unbind
```

## Environment variables Roost sets

When Roost spawns a tab's shell, it injects:

| Variable        | Purpose                                                              |
|-----------------|----------------------------------------------------------------------|
| `TERM`          | Set to `xterm-256color`                                              |
| `COLORTERM`     | Set to `truecolor`                                                   |
| `ROOST_TAB_ID`  | Integer tab id (used by `roost-cli-rs` to route notifications)          |
| `ROOST_SOCKET`  | Absolute path to the Unix socket                                     |

Existing environment is inherited verbatim before these are set.

## Environment variables Roost reads

`roost-cli-rs` reads:

| Variable | Effect |
|---|---|
| `ROOST_SOCKET` | Override the socket the CLI dials |
| `ROOST_TAB_ID` | Default tab id when `--tab` is not given |
| `ROOST_PROJECT_ID` | Default project id, set by the UI |

The daemon does not currently honour any environment override for the database or socket paths; if you need a different location, modify `crates/roost-common/src/lib.rs` and rebuild.

## Inspecting the database

Use any SQLite client. The schema is small:

```bash
sqlite3 "$HOME/Library/Application Support/roost/roost.db"
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
rm "$HOME/Library/Application Support/roost/roost.db"*

# Linux (uses XDG_DATA_HOME with the spec-default fallback)
rm "${XDG_DATA_HOME:-$HOME/.local/share}/roost/roost.db"*
```

Relaunch the UI. The daemon will recreate the schema and a default project + tab.
