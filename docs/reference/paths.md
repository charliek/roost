# Paths and Environment

Roost resolves all of its filesystem state once at startup. Other components read the paths from this resolution; nothing should derive its own.

There are three UI bundle profiles — `Mac`, `Linux`, and `Iced` (slugs `mac`, `linux`, `iced`) — and each running UI resolves exactly one of them; each owns its own workspace + PTY supervisor in-process, so there is no daemon a UI depends on. A fourth profile, `Session`, is resolved by the headless `roost-session` daemon (HS-1a, plan 035) — a workspace + PTY supervisor with no UI attached, opt-in via `roostctl session start` (see [Session profile](#session-profile) below). It is not a legal `roostctl --target` value or `ROOST_BUNDLE_PROFILE` value; `roostctl session start|stop|status` address its socket directly instead. The Rust definition lives in `crates/roost-ipc/src/paths.rs`; the Swift companion is `mac/Sources/Roost/BundleProfile.swift`. On macOS the two implementations are tested in lockstep.

| Profile | Who resolves it | `app_label` | `app_id` |
|---|---|---|---|
| `Mac`   | the Swift `Roost.app` | `Roost` | `ai.stridelabs.Roost` |
| `Linux` | the packaged Linux UI (`/usr/bin/roost`) | `Roost-linux` | `ai.stridelabs.Roost` on Linux; `ai.stridelabs.Roost.linux` on macOS |
| `Iced`  | a dev build of `roost-iced`, and the experimental macOS `Roost-Iced.app` | `Roost-iced` | `ai.stridelabs.Roost.iced` |
| `Session` | the headless `roost-session` daemon (`/usr/bin/roost-session` on the Linux `.deb`) | `RoostSession` (`RoostSessionDev` in debug builds) | `ai.stridelabs.Roost.session` |

`app_label` is fixed per profile on every platform — it is the string `identify` and `roostctl doctor` report, and on macOS it is also the directory component of the profile's paths. `app_id` is the only field that resolves per platform: on Linux the `Linux` profile shares `ai.stridelabs.Roost` with the `Mac` profile, which can never run there and already shares that platform's path namespace; on macOS every profile stays independently resolvable (so their paths and identities can never collide), even though nothing ships or launches the `Linux` one there.

The Linux `.deb` ships `roost-iced` built with the `roost-iced/linux-package` Cargo feature, which flips its compiled-in default profile from `Iced` to `Linux` on Linux. The `Linux` profile's paths are **byte-identical** to the ones the pre-iced Linux package resolved (it is the same profile, renamed), so an installed Roost upgrades in place with no migration and no change to `roostctl` or Claude hooks. Dev builds (no feature) and every non-Linux platform keep the isolated `Iced` profile. `ROOST_BUNDLE_PROFILE` overrides the compiled-in default in every build, packaged or not.

The profile defaults to:

| Binary       | Default profile | Override |
|--------------|------------------|----------|
| Swift `Roost.app` | `Mac` | n/a (the app picks `Mac` directly) |
| `roost` (Linux `.deb`, built `--features roost-iced/linux-package`) | `Linux` | `ROOST_BUNDLE_PROFILE=iced` to keep the isolated profile in a packaged build (or `=mac` to target another UI) |
| `roost-iced` (dev build, any platform; macOS `Roost-Iced.app`) | `Iced` | `ROOST_BUNDLE_PROFILE=mac` / `=linux` to dial another profile's namespace |
| `roostctl` (binary from the `roost-cli` crate) | auto-detect | `ROOST_BUNDLE_PROFILE` / `--socket` / `ROOST_SOCKET` / `--target {mac,linux,iced}` |

The two sides treat an **unrecognized** `ROOST_BUNDLE_PROFILE` value differently, on purpose. A UI logs a warning and falls back to its compiled-in default rather than refusing to launch; `roostctl` hard-errors with `unknown ROOST_BUNDLE_PROFILE value … (expected mac, linux, or iced)`. A stale `ROOST_BUNDLE_PROFILE=gtk` left over from an older install therefore starts the UI on its normal profile (with a warning in the log) but stops the CLI outright. `session` gets the same `roostctl` rejection — it names a real profile kind internally, but a session is deliberately not one of `--target`'s / `ROOST_BUNDLE_PROFILE`'s legal values, by design (see [Session profile](#session-profile)), not because it is unimplemented.

## File locations

The user-editable config file lives under XDG on **both** platforms — `~/.config/roost/config.conf` (or `$XDG_CONFIG_HOME/roost/config.conf` if set). Set `ROOST_CONFIG` to an absolute path to read config from there instead (used by the E2E harness to drive the command launcher off a seeded config). The state files (`state.json`, socket) follow each platform's native convention. The directory component on macOS is the profile's `app_label` — `Roost`, `Roost-linux`, `Roost-iced`, or `RoostSession`/`RoostSessionDev` for a headless session.

Set `ROOST_STATE_DIR` to an **absolute** path to redirect **only** the state directory (where `state.json` and its `state.lock` live) — the socket, the socket lock, and the log dir stay on the default profile path, so `roostctl` and the E2E harness still find the running UI by its unchanged socket. Note the consequence: two UIs with different `ROOST_STATE_DIR` values no longer collide on state, so a collision that used to be loud is now silent isolation; the socket lock is what still catches a genuine second instance on one socket. The E2E harness uses this to give each run an isolated, throwaway `state.json` without touching a developer's real saved tabs. Unlike `ROOST_CONFIG` (which accepts any non-empty value), `ROOST_STATE_DIR` requires an absolute path: a relative value is ignored (a relative state dir would resolve against the process's working directory). Note this does **not** isolate the macOS app's `UserDefaults` (e.g. sidebar visibility), which is a separate store.

One derivation rides on top of the seam, and it is asymmetric on purpose. When something **launches** a headless `roost-session` for you — the UI's Connect on a `localhost` host, or `roostctl session start` — and `ROOST_STATE_DIR` is set to a value the resolver honours, the daemon is handed `<that directory>/session` instead of inheriting the value verbatim. Without it the daemon would resolve its launcher's *own* state dir, find that launcher's `state.lock` already held, and refuse to start ("another session (pid N) is using this state directory") — the isolation seam colliding with itself. `roostctl session start` prints the derived path on stderr when it does this; the UI records it in its log. The daemon stays addressable either way, because the socket does not move with `ROOST_STATE_DIR` — `roostctl session status|stop` find it by socket. The other half of the asymmetry: the daemon's own reading of the variable is unchanged, so running `ROOST_STATE_DIR=/tmp/x roost-session start` **directly** still puts its state in `/tmp/x` verbatim. Only a launcher derives, and only from a value that would redirect its own state dir — an empty or relative value derives nothing and is passed through untouched.

This is a deliberate divergence from Apple's HIG on macOS: Roost matches the convention used by Ghostty, nvim, fish, and most CLI-adjacent tools, which keeps user-edited config alongside the rest of one's dotfiles. State files (which the user does not edit) stay in `~/Library/Application Support/<app_label>/` and the socket lives in `~/Library/Caches/<app_label>/`.

### macOS — `Mac` profile (Swift `Roost.app`)

| Path | Purpose |
|---|---|
| `~/.config/roost/config.conf` | User-editable config; see [Config keys](#config-keys) below |
| `~/Library/Application Support/Roost/state.json` | UI-owned workspace state (projects, tabs) |
| `~/Library/Application Support/Roost/state.lock` | flock guarding `state.json` (moves with `ROOST_STATE_DIR`) |
| `~/Library/Caches/Roost/roost.sock` | Unix socket the UI listens on |
| `~/Library/Caches/Roost/roost.lock` | flock guarding the socket's bind + lifetime |
| `~/Library/Logs/Roost/roost.log` | App log |

### macOS — `Iced` profile (`Roost-Iced.app`, or `cargo run -p roost-iced`)

Same shape as the `Mac` profile with `Roost-iced` in place of `Roost`, so the
experimental iced build runs beside the Swift app without touching its state:

| Path | Purpose |
|---|---|
| `~/Library/Application Support/Roost-iced/state.json` | Iced workspace state |
| `~/Library/Application Support/Roost-iced/state.lock` | Iced state lock |
| `~/Library/Caches/Roost-iced/roost.sock` | Iced Unix socket |
| `~/Library/Caches/Roost-iced/roost.lock` | Iced socket lock |
| `~/Library/Logs/Roost-iced/roost.log` | Iced log (also teed to stdout) |

### macOS — `Linux` profile

The `Linux` profile still *resolves* on macOS (`Roost-linux` /
`ai.stridelabs.Roost.linux`, same shape as the two above), but nothing
ships or launches it there. You only see these paths if you set
`ROOST_BUNDLE_PROFILE=linux` by hand:
`~/Library/Application Support/Roost-linux/`,
`~/Library/Caches/Roost-linux/`, `~/Library/Logs/Roost-linux/`.

### Linux

Linux follows XDG conventions for everything. The `Mac` and `Linux` profile
kinds both resolve to the production `roost` namespace, and the packaged
`.deb` resolves `Linux` (see above), so the installed package lands here.
A dev build of `roost-iced` stays in a separate `roost-iced` namespace so
it can run beside the installed package.

| Path | Purpose |
|---|---|
| `$XDG_CONFIG_HOME/roost/config.conf` | User-editable config; defaults to `~/.config/roost/` |
| `$XDG_DATA_HOME/roost/state.json` | UI-owned workspace state; defaults to `~/.local/share/roost/` |
| `$XDG_DATA_HOME/roost/state.lock` | flock guarding `state.json`; moves with `ROOST_STATE_DIR` |
| `$XDG_RUNTIME_DIR/roost/roost.sock` | Unix socket; falls back to `/tmp/roost-<uid>/roost.sock` when `XDG_RUNTIME_DIR` is unset |
| `$XDG_RUNTIME_DIR/roost/roost.lock` | flock guarding the socket's bind + lifetime |
| `$XDG_STATE_HOME/roost/roost.log` | app log (also teed to stdout); falls back to `~/.local/state/roost/` |

For a dev build of Iced, replace each `roost` path component with
`roost-iced`; its socket fallback is `/tmp/roost-iced-<uid>/roost.sock`. A
packaged (`.deb`) build uses the `roost` paths above, unchanged.

These are the paths every previous Linux release used, and the rename of
the profile kind deliberately did not move any of them. In the same
spirit the package installs its desktop entry as
`/usr/share/applications/ai.stridelabs.Roost.desktop` **plus** a
`NoDisplay` alias at `ai.stridelabs.Roost.gtk.desktop` — the id v0.0.17
and earlier shipped, which a launcher pin created back then references
forever. The alias has no menu entry of its own; it exists so those pins
keep launching.

The directories are created at first launch with mode `0700`.

### Session profile

`Session` resolves paths the same way the three UI profiles do, and is
now live: the headless `roost-session` daemon (HS-1a, plan 035) resolves
it — the Linux `.deb` ships it as `/usr/bin/roost-session` and
`Roost-Iced.app` ships it at `Contents/MacOS/roost-session` (HS-4b),
opt-in via `roostctl session start` or the palette's
**Connect Host: localhost**. It is still **not** a `roostctl --target` value
and **not** a legal `ROOST_BUNDLE_PROFILE` value — `roostctl --target
session` is rejected, and `ROOST_BUNDLE_PROFILE=session` is rejected by
ordinary target resolution (the dedicated `session` verbs never consult
either knob — they resolve this profile directly), by design
(see `session.rs`'s module doc: a session is not a UI, and `start` has to
work when nothing is listening at all, so `roostctl session
start|stop|status` address this profile's socket directly instead as a
pre-connect carve-out). Any other op reaches a session only through an
explicit `roostctl --socket <path>`. Debug builds substitute
`RoostSessionDev` / `roost-session-dev` for the directory name in **all**
of the paths below (socket, state, and logs), so a dev session can never
collide with a real one.

| Platform | Socket | State | Logs |
|---|---|---|---|
| macOS | `~/Library/Caches/RoostSession/roost.sock` | `~/Library/Application Support/RoostSession/` | `~/Library/Logs/RoostSession/` |
| Linux | `$XDG_RUNTIME_DIR/roost-session/roost.sock`, falling back to `/tmp/roost-session-<uid>/roost.sock` | `$XDG_DATA_HOME/roost-session/` | `$XDG_STATE_HOME/roost-session/` |

Before creating any of the above, the daemon installs `umask 0077`, so
the state dir, log dir, and socket directory it creates land at `0700`
and `state.json` at `0600` (the socket file itself gets `0600` the same
way every profile's socket does — see [`ipc.md`](ipc.md#session-sockets)).
A session socket additionally checks the connecting peer's UID at accept
time and drops any connection from a different user, which no UI socket
does. `validate_runtime_dir` rejects rather than repairs a socket
directory some other mode or owner already created, so a session's
lock-acquisition step never silently inherits a loosened directory.

### SSH scratch directories

A saved host reached over SSH (host-sessions HS-3 — see [Host sessions (development) → Transport: SSH hosts](../development/host-sessions.md#transport-ssh-hosts)) does not use the session profile's paths above; the client owns a per-*attempt* scratch directory of its own, one per connect attempt rather than one per host, under whichever of the following candidates leaves room for a 103-byte `AF_UNIX` `sun_path`:

```text
$TMPDIR/roost-ssh-<host_id>-<pid>-<seq>/     (preferred)
/tmp/roost-ssh-<host_id>-<pid>-<seq>/        (fallback, if $TMPDIR doesn't fit)
```

`<host_id>` is the saved host's own id, `<pid>` the client process's, `<seq>` a per-process counter — the combination is unique per attempt, so a double Connect, a disconnect racing its own reconnect, or a superseded establish landing late each own a directory nothing else is touching. Each holds a generated `ssh_config`, the `ssh` `ControlMaster`'s control socket (`ctl`), and the local bridge's own listening socket (`bridge.sock`, `0600`, the address a client dials to reach the remote session). The directory (`0700`) and everything in it are removed on disconnect (`ssh -O exit` first, then the directory) or reclaimed by the next connect attempt's sweep; a one-shot `host add --verify` / Add Host dialog probe against an SSH target uses the same candidates under a `roost-ssh-verify-<pid>-<nanos>-<n>` name instead, which a sweep never mistakes for a host's own leftovers.

`ROOST_SSH_BIN` overrides the `ssh` binary these directories' invocations exec (default: `ssh` on `PATH`) — see [`cli.md`](cli.md#environment).

### Two single-instance locks

A running UI holds **two** flocks, because the two things a single
instance must own move independently:

* `<socket dir>/roost.lock` — the **socket/bind lock**. Guards the
  probe→unlink→bind sequence and the bound socket's lifetime. Follows
  `XDG_RUNTIME_DIR` (macOS: `~/Library/Caches/<app_label>/`).
* `<state dir>/state.lock` — the **state lock**. Guards `state.json`.
  Follows `ROOST_STATE_DIR` / `XDG_DATA_HOME`.

Neither is legacy. One lock beside the socket let two processes with the
same `ROOST_STATE_DIR` and different runtime dirs both write one
`state.json`; one lock beside `state.json` would let two processes with
different state dirs both bind one socket, the second unlinking the
first's. Acquisition order is socket first, then state, and release is
the reverse — mixed orders would let two starting processes refuse each
other.

The filenames differ on purpose: `state_dir` can equal the socket's
directory (the HOME-less `/tmp/<app_label>` fallback, or a
`ROOST_STATE_DIR` aimed at the runtime dir), and one shared name would
make the two locks one file — `flock` is per-open-file-description, so
the app would contend with itself and refuse to start. When both paths
do resolve to one file, acquisition degrades to a single lock.

Contention on the socket lock activates the running window and exits 0.
Contention on the **state** lock refuses to start with a message naming
the holder PID and the state lock path: taking the socket lock first
proves nothing is listening on our socket, so the holder is on a
different runtime dir and there is no window to activate.

Cross-version exclusion holds only when the runtime path agrees — a new
binary cannot contend with an older one whose `XDG_RUNTIME_DIR` differs,
because it has no way to discover that path.

### No migration from pre-rewrite lowercase paths

Pre-rewrite builds stored their state under lowercase `~/Library/Application Support/roost/` and `~/Library/Caches/roost/`. The current builds use capital `Roost`. There is no auto-migration — state in the lowercase directories is intentionally orphaned, and a pre-rewrite build's SQLite database is not migrated into `state.json`. Start empty.

## Config keys

`config.conf` is a tiny `key = value` file (no sections, no nesting). Lines starting with `#` are comments. Missing file → built-in defaults; unknown keys are ignored. Keybindings use Ghostty's `keybind = trigger=action` syntax — see [Keybindings](../getting-started/keybindings.md#custom-keybindings) for the full action list. The full reference (including the `copy-on-select` semantics) lives in [`config.md`](config.md).

Keys use Ghostty-style hyphens (`font-family`, not `font_family`); a misspelled key is silently ignored.

| Key              | Default                              | Effect                                                 |
|------------------|--------------------------------------|--------------------------------------------------------|
| `font-family`    | system monospace (macOS) / `JetBrains Mono, Monospace` (Linux) | Terminal cell font. See [Fonts](fonts.md) for how each UI resolves it. |
| `font-size`      | `13` (Linux) / `14` (macOS)          | Points.                                                |
| `theme`          | `roost-dark`                         | Bundled color theme name. See [Themes](themes.md).     |
| `keybind`        | (built-in defaults; see Keybindings) | Repeatable. `<trigger> = <action>`; later lines override. |
| `command`        | (none)                               | Repeatable. A command-launcher entry (`Cmd/Alt+Shift+T`). See [Command launcher](#command-launcher) below. |
| `copy-on-select` | `true`                               | `off` / `true` / `clipboard`. Controls what a mouse-drag selection writes on release. See [`config.md`](config.md#copy-on-select) for per-platform behavior. |

Tab-strip pill widths (`tab-min-width` / `tab-max-width`, macOS) are documented in [Tab Strip](tab-strip.md#config-keys).

An unresolvable `font-family` degrades to the platform's system monospace rather than failing to launch; [Fonts](fonts.md#how-font-family-resolves) has the per-UI resolution rules.

Example `config.conf`:

```conf
font-family = "JetBrains Mono"
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
| `ROOST_SSH_BIN` | Override the `ssh` binary a host's SSH transport execs (default: `ssh` on `PATH`) — see [SSH scratch directories](#ssh-scratch-directories) above. Read by both `roostctl host add --verify` and the UI's own tunnel. |

`roostctl` also honours `ROOST_BUNDLE_PROFILE=mac|linux|iced` (the env form of
`--target`; an unrecognized value is a hard error, not a fallback). With no
explicit selector it probes every distinct socket, chooses the only live UI, or
names the live candidates when selection is ambiguous. The candidates are the
three profiles' sockets in `mac`, `linux`, `iced` order, deduplicated by path —
so macOS probes three and Linux probes two, where `mac` and `linux` collapse
onto the same production socket.

## Resetting state

To wipe Roost's persistent state and start fresh:

```bash
# macOS — Mac profile (Swift Roost.app)
rm "$HOME/Library/Application Support/Roost/state.json"

# macOS — Iced profile (Roost-Iced.app, or cargo run -p roost-iced)
rm "$HOME/Library/Application Support/Roost-iced/state.json"

# Linux (uses XDG_DATA_HOME with the spec-default fallback)
rm "${XDG_DATA_HOME:-$HOME/.local/share}/roost/state.json"

# Linux — Iced dev build
rm "${XDG_DATA_HOME:-$HOME/.local/share}/roost-iced/state.json"
```

`state.json` is the UI-owned persistent store. Relaunch the UI — it will recreate default state on first run.
