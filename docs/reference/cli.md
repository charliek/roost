# `roostctl`

Shell-integration CLI for the running Roost UI. Talks JSON over
a Unix-domain socket directly to the UI process — no daemon.
Intended to be invoked from inside a Roost tab (typically by
Claude Code hooks) but works from any shell that can reach the
socket. See [`docs/reference/ipc.md`](ipc.md) for the wire
format.

Crate: `crates/roost-cli` (binary `roostctl`).

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
| `doctor` | Read-only diagnosis of the Roost integration (target, socket, shell, tab, Claude hooks) |

`--socket` overrides `ROOST_SOCKET`; one of the two must resolve to the running UI's socket.

### Where `roostctl` lives

`roostctl` ships next to each UI, but the two platforms put it in
different places — only one is on `PATH`:

| Platform | Path | On `PATH`? |
|---|---|---|
| **Linux (`.deb`)** | `/usr/bin/roostctl` | ✅ yes |
| **macOS (`.dmg`)** | `Roost.app/Contents/Resources/bin/roostctl` (inside the bundle) | ❌ no — a Finder-launched app gets a minimal `PATH` |

For your own shell on macOS, symlink it onto `PATH` once
(`ln -s /Applications/Roost.app/Contents/Resources/bin/roostctl
/usr/local/bin/roostctl`). **Provider scripts don't need to** — Roost
sets `ROOST_ROOSTCTL` to the absolute path of its own `roostctl` when it
runs them, so `"${ROOST_ROOSTCTL:-roostctl}"` is portable across both
platforms. See [Extending Roost](../guides/extending.md#opening-tabs-from-activate).

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

```text
socket=/Users/charliek/Library/Caches/Roost/roost.sock
pid=14138
active_project=1
active_tab=5
ui_version=0.1.0
proto_version=1
```

Prints `key=value` lines, not JSON. Useful for verifying the socket is reachable and the env vars are wired correctly.

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
roostctl tab open --project-id 1 -- htop                       # run a command in the tab
roostctl tab open --project-id 1 --hold -- make test           # keep the tab open after it exits
roostctl tab open --project-id 1 --after-tab 5 --focus -- vim   # next to tab 5, then focus it
roostctl tab close --tab 5
roostctl tab send --tab 5 --bytes 'ls -la\n'
roostctl tab resize --tab 5 --cols 120 --rows 40
roostctl tab reorder --project-id 1 --order 3,5,7
roostctl tab dump --tab 5          # the visible viewport as text
roostctl tab dump --tab 5 --json   # full result: dims + cursor + rows
```

`tab open` prints the new tab id on stdout (so `id=$(roostctl tab open …)`). A **command** can follow `--`; without one the tab opens the default shell. The command's working directory is `--cwd` (default: the project's cwd).

| Flag | Effect |
|---|---|
| `-- <cmd…>` | Run this command in the tab. The tab **closes when the command exits** (hold=false) — standard terminal behavior. |
| `--hold` | Keep the tab open after the command exits, dropping to an interactive shell (mirrors `command = … hold=true`). Only meaningful with a command. |
| `--after-tab <id>` | Place the new tab immediately after that tab (same project) instead of at the end. Best-effort: if that tab is gone by the time the reorder lands, the new tab stays at the end. |
| `--focus` | Focus (activate) the new tab after opening. |

These compose: `--after-tab X --focus -- <cmd>` is the "open a command in a tab right here and switch to it" primitive that providers and other scripts use. (`--after-tab`/`--focus` are CLI orchestration over `tab.reorder` / `tab.focus`; `-- <cmd>` fills the `tab.open` op's `argv` — see [ipc.md](ipc.md).)

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

## `doctor`

A **read-only** diagnostic for the Roost integration — it reports and
links, it never repairs, installs, or mutates anything, so it's safe to
run blind whenever something looks broken (a dot that won't light, hooks
that don't fire, a stale socket).

```bash
roostctl doctor
roostctl doctor --tab 7
roostctl doctor --json
```

| Flag | Type | Default | Description |
|---|---|---|---|
| `--tab` | int | `$ROOST_TAB_ID` / the UI's active tab | Inspect this tab instead. Read directly from the env var rather than clap's `env = "ROOST_TAB_ID"`, so an unparsable `$ROOST_TAB_ID` becomes a diagnostic (`env.tab_id: fail`) instead of a silent clap exit 2 |
| `--json` | flag | `false` | Machine-readable report |

The report is five sections. Each declares one of three scopes: **process**
(the shell/process that invoked doctor), **ui** (the Roost instance doctor
reached), **tab** (the selected tab) — a *process* fact is never used to
judge a *tab* fact unless the selected tab is doctor's own tab.

| Section | Scope | Answers |
|---|---|---|
| `env` | process | Are `ROOST_TAB_ID` / `ROOST_SOCKET` set, and valid? |
| `ui` | ui | Which UI did target resolution pick, is its socket reachable, does `identify` succeed, does its version match `roostctl`'s, and does it speak the current agent-state wire format? |
| `shell` | process (`shell.marks_observed` is tab-scoped) | Is the shell-integration contract offered, can this shell/version emit the OSC 133 marks that drive the running dot, and has a mark actually been observed on doctor's own tab? |
| `tab` | tab | The selected tab's four agent axes (shell state, agent lifecycle, attention, ownership), the state derived from them, and whether raw OSC 9/99/777 is currently suppressed. Observation only — nothing in this section fails. |
| `claude` | process (`claude.observed` is tab-scoped) | Is `claude` on `PATH`, does the hook settings file parse and register all six lifecycle events, does each hook command resolve to a runnable `roostctl`, has this tab actually seen a Claude hook fire? |

Every check carries a stable `id` (`env.tab_id`, `ui.socket`,
`shell.marks_capability`, `claude.hook_command`, …) and one of four
statuses: `ok`, `warn`, `fail`, `info`. `info` is an observation with no
verdict — a resolved path, an axis that doesn't apply here, a section
that degrades because Claude isn't configured. Every `fail`/`warn` prints
a link to the doc page that explains it.

Trimmed real output, pointed at a socket nobody is listening on:

```text
roostctl doctor — roostctl 0.0.15

Environment (process)
  ok    env.tab_id               ROOST_TAB_ID=2254
  ok    env.socket               ROOST_SOCKET=/nonexistent/path.sock

Roost UI (ui)
  fail  ui.socket                /nonexistent/path.sock: missing — no Roost UI has bound this path
                                 → https://charliek.github.io/roost/reference/cli/#environment
  fail  ui.identify              no connection: io error: No such file or directory (os error 2)
                                 → https://charliek.github.io/roost/reference/cli/#identify
  info  ui.agent_model           undetermined — tab.list failed: no connection: io error: No such file or directory (os error 2)

  […]

Claude Code (process)
  ok    claude.binary            2.1.220 (Claude Code)
  fail  claude.hook_events       missing StopFailure; run `roostctl claude install --force`
                                 → https://charliek.github.io/roost/guides/claude-code/#install

8 ok, 0 warn, 4 fail, 14 info — exit 1 (https://charliek.github.io/roost/reference/cli/#exit-codes)
```

`--json` carries the same facts as JSON — a top-level `schema_version`,
an explicit `exit_code` (so a script never has to re-derive "did
anything fail" from the check list), and every check's stable `id` and
`status`. That's the shape to script against; the text output's column
widths are not.

## Environment

| Variable | Effect |
|---|---|
| `ROOST_SOCKET` | Override the UI socket the CLI dials |
| `ROOST_TAB_ID` | Default tab id when `--tab` is not given |
| `ROOST_ROOSTCTL` | Set by the UI for provider scripts: absolute path to its own `roostctl`. Best-effort — may be absent if the UI can't resolve its bundled/sibling CLI, so scripts keep the `"${ROOST_ROOSTCTL:-roostctl}"` fallback (see [Where `roostctl` lives](#where-roostctl-lives)) |
| `ROOST_DEBUG` | If set, `claude-hook` writes failure messages to stderr |

`ROOST_SOCKET` / `ROOST_TAB_ID` are auto-set by the UI when it spawns a tab's shell. Set them by hand only when invoking the CLI from outside a Roost tab (e.g. a CI runner). The UI side also honors `ROOST_CONFIG` (config path) and `ROOST_BUNDLE_PROFILE` (`mac`/`gtk`) — see [Paths & Environment](paths.md).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | RPC error or connection failure |
| 2 | Bad command-line input |

`doctor` exits 1 when **any** check's status is `fail`; `warn` never
affects the exit code. Note that "no Roost UI is running" is itself a
failed check (`ui.socket` / `ui.target`), so `roostctl doctor` exits 1
whenever nothing is listening — that's by design, not a bug: the whole
point of the `ui` section is to fail when there's nothing there.
