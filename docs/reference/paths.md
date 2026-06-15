# Paths and Environment

Roost resolves all of its filesystem state once at startup. Other components read the paths from this resolution; nothing should derive its own.

Each UI owns its own `BundleProfile` — two variants, `Mac` (Swift `Roost.app`, `CFBundleIdentifier ai.stridelabs.Roost`) and `Gtk` (gtk4-rs `roost-linux`, app id `ai.stridelabs.Roost.gtk`). There is no shared daemon; the profile a UI resolves determines the socket `roostctl` dials. The Rust definition lives in `crates/roost-ipc/src/paths.rs`; the Swift companion is `mac/Sources/Roost/BundleProfile.swift`. The two implementations are tested in lockstep.

The profile defaults to:

| Binary       | Default profile | Override |
|--------------|------------------|----------|
| Swift `Roost.app` | `Mac` | n/a (the app picks `Mac` directly) |
| `roost-linux`     | `Gtk` | `ROOST_BUNDLE_PROFILE=mac` to dial a `Mac`-profile UI |
| `roostctl` (binary from the `roost-cli` crate) | `Mac` | `ROOST_BUNDLE_PROFILE` / `--socket` / `ROOST_SOCKET` / `--target {mac,gtk}` |

## File locations

The user-editable config file lives under XDG on **both** platforms — `~/.config/roost/config.conf` (or `$XDG_CONFIG_HOME/roost/config.conf` if set). Set `ROOST_CONFIG` to an absolute path to read config from there instead (used by the E2E harness to drive the command launcher off a seeded config). The state files (`state.json`, socket) follow each platform's native convention. The directory component on macOS is the profile's `app_label` — `Roost` or `Roost-gtk`.

Set `ROOST_STATE_DIR` to an **absolute** path to redirect **only** the state directory (where `state.json` lives) — the socket, single-instance lock, and log dir stay on the default profile path, so `roostctl` and the E2E harness still find the running UI by its unchanged socket. The E2E harness uses this to give each run an isolated, throwaway `state.json` without touching a developer's real saved tabs. Unlike `ROOST_CONFIG` (which accepts any non-empty value), `ROOST_STATE_DIR` requires an absolute path: a relative value is ignored (a relative state dir would resolve against the process's working directory). Note this does **not** isolate the macOS app's `UserDefaults` (e.g. sidebar visibility), which is a separate store.

This is a deliberate divergence from Apple's HIG on macOS: Roost matches the convention used by Ghostty, nvim, fish, and most CLI-adjacent tools, which keeps user-edited config alongside the rest of one's dotfiles. State files (which the user does not edit) stay in `~/Library/Application Support/<app_label>/` and the socket lives in `~/Library/Caches/<app_label>/`.

### macOS — `Mac` profile (Swift `Roost.app`)

| Path | Purpose |
|---|---|
| `~/.config/roost/config.conf` | User-editable config; see [Config keys](#config-keys) below |
| `~/Library/Application Support/Roost/state.json` | UI-owned workspace state (projects, tabs) |
| `~/Library/Caches/Roost/roost.sock` | Unix socket the UI listens on |
| `~/Library/Caches/Roost/roost.lock` | flock-based single-instance lock |
| `~/Library/Logs/Roost/roost.log` | App log |

### macOS — `Gtk` profile (`cargo run -p roost-linux` dev mode)

Same shape as the `Mac` profile with `Roost-gtk` in place of `Roost`:

| Path | Purpose |
|---|---|
| `~/Library/Application Support/Roost-gtk/state.json` | GTK-app workspace state |
| `~/Library/Caches/Roost-gtk/roost.sock` | GTK-app Unix socket |
| `~/Library/Caches/Roost-gtk/roost.lock` | GTK-app single-instance lock |
| `~/Library/Logs/Roost-gtk/roost.log` | GTK-app log (also teed to stdout); distinct from the Swift app's `~/Library/Logs/Roost/roost.log` |

### Linux

Linux follows XDG conventions for everything. There is only one UI variant on Linux — both `Mac` and `Gtk` profile kinds resolve to the same XDG paths.

| Path | Purpose |
|---|---|
| `$XDG_CONFIG_HOME/roost/config.conf` | User-editable config; defaults to `~/.config/roost/` |
| `$XDG_DATA_HOME/roost/state.json` | UI-owned workspace state; defaults to `~/.local/share/roost/` |
| `$XDG_RUNTIME_DIR/roost/roost.sock` | Unix socket; falls back to `/tmp/roost-<uid>/roost.sock` when `XDG_RUNTIME_DIR` is unset |
| `$XDG_STATE_HOME/roost/roost.log` | app log (also teed to stdout); falls back to `~/.local/state/roost/` |

The directories are created at first launch with mode `0700`.

### No migration from pre-rewrite lowercase paths

Pre-rewrite builds stored their state under lowercase `~/Library/Application Support/roost/` and `~/Library/Caches/roost/`. The current builds use capital `Roost`. There is no auto-migration — state in the lowercase directories is intentionally orphaned, and a pre-rewrite build's SQLite database is not migrated into `state.json`. Start empty.

## Config keys

`config.conf` is a tiny `key = value` file (no sections, no nesting). Lines starting with `#` are comments. Missing file → built-in defaults; unknown keys are ignored. Keybindings use Ghostty's `keybind = trigger=action` syntax — see [Keybindings](../getting-started/keybindings.md#custom-keybindings) for the full action list. The full reference (including the `copy-on-select` semantics) lives in [`config.md`](config.md).

Keys use Ghostty-style hyphens (`font-family`, not `font_family`); a misspelled key is silently ignored.

| Key              | Default                              | Effect                                                 |
|------------------|--------------------------------------|--------------------------------------------------------|
| `font-family`    | `JetBrains Mono, Monaco, monospace`  | Comma-separated list. The first installed family wins. |
| `font-size`      | `12`                                 | Points.                                                |
| `theme`          | `roost-dark`                         | Bundled color theme name. See [Themes](themes.md).     |
| `keybind`        | (built-in defaults; see Keybindings) | Repeatable. `<trigger> = <action>`; later lines override. |
| `command`        | (none)                               | Repeatable. A command-launcher entry (`Cmd/Alt+Shift+T`). See [Command launcher](#command-launcher) below. |
| `copy-on-select` | `true`                               | `off` / `true` / `clipboard`. Controls what a mouse-drag selection writes on release. See [`config.md`](config.md#copy-on-select) for per-platform behavior. |

Tab-strip pill widths (`tab-min-width` / `tab-max-width`, macOS) are documented in [Tab Strip](tab-strip.md#config-keys).

Roost probes the system at startup for each candidate in `font-family` (left-to-right) and picks the first that's installed. Pango's own comma-separated fallback is unreliable on macOS — when the head of the list is missing it can silently fall through to a *proportional* font (Verdana), which produces wide cells with narrow glyphs and huge gaps between letters. The probe avoids that.

If none of the requested families exist, Roost falls back to `monospace` and logs a warning at startup:

```bash
./roost 2>&1 | grep -i 'font:'
```

Successful family selection is logged at debug level only (silent on a normal launch); the surface signal is the absence of a warning.

Example `config.conf`:

```conf
font-family = Iosevka, JetBrains Mono, Monaco, monospace
font-size   = 13

# Add a second trigger for new_tab without removing the default Cmd-T.
keybind = super+j = new_tab

# Disable the default rename-project shortcut.
keybind = super+shift+r = unbind

# Command-launcher entries (Cmd/Alt+Shift+T).
command = label="Lazygit" run="lazygit"
command = label="Logs" run="docker compose logs -f" hold=true
```

## Command launcher

Each `command =` line adds an entry to the command launcher
(`Cmd-Shift-T` / `Alt-Shift-T`). Activating one spawns a new tab in the
active project and runs the command through your login shell. The value
is a record of quote-aware `key="value"` tokens:

| Token   | Required | Effect                                                              |
|---------|----------|---------------------------------------------------------------------|
| `label` | yes      | The text shown in the launcher list.                                |
| `run`   | yes      | The shell command to run.                                           |
| `title` | no       | The tab title (defaults to `label`).                                |
| `hold`  | no       | `hold=true` keeps the shell open after the command exits (otherwise the tab closes when it finishes). |
| `env`   | no       | `env="KEY=VALUE"` exported before `run`. Repeat the token for more. |

A line missing `label` or `run` is skipped (logged, not fatal). The
launcher reads the config fresh each time it opens, so edits take effect
without a restart.

## Environment variables Roost sets

When Roost spawns a tab's shell, it injects the following. Existing
environment is inherited verbatim *before* these are set — the user's
own values for `TERM_PROGRAM_VERSION` etc. would be overwritten;
`ROOST_SHELL_FEATURES` is the only one that defers to a pre-existing
value (you can opt out of the default features by setting it in your
rc — see [Feature flags](../guides/cwd-tracking.md#feature-flags)).

### Terminal advertisement

| Variable               | Value             | Purpose                                                                  |
|------------------------|-------------------|--------------------------------------------------------------------------|
| `TERM`                 | `xterm-256color`  | Terminfo entry the shell should use. Roost emulates xterm-256color faithfully. |
| `COLORTERM`            | `truecolor`       | Signals 24-bit color support to modern TUIs (opencode, neovim, lazygit). Stripped at the SSH boundary unless [`ssh-env`](../guides/cwd-tracking.md#feature-flags) wraps `ssh` to forward it. |
| `TERM_PROGRAM`         | `Roost`           | Lets remote tools detect they're running inside Roost.                   |
| `TERM_PROGRAM_VERSION` | bundle short version | Same use case; tracks the running Roost build.                       |
| `FORCE_HYPERLINK`      | `1`               | Advertises OSC 8 hyperlink support. CLIs that gate on the `supports-hyperlinks` library (Claude Code, anything on chalk/terminal-link) only allowlist known terminals by `TERM_PROGRAM`, and `Roost` isn't one — without this they emit plain text instead of clickable links (e.g. Claude Code's footer `PR #N`). Roost renders + opens OSC 8 links (Cmd/Ctrl-click), so the override is honest. Forwarded over SSH via [`ssh-env`](../guides/cwd-tracking.md#feature-flags). |

### Tab identity + IPC routing

| Variable        | Purpose                                                              |
|-----------------|----------------------------------------------------------------------|
| `ROOST_TAB_ID`  | Integer tab id (used by `roostctl` to route notifications). Gate any shell-integration extension you write on this. |
| `ROOST_SOCKET`  | Absolute path to the Unix domain socket (`roostctl` auto-detects it from this). |

### Shell integration

| Variable                  | Value                              | Purpose                                                 |
|---------------------------|------------------------------------|---------------------------------------------------------|
| `ROOST_SHELL_INTEGRATION` | `1`                                | Marker that the shell-integration env contract is in effect. |
| `ROOST_SHELL_FEATURES`    | `cwd,title,marks,prompt,ssh-env`*  | Comma list of features the shipped scripts enable. Prefix any feature with `no-` to disable it (e.g. `cwd,title,marks,prompt,no-ssh-env`). See [Feature flags](../guides/cwd-tracking.md#feature-flags). |
| `ROOST_RESOURCES_DIR`     | absolute path                      | Directory holding the shipped `shell-integration/` scripts. Source `$ROOST_RESOURCES_DIR/shell-integration/roost.bash` (or `.zsh`) to load them manually. |

\* Default only when `ROOST_SHELL_FEATURES` is unset in the inherited
env; set it in your rc / launch config to override.

### Internal bootstrap (don't depend on these)

Roost also sets `ZDOTDIR` (zsh) and `ENV` + a few `ROOST_BASH_*`
helpers (bash auto-bootstrap) to inject the shell integration without
requiring the user to edit their rc. These are reserved internals —
read them if you're debugging Roost's startup, but don't build on
them from user code.

### `ssh-env` and the SSH boundary

Without intervention, macOS's default `/etc/ssh/ssh_config.d/100-macos.conf`
only forwards `LANG LC_*` over `ssh` — `COLORTERM` (and
`TERM_PROGRAM` / `TERM_PROGRAM_VERSION`) silently drop, so modern TUIs
on the remote host fall back to 256-color rendering. The `ssh-env`
feature (default on) defines an `ssh` shell function that adds
`-o "SendEnv COLORTERM TERM_PROGRAM TERM_PROGRAM_VERSION FORCE_HYPERLINK"` to
every invocation. The remote host has to *accept* the forwarded vars
(`sshd_config::AcceptEnv`); Debian/Ubuntu defaults only accept
`LANG LC_*`, so the server-side setting often needs updating too.
See [Feature flags](../guides/cwd-tracking.md#feature-flags) for the
opt-out (`no-ssh-env`).

## Environment variables Roost reads

`roostctl` reads:

| Variable | Effect |
|---|---|
| `ROOST_SOCKET` | Override the socket the CLI dials |
| `ROOST_TAB_ID` | Default tab id when `--tab` is not given |

`roostctl` also honours `ROOST_BUNDLE_PROFILE=mac|gtk` to pick which UI's socket it dials by default (useful when a Mac `Roost.app` and a GTK dev UI both run on macOS).

## Resetting state

To wipe Roost's persistent state and start fresh:

```bash
# macOS — Mac profile (Swift Roost.app)
rm "$HOME/Library/Application Support/Roost/state.json"

# macOS — Gtk dev profile (cargo run -p roost-linux on Mac)
rm "$HOME/Library/Application Support/Roost-gtk/state.json"

# Linux (uses XDG_DATA_HOME with the spec-default fallback)
rm "${XDG_DATA_HOME:-$HOME/.local/share}/roost/state.json"
```

`state.json` is the UI-owned persistent store. Relaunch the UI — it will recreate default state on first run.
