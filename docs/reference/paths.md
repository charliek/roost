# Paths and Environment

Roost resolves all of its filesystem state once at startup. Other components read the paths from this resolution; nothing should derive its own.

All path resolution flows through a `BundleProfile` — two variants, `Mac` (Swift `Roost.app`, `CFBundleIdentifier ai.stridelabs.Roost`) and `Gtk` (gtk4-rs `roost-linux`, app id `ai.stridelabs.Roost.gtk`). The Rust definition lives in `crates/roost-common/src/lib.rs`; the Swift companion is `mac/Sources/Roost/BundleProfile.swift`. The two implementations are tested in lockstep.

The profile defaults to:

| Binary       | Default profile | Override |
|--------------|------------------|----------|
| Swift `Roost.app` | `Mac` | n/a (the app picks `Mac` directly) |
| `roost-linux`     | `Gtk` | `ROOST_BUNDLE_PROFILE=mac` to dial a `Mac`-profile daemon |
| `roost-core` (daemon, removed in M7) | `Mac` | `ROOST_BUNDLE_PROFILE=gtk` |
| `roost-cli-rs` / `roostctl` | `Mac` | `ROOST_BUNDLE_PROFILE` / `--socket` / `ROOST_SOCKET` |

## File locations

The user-editable config file lives under XDG on **both** platforms — `~/.config/roost/config.conf` (or `$XDG_CONFIG_HOME/roost/config.conf` if set). The state files (database, socket) follow each platform's native convention. The directory component on macOS is the profile's `app_label` — `Roost` or `Roost-gtk`.

This is a deliberate divergence from Apple's HIG on macOS: Roost matches the convention used by Ghostty, nvim, fish, and most CLI-adjacent tools, which keeps user-edited config alongside the rest of one's dotfiles. State files (which the user does not edit) stay in `~/Library/Application Support/<app_label>/` and the socket lives in `~/Library/Caches/<app_label>/`.

### macOS — `Mac` profile (Swift `Roost.app`)

| Path | Purpose |
|---|---|
| `~/.config/roost/config.conf` | User-editable config; see [Config keys](#config-keys) below |
| `~/Library/Application Support/Roost/roost.db` | SQLite database (projects, tabs) — kept until the daemon is deleted in M7 |
| `~/Library/Application Support/Roost/roost.db-wal` | SQLite write-ahead log (auto-created) |
| `~/Library/Application Support/Roost/roost.db-shm` | SQLite shared memory (auto-created) |
| `~/Library/Application Support/Roost/state.json` | UI-owned workspace state (post-M3); replaces SQLite |
| `~/Library/Caches/Roost/roost.sock` | Unix socket the UI listens on |
| `~/Library/Caches/Roost/roost.lock` | flock-based single-instance lock (post-M3) |
| `~/Library/Logs/Roost/roost.log` | App log |

### macOS — `Gtk` profile (`cargo run -p roost-linux` dev mode)

Same shape as the `Mac` profile with `Roost-gtk` in place of `Roost`:

| Path | Purpose |
|---|---|
| `~/Library/Application Support/Roost-gtk/state.json` | GTK-app workspace state |
| `~/Library/Caches/Roost-gtk/roost.sock` | GTK-app Unix socket |
| `~/Library/Caches/Roost-gtk/roost.lock` | GTK-app single-instance lock |
| `~/Library/Logs/Roost-gtk/roost.log` | GTK-app log |

### Linux

Linux follows XDG conventions for everything. There is only one UI variant on Linux — both `Mac` and `Gtk` profile kinds resolve to the same XDG paths.

| Path | Purpose |
|---|---|
| `$XDG_CONFIG_HOME/roost/config.conf` | User-editable config; defaults to `~/.config/roost/` |
| `$XDG_DATA_HOME/roost/roost.db` | SQLite database (pre-M7); defaults to `~/.local/share/roost/` |
| `$XDG_DATA_HOME/roost/state.json` | UI-owned workspace state (post-M3) |
| `$XDG_RUNTIME_DIR/roost/roost.sock` | Unix socket; falls back to `/tmp/roost-<uid>/roost.sock` when `XDG_RUNTIME_DIR` is unset |
| `$XDG_STATE_HOME/roost/roost.log` | App log; falls back to `~/.local/state/roost/` |

The directories are created at first launch with mode `0700`.

### No migration from pre-M1 lowercase paths

Pre-M1 builds of the refactor branch stored their state under lowercase `~/Library/Application Support/roost/` and `~/Library/Caches/roost/`. M1 of the daemon-removal refactor flipped these to capital `Roost`. There is no auto-migration — state in the lowercase directories is intentionally orphaned. Refactor-branch users will see a fresh SQLite database / `state.json` after upgrading.

The same policy will apply when the daemon's SQLite goes away in M3+M7 — the SQLite file is intentionally not migrated into `state.json`. Drop SQLite, start empty.

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

The daemon also honours `ROOST_BUNDLE_PROFILE=mac|gtk` to override its default `Mac` profile (useful when running the daemon side-by-side with the Mac UI vs the GTK dev UI on macOS).

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
# macOS — Mac profile (Swift Roost.app)
rm "$HOME/Library/Application Support/Roost/roost.db"*
rm "$HOME/Library/Application Support/Roost/state.json"

# macOS — Gtk dev profile (cargo run -p roost-linux on Mac)
rm "$HOME/Library/Application Support/Roost-gtk/roost.db"*
rm "$HOME/Library/Application Support/Roost-gtk/state.json"

# Linux (uses XDG_DATA_HOME with the spec-default fallback)
rm "${XDG_DATA_HOME:-$HOME/.local/share}/roost/roost.db"*
rm "${XDG_DATA_HOME:-$HOME/.local/share}/roost/state.json"
```

`roost.db*` is the pre-M7 SQLite database (still present while the daemon is alive); `state.json` is the M3-onwards UI-owned persistent state. Relaunch the UI — it will recreate default state on first run.
