# `roostctl`

Shell-integration CLI for the running Roost UI. Talks JSON over
a Unix-domain socket directly to the UI process — no daemon by
default. The one exception is opt-in: `roostctl session` starts,
stops, and inspects a headless `roost-session` daemon (see
[`session` subcommands](#session-subcommands) below).
Intended to be invoked from inside a Roost tab (typically by an
[agent hook](../guides/agents.md)) but works from any shell that can
reach the socket. See [`docs/reference/ipc.md`](ipc.md) for the wire
format.

Crate: `crates/roost-cli` (binary `roostctl`).

## Usage

```text
roostctl [--socket <PATH>] [--target <mac|linux|iced>] [--json] <COMMAND>
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
| `wait` | Block until a tab reaches a state, shows a string, or closes |
| `events` | Print the event stream, one JSON line per event |
| `tab open` / `close` / `send` / `resize` / `reorder` | Tab lifecycle + I/O |
| `project list` / `create` / `ensure` / `rename` / `delete` / `reorder` | Project lifecycle |
| `open` | Find-or-create a project by name, then open a tab in it — one call, atomic on the server |
| `agent ensure` / `set` / `install` / `uninstall` / `status` | Wire Roost's hook entries into the supported agents' own configs |
| `agent-hook <agent>` | Internal: the one hook entrypoint every supported agent invokes |
| `claude install` | Alias of `agent install claude` |
| `claude-hook` | Internal: invoked by Claude on each hook event (kept for settings files an earlier Roost wrote) |
| `doctor` | Read-only diagnosis of the Roost integration (target, socket, shell, tab, agent hooks) |
| `session start` / `stop` / `status` | Start, stop, or inspect the headless `roost-session` daemon |
| `rpc <op> [params]` | Call any IPC op directly by name, bypassing every named verb |

`--socket` overrides `ROOST_SOCKET`; one of the two must resolve to the running UI's socket. A
session is not a UI: `session start|stop|status` address the session profile's own socket
directly and ignore `--target` / `--socket` / `ROOST_BUNDLE_PROFILE` entirely — any other op
reaches a running session only via an explicit `--socket <path>` (see
[`session` subcommands](#session-subcommands)).

### JSON output

`--json` is a global flag: every command accepts it, before or after the
subcommand (`roostctl --json tab list` and `roostctl tab list --json` are
the same call). With it, a command prints exactly one JSON document on
stdout:

| Command | `--json` prints |
|---|---|
| A command backed by one IPC op (`identify`, `tab open`, `tab dump`, `tab send-file`, `project create`, `render-stats`, `palette …`, `host add`/`connect`/`disconnect`, bare `agent set`, …) | That op's result object, verbatim |
| A command that prints nothing without the flag (`notify`, `set-title`, `tab send`, `tab close`, `tab focus`, `tab resize`, `tab set-state`, `tab clear-notification`, `tab reorder`, `project rename`/`delete`/`reorder`, `host remove`) | The op's result object — usually `{}` |
| `tab list`, `project list` | The `tab.list` result |
| `host list`, `host status`, `agent status`, `agent ensure`, `doctor` | The same JSON these printed with their own `--json` before it became global |
| `agent install` / `uninstall` / `set --local` | The outcome `agent ensure --json` prints |
| `wait` | `{"tab_id", "satisfied": {"state", "text", "gone"}, "after_ms"}` once the condition holds — see [`wait`](#wait) |
| `screenshot` | `{"out": "<path>", "bytes": N}` — and it needs `--out`, since the PNG and the JSON would otherwise share stdout |
| `session start` | `{"outcome", "socket", "identity", "launcher_reported_pid"}`, `outcome` being `started` or `already-running` |
| `session stop` | `{"socket", "session_id", "reap"}` — `session_id` and `reap` are `null` when nothing was running |
| `session status` | `{"socket", "identity", "projects", "tabs"}` |
| `agent-hook`, `claude-hook` | Nothing different: they ignore the flag and always answer `{}` and exit 0, because a decision hook must never be blocked by a CLI error |
| `project ensure` | `{"project", "created"}` — always, `--json` or not; see [`project ensure`](#project-subcommands) |
| `open` | `{"project", "tab", "created"}` — always, `--json` or not: an agent verb, not a human-typed one; see [`open`](#open) |
| `rpc` | Not affected by the flag at all — see [`rpc`](#rpc) below |
| `events` | Not affected either: always one JSON line per event — see [`events`](#events) |

Without `--json`, every command prints what it always has. An error is
never written to stdout either way — see [Exit codes](#exit-codes).

### Which tab a command acts on

A command that **changes** a tab — `notify`, `set-title`, `tab set-state`,
`tab clear-notification`, `tab close`, `tab send`, `tab resize`,
`tab focus` — acts on `--tab`, or else on `$ROOST_TAB_ID`. With neither
it refuses, before it dials anything, and exits 2:

```text
roostctl: usage: no --tab and ROOST_TAB_ID is unset; refusing to guess the active tab for a command that changes it — `roostctl tab list` shows ids
```

Every shell inside a Roost tab has `ROOST_TAB_ID` set to its own tab, so
the bare forms in the examples below work exactly as written there.
Outside a tab — a CI runner, a second terminal, an agent driving Roost
from elsewhere — pass `--tab`; the UI's active tab is whatever a person
last clicked, which is no answer for a command that writes to a tab.

The read-only `tab dump` and `wait` still fall back to the UI's active
tab when `ROOST_TAB_ID` is unset too. `tab send-file` has always required `--tab`.
`events --tab` is a filter, not a target, so it reads neither: without the
flag every event prints.
A `ROOST_TAB_ID` that is not a tab id is refused the same way (exit 2).

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
roostctl notify --title "Build done" --body "tests pass"   # inside a Roost tab
roostctl notify --tab 3 --title "From CI" --body "deploy ready"
```

The first form relies on `ROOST_TAB_ID`, which only a shell inside a Roost tab has; anywhere else pass `--tab` ([why](#which-tab-a-command-acts-on)).

| Flag | Type | Default | Description |
|---|---|---|---|
| `--title` | string | required | Notification title |
| `--body` | string | empty | Notification body |
| `--tab` | int | `$ROOST_TAB_ID` | Target tab id; exits 2 when neither is set |

## `set-title`

Set a tab's display title. Persists across restarts and locks the tab against subsequent OSC 1/2 escapes from the shell.

```bash
roostctl set-title --title "build-watcher"   # inside a Roost tab
roostctl set-title --title "deploy" --tab 3
```

The first form relies on `ROOST_TAB_ID`, which only a shell inside a Roost tab has; anywhere else pass `--tab` ([why](#which-tab-a-command-acts-on)).

| Flag | Type | Default | Description |
|---|---|---|---|
| `--title` | string | required | New tab title |
| `--tab` | int | `$ROOST_TAB_ID` | Target tab id; exits 2 when neither is set |

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
app_id=ai.stridelabs.Roost
```

Prints `key=value` lines; `--json` prints the full `identify` result instead. Useful for verifying the socket is reachable and the env vars are wired correctly.

## `tab focus`

```bash
roostctl tab focus               # focus the calling shell's tab (inside a Roost tab)
roostctl tab focus --tab 7
roostctl tab focus --tab h3.7    # a tab on connected host 3 (host sessions)
```

The bare form relies on `ROOST_TAB_ID`, which only a shell inside a Roost tab has; anywhere else pass `--tab` ([why](#which-tab-a-command-acts-on)).

Raises the window, switches the active project, selects the tab. Used as the click-through target for desktop banners.

`--tab` also accepts the host-qualified `h<host>.<id>` spelling (see [Host Sessions](../guides/host-sessions.md)) on a UI target — selects that host's tab in the sidebar rather than reporting a local previous selection. Against a bare session socket the qualified form is `invalid-param`: a session has no UI to hold a selection.

## `tab list`

```bash
roostctl tab list
roostctl tab list --json
```

Default output is a human-readable tree; `--json` prints the raw response. Each tab carries `id`, `title`, `agent_state`, `has_notification`, and `is_active`.

## `tab set-state`

```bash
roostctl tab set-state --state running   # inside a Roost tab
roostctl tab set-state --tab 3 --state idle
```

The first form relies on `ROOST_TAB_ID`, which only a shell inside a Roost tab has; anywhere else pass `--tab` ([why](#which-tab-a-command-acts-on)).

| Flag | Type | Default | Description |
|---|---|---|---|
| `--state` | string | required | One of `none`, `running`, `needs_input`, `idle` |
| `--tab` | int | `$ROOST_TAB_ID` | Target tab id; exits 2 when neither is set |

## `tab open` / `close` / `send` / `resize` / `reorder` / `dump`

Tab lifecycle and I/O for automation. `tab close`, `tab send` and `tab resize` act on `--tab` or `$ROOST_TAB_ID` and exit 2 with neither; `tab dump` falls back to the UI's active tab ([why](#which-tab-a-command-acts-on)). `tab send` needs an existing live PTY (a UI must have already attached); errors with `NotFound` otherwise. `--bytes` accepts Rust string-escape sequences (`\n`, `\r`, `\x1b`, …); pass `--raw` to disable escape decoding.

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
roostctl tab dump --tab 5 --scrollback 200   # 200 rows of history, then the viewport
```

`tab open` prints the new tab id on stdout (so `id=$(roostctl tab open …)`); `--json` prints the `tab.open` result instead. A **command** can follow `--`; without one the tab opens the default shell. The command's working directory is `--cwd` (default: the project's cwd).

| Flag | Effect |
|---|---|
| `-- <cmd…>` | Run this command in the tab. The tab **closes when the command exits** (hold=false) — standard terminal behavior. |
| `--hold` | Keep the tab open after the command exits, dropping to an interactive shell (mirrors `command = … hold=true`). Only meaningful with a command. |
| `--after-tab <id>` | Place the new tab immediately after that tab (same project) instead of at the end. Best-effort: if that tab is gone by the time the reorder lands, the new tab stays at the end. |
| `--focus` | Focus (activate) the new tab after opening. |

These compose: `--after-tab X --focus -- <cmd>` is the "open a command in a tab right here and switch to it" primitive that providers and other scripts use. (`--after-tab`/`--focus` are CLI orchestration over `tab.reorder` / `tab.focus`; `-- <cmd>` fills the `tab.open` op's `argv` — see [ipc.md](ipc.md).)

**`tab send` needs no credential.** A write needs none on a UI socket (`--target mac|linux|iced`) or a **host session's own socket** (reached with `--socket <path>`, e.g. one a `roostctl session start` daemon owns) alike: same-UID access to the socket is the whole boundary, and there is no authority layer above it to present anything to. There is deliberately no `--lease` flag and never was one worth adding back — argv leaks through shell history and `ps`, and there is nothing on this wire a flag like that could still mean.

`tab dump` reads the tab's live terminal viewport as text — the determinism backbone for tests: assert on exact content instead of matching pixels. Plain output is one line per visible row (trailing blanks trimmed); `--json` adds dimensions and cursor. Backed by the `tab.dump` IPC op — see [ipc.md](ipc.md).

**`--scrollback <N>`** widens the read to `N` rows of history above the viewport. In plain mode the history rows print **first, with no separator**, so `roostctl tab dump --scrollback 200 --tab 5 | grep …` keeps working over the larger window rather than needing a new parse; `--json` carries them as `scrollback_text` beside a `scrollback_rows` count of everything that was available. The rows are anchored on the viewport that was dumped, so the last history row is always the one directly above the first visible row, even on a tab the user has scrolled up. Asking for more than exists — or more than the server's 10 000-row cap — is **clamped, not refused**: `--scrollback 1000000` is a legitimate way to say "all of it".

Omitting the flag sends the identical request bytes roostctl always sent, so nothing changes against an older Roost. **Using** it against one does not degrade quietly, though: `tab.dump`'s params are strict, so a UI or session that predates the field answers `unknown-field` and the whole request fails rather than returning a viewport-only result you might mistake for an empty history. Against a host session there is no capability list to consult: protocol 5 deleted `session.identify.features`, and the session integer now carries the whole contract — a session this Roost will talk to at all serves the field.

`--tab` on `dump` (and the `tab.dump_resolved` op, plus the test-only `tab.capture_pty_input`) accepts the same host-qualified `h<host>.<id>` spelling `tab focus` does, reading an attached host tab's **client-side** terminal — the UI's own copy, distinct from the session's own `tab dump` served over its socket. See [Host Sessions](../guides/host-sessions.md).

## `tab send-file`

```bash
roostctl tab send-file --tab h2.7 ~/Desktop/shot.png
roostctl tab send-file --tab h2.7 shot.png notes.pdf --json
roostctl tab send-file --tab 5 ~/Desktop/shot.png       # a local tab
```

Send local files to a tab. On a **host** tab each file is uploaded to
that host and the host paths are pasted, so an agent running there can
actually read them; on a local tab it pastes the escaped local paths.
Exactly what dropping the files onto the tab does — same route, same
gesture queue — backed by the [`tab.send_file`](ipc.md#tabsend_file) op.

**Linux/iced only.** The Swift `Roost.app` does not serve this op and
answers `unknown-op`; it has no host sessions to upload to, and its
local drop route is unchanged. See [the Swift fold-in
note](ipc.md#tabsend_file).

| Flag | Effect |
|---|---|
| `--tab <id>` | **Required.** Bare engine id, or `h<host>.<id>` for a connected host's tab. |
| `<path>…` | At least one. Canonicalized in **roostctl's own** working directory before sending. |
| `--json` | Print the whole result — `pasted`, `uploads`, `skipped` — instead of just the pasted text. |

`--tab` is required here, and unlike the other commands that change a
tab it does **not** read `ROOST_TAB_ID` either: the id in a shell's
environment is a local tab, which is never the host tab a caller sending
files means, and guessing it would paste a remote-looking gesture into a
local shell. Paths are canonicalized on this side because the shell you typed
them in is the only cwd that can resolve a relative one — the op itself
refuses relative paths for exactly that reason — so a path that is not
there fails locally, naming it (`cannot send shot.png: …`), rather than
after a round trip.

Default output is the pasted text on stdout — the host paths for a host
tab, the escaped local paths for a local one. Files the gesture could
not send (a directory, something unreadable, a file over the 10 MiB
per-file cap) are reported in `--json`'s `skipped` and do not stop the
rest; a request where *every* path was skipped is an error naming each
one.

**It blocks, and prints nothing until it is done.** The command returns
when the paste has been queued in the tab, which for a 10 MiB file over
a slow link is minutes. That is deliberate: there is no progress channel
on this wire, and the pasted text is the only thing worth printing. The
wait is bounded — 60 s plus 300 s per file, multiplied by
`ROOST_TEST_TIMEOUT_SCALE` like every other budget — because `IpcClient`
has no request timeout of its own and a wedged UI would otherwise hang
the caller forever. On expiry it says so and warns that the paste may
still land.

## `wait`

Block until a tab reaches a condition, then exit `0` — the no-`sleep`
synchronization primitive for scripts, tests and agents. At least one
condition is required; when several are given, all must hold.

```bash
roostctl wait --tab 5 --state idle            # until the agent state is idle
roostctl wait --tab 5 --text 'BUILD OK'       # until the viewport contains a string
roostctl wait --tab 5 --gone                  # until the tab is closed
roostctl wait --tab 5 --state idle --no-timeout --json
```

| Flag | Type | Default | Description |
|---|---|---|---|
| `--state` | string | — | Wait until the tab's agent state equals this (`none`/`running`/`needs_input`/`idle`) |
| `--text` | string | — | Wait until the viewport (via `tab.dump`) contains this substring. Pick a needle from command *output*, not the echoed command |
| `--gone` | flag | `false` | Wait until the tab no longer exists. Cannot be combined with `--state` or `--text` |
| `--timeout` | float | `5.0` | Give up after this many seconds. `0` checks once: exit `0` if the condition already holds, `4` if not |
| `--no-timeout` | flag | `false` | Wait for as long as it takes. Cannot be combined with `--timeout` (exit 2) |
| `--interval-ms` | int | `100` | The poll interval where there is no stream, and how often `--text` re-reads the viewport where there is |
| `--tab` | bare id | `$ROOST_TAB_ID`, then the UI's active tab | Target tab. A host tab (`h<host>.<id>`) is refused with `usage` (exit 2): the stream is the local workspace's |

**Where it listens.** `wait` asks the UI's `identify` which socket owns
the tabs, and follows that socket's [event stream](ipc.md#eventssubscribe)
instead of polling:

| `identify` says | `wait` reads |
|---|---|
| `local_session_socket` is present (`local-backend = session`) | The local session: both connections dial that socket |
| `ops` names `events.subscribe` (the iced UI, in-process) | The UI socket itself |
| Neither — the Swift Mac app, or a Roost older than this | The poll loop: `tab.list` (and `tab.dump` for `--text`) every `--interval-ms`, as `wait` always did |

On a stream it subscribes on one connection, then — on a second — checks
that `identify.instance_id` (or, on a session,
`session.identify.session_id`) is the one the subscription acked, and only
then takes a `tab.list` snapshot. Batches the snapshot already holds are
discarded; a snapshot older than the subscription fences nothing and counts
as a lost stream. `--state` and `--gone` are then decided by
`tab.state_changed` and `tab.closed` as they arrive, with no `tab.dump` at
all. No event carries a tab's output, so `--text` re-reads the viewport on
every event for the tab and every `--interval-ms`. A bare `--tab` id means
the same tab on the UI socket and on the session under
`local-backend = session`, so the id (or the active tab) is resolved on the
UI and used on the session unchanged.

**When the stream goes away.** A stream that closes without a label, skips a
revision, or ends with `stream.ended`, or whose second connection drops —
while the subscription is still being opened, too — is resolved again from
`identify` once, after any local-backend switch in flight has settled. The
wait carries on **only if the new stream is the same process** — the same
`instance_id` or `session_id` its first subscription acked — as it is for a
subscriber the server cut for falling behind. Anything else exits 1
`connection`: "the Roost serving tab N changed … tab ids do not carry across
— re-resolve the tab and wait again". Tab ids belong to the process that
minted them: a switch replays the tabs onto its destination under new ids,
and a restart mints new ones, so tab N over there is some other tab or none,
and `--gone` read off its snapshot would be wrong. `stream.ended` is a
switch, so a wait across one normally ends that way. A second loss exits 1
`connection` too, and so does a resolve that fails — no Roost found, or a
server that refuses the subscribe — with that failure's own code and message
inside the `connection` one. A session that stops (`session.stopping`) exits
1 `connection` at once, with that reason. If the two connections of one
subscription keep reaching different processes (the server restarting
between them), the sequence is retried three times and then exits 1
`connection`.

**Exits.** `0` once the condition holds; **4** (`timeout`) if `--timeout`
elapses first, including while a backend switch in flight holds the wait off
(re-checked every `--interval-ms`, never past the deadline) — before plan
066 it exited 1, which a script could not tell from a failed connection; 2
`usage` for a bad command line; 1 for everything in [Exit
codes](#exit-codes).

With `--json`, success prints:

```json
{"tab_id": "5", "satisfied": {"state": "idle", "text": null, "gone": null}, "after_ms": 812}
```

Each `satisfied` field is `null` unless its flag was given: `state` is the
state the tab was seen in, `text` the needle that was found, `gone` is
`true`. `after_ms` counts from the command's start, dialling included.

## `events`

Print the event stream as a line protocol: one compact JSON document per
line, flushed as it arrives, until the stream ends.

```bash
roostctl events                       # every event
roostctl events --tab 5               # only the events that name tab 5
roostctl events | jq -c 'select(.event == "tab.state_changed")'
```

Each line is one [event envelope](ipc.md#events) with the revision of the
commit it rode in:

```json
{"event":"tab.state_changed","data":{"tab_id":"5","state":"idle"},"revision":42}
```

Events of one commit share a `revision` and keep their order; a commit
with no events prints nothing. The stream's last line is its terminal
envelope, which carries no `revision`:
`{"event":"session.stopping","data":{"reason":"stop"}}` from a session, or
`{"event":"stream.ended","data":{"reason":"backend-switch"}}` from a UI whose
local backend is switching.

| Flag | Default | Description |
|---|---|---|
| `--tab` | — | Keep only the events that name this tab — by `data.tab_id` (for `active.changed`, the tab it made active), `data.tab.id` for `tab.opened`, or any of `data.tab_ids` for `tabs.reordered` — and drop the rest, event by event: a commit touching several tabs keeps just this tab's lines. Events that name no tab (`project.*`, `projects.reordered`, `workspace.durability_changed`) are dropped. The terminal envelope always prints. A bare id only; a host tab is refused with `usage`. Not read from `ROOST_TAB_ID` |

It reads the same socket [`wait`](#wait) does — the in-process UI, or the
local session under `local-backend = session` — with the same identity check,
and no snapshot. There is no poll fallback.

| Exit | When |
|---|---|
| 0 | The terminal envelope arrived (printed as the last line), or the reader of stdout went away (`events \| head -n1`) |
| 1 `connection` | The stream closed without a label or skipped a revision — nothing is resolved again; run it again |
| 1 *the server's own* | The server does not serve the stream: the Swift Mac app, an older Roost. Its refusal is passed through verbatim (`not-implemented`, `unknown-op`) |
| 2 `usage` | A bad command line, including a host `--tab` |

`events` is always JSON, with or without `--json`; a failure still goes
through the one [error envelope](#exit-codes).

## `project` subcommands

```bash
roostctl project list
roostctl project create --name "scratch" --cwd ~
roostctl project ensure --name "scratch"                 # --cwd defaults to $PWD
roostctl project ensure --name "scratch" --cwd ~/scratch
roostctl project rename --project-id 1 --name "main"
roostctl project delete --project-id 2
roostctl project reorder --order 1,3,2
```

`project delete` cascades to the project's tabs. `project reorder` is the same shape as `tab reorder` — any id not in `--order` keeps its prior position.

`project ensure` finds the project with this **exact** name, or creates
it at `--cwd` (defaulting to `$PWD`) if none exists — atomic on the
server, so two callers racing the same name converge on one project
(#221) rather than each creating their own — and never activates it.
Always prints `{"project", "created"}` (the `project.ensure` wire
result), **with or without `--json`**: `project create` is the only
other `project` verb with a one-line human form, and
`rename`/`delete`/`reorder` print nothing without the flag, so there is
no single convention `ensure` could join instead.

Refuses `unsupported` (exit 1) against a server whose `identify --json`
`ops` field doesn't list `project.ensure` — the Swift Mac app today.
The refusal names the manual route — `project list --json` to find an
existing project by name, `project create` to make one — rather than
falling back to it itself: a list-then-create fallback inside `ensure`
would reopen the very race (#221) it exists to close.

## `open`

```bash
roostctl open --project "review" --cwd "$PWD" -- bash -lc 'make test'
roostctl open --project "review" --title "review" --focus -- vim
roostctl open --project "review" --hold -- make test   # keep the tab open after it exits
```

The one-shot agent verb: `project.ensure` (find-or-create, by exact name),
then `tab.open` in the ensured project. It replaces composing those two
calls by hand, and the older `project list`-then-`project create` recipe.
Each call is atomic on the server, so unlike that recipe it cannot race
another caller into a second project of the same name (#221). `--cwd`
defaults to `$PWD` and is used for **both** calls: the project (if this call
creates it) and the new tab. Without `-- cmd…` the tab opens the default
shell, same as [`tab open`](#tab-open-close-send-resize-reorder-dump);
`--hold` and `--focus` compose exactly as they do there.

Always prints `{"project", "tab", "created"}` — the ensured project, the
opened tab, and whether the project was just created — **with or
without `--json`**: this is an agent verb, not a human-typed one.

**Refusals:**

| Exit | `code` | When |
|---|---|---|
| 1 | `unsupported` | This server's `identify --json` `ops` doesn't list `project.ensure` (the Mac app today) — names the manual route (`project list --json` + `tab open --project-id`) rather than racing it itself (#221) |
| 2 | `usage` | `identify --json`'s `local_backend_switch` shows a backend switch in progress — the project set is mid-flight, so a name resolved against it may not hold; retry once it settles |
| 1 | *the server's own* | `tab.open`'s own refusal, verbatim — most notably `not-found` if the ensured project vanished between the two calls (a backend switch landing in that window, say). **The two calls are not atomic with each other**, only each is atomic on the server, so this is possible even though `ensure` itself never races another `ensure` |

## `screenshot`

Capture a PNG of the running UI's whole window (sidebar + tab bar + active terminal), rendered **in-process** by the UI itself. Because it re-draws the view tree rather than grabbing screen pixels, it needs no screen-recording permission and works even when the window is unfocused, behind other windows, or offscreen — handy for confirming a UI change without OS screen capture.

```bash
roostctl screenshot --out shot.png        # write a file
roostctl screenshot --scale 2 --out shot.png   # 2x super-sampled
roostctl screenshot > shot.png            # raw PNG bytes to stdout
```

`--scale` is `1` (default, logical window size) or `2`. With `--out` the CLI writes the file and prints the dimensions + byte count to stderr; without it, the raw PNG bytes go to stdout (nothing else is printed, so the stream stays binary-clean). `--json` requires `--out` and prints `{"out": "<path>", "bytes": N}` on stdout instead of the stderr line. Backed by the `app.screenshot` IPC op — see [ipc.md](ipc.md).

## `render-stats`

Read the running UI's render-path counters — the only way to measure the real draw path, since `TerminalWidget::draw` needs a live renderer no unit test can construct.

```bash
roostctl render-stats             # running totals since process start
roostctl render-stats --reset     # read, then zero for a clean next delta
```

Prints one counter per line plus two derived averages (`ns_per_refresh`, `ns_per_draw`, shown as `-` when the matching call count is zero). `--reset` zeroes the counters **after** the read, so the read-reset / run workload / read pattern gives you a delta directly.

`refresh_*` covers the snapshot rebuild that walks libghostty's render state (with `rows_rebuilt` / `cells_walked` measuring what it touched); `draw_*` and `fill_text_calls` cover the widget draw pass. Note that `roostctl screenshot` re-renders the window and so inflates the draw counters — read before capturing, or reset after. Both Rust UIs report real numbers, and `fill_text_calls` counts sprite-rendered cells (box drawing, blocks) as glyph draws on both, so it is comparable across them. Backed by the `app.render_stats` IPC op — see [ipc.md](ipc.md).

## `palette` subcommands

Drive the command-palette overlay: open it, inspect its rows, filter, activate a row, dismiss. Activating a row runs the **same** command its keybind would (a command row's id is its keybind action), so this is a command-dispatch surface, not just a UI poke. Each subcommand prints the resulting palette state (a `>` marks the highlighted row); `--json` emits the structured result.

```bash
roostctl palette open                      # the command palette
roostctl palette open --kind launcher      # the command launcher
roostctl palette open --kind custom        # the script-backed provider palette
roostctl palette open --kind agents        # the agent-jump palette
roostctl palette state                     # current rows / filter / selection
roostctl palette query theme               # set the filter
roostctl palette activate new_tab          # confirm the row (runs its command)
roostctl palette dismiss
```

`--kind` is `commands` (default), `launcher`, `custom`, or `agents`; any other value errors `invalid-param`. `palette activate <id>` errors `not-found` if no palette is open or no visible row has that id. Backed by the `palette.*` IPC ops — see [ipc.md](ipc.md).

## `agent` subcommands

Wire Roost's hook entries into each supported agent's own configuration
file, and take them back out. Every verb but one reads and writes
dotfiles directly and works with nothing running; bare `agent set` is
the exception — it dials the running UI, because only the UI can reach
the hosts this machine is connected to.

```bash
roostctl agent status                        # per agent: installed, wired, up to date
roostctl agent ensure [--json] [--startup]    # reconcile to `agent-hooks`; --startup never removes
roostctl agent set claude,codex [--json]      # via the running UI: set the key here, raise every connected host
roostctl agent set claude,codex --local       # set `agent-hooks` to exactly this list here, and wire it
roostctl agent set off --local                # set `agent-hooks = off`, and unwire everything
roostctl agent install claude                 # or --all; explicit wins over `agent-hooks = off`
roostctl agent uninstall claude               # or --all
```

`ensure` reads the `agent-hooks` key from `config.conf`
([`config.md`](config.md#agent-hooks)) — the same value the UIs act on
at startup, through the same parser (`roost-cli` depends on
`roost-ui-model` for exactly this) — and reconciles this machine's files
to match: wires what the key allows and **takes out what it does not**.
Bare `agent ensure` is that full reconcile; `--startup` is the narrower
shape a UI launch runs — wire and refresh what the key names, but never
remove anything, because a launch must not undo a hook someone added by
hand or react to a key that changed while the app was closed. An
unanswered key (nobody has run the consent dialog or `agent set` yet) is
a no-op for either shape: nothing is wired, and nothing already wired is
touched.

`set <list|off>` answers the consent question from a terminal: the
argument is a comma list of agent names (`claude`, `codex`, `grok`,
`cursor`, `opencode`) or the literal `off`, and an unrecognised name
refuses the whole list rather than narrowing it silently.

Bare `set` puts [`agent.set_hooks`](ipc.md#agentset_hooks) to the
running UI, which writes this machine's `agent-hooks` key, reconciles
its files, and raises every **connected non-localhost** host to at least
the same list in the same call — so a headless box and a desk box agree
without either editing the other's dotfiles. It prints where the key
landed, what changed here, and one line per host (`--json` prints the
op's reply verbatim), and exits non-zero if anything failed here or a
host could not be asked.

`set <list|off> --local` writes the key and reconciles this machine's
files directly instead, the same way `ensure` does — with nothing
running, and reaching no host at all, so a connected host's live session
does not pick up the change until it reconnects.

`install` and `uninstall` take an agent name or `--all`, and
deliberately ignore `agent-hooks = off` — an explicit verb always wins.
Both also *move* the key rather than leaving it alone: `install`
unions the named agent(s) into it (so the next `ensure` does not treat
what you just asked for as unconsented), and `uninstall` narrows it back
out, spelling the result `off` when nothing would be left allowed.
`status` changes nothing on disk; each row names the agent, whether its
config directory is present, whether the resolved `agent-hooks` key
allows it, whether Roost's entries are actually wired on disk, and
whether they're at the current integration version.

See the [Agent Hooks](../guides/agents.md) guide for the mechanism, the
wire format each agent gets, and the guarantee about what a merge does
and doesn't touch.

## `agent-hook`

Internal: the one hook entrypoint every supported agent's installed
command invokes — `roostctl agent-hook <agent>`, where `<agent>` is
`claude`, `codex`, `grok`, `cursor`, or `opencode` (gx reports through
`grok`; there's no separate name for it). Reads the agent's JSON event
payload from stdin, takes the event name from the payload itself
(`hook_event_name` — there is no `--event` flag, since one installed
command string has to serve every event), dials `ROOST_SOCKET` for the
tab named by `ROOST_TAB_ID`, and translates the event into a
`tab.agent_report`. **Always** exits 0 with `{}` on stdout, whatever
happens — an agent Roost has no adapter for, a malformed payload, an
unreachable socket, all drain stdin and answer the same inert `{}`,
because Claude's and codex's `PermissionRequest` are *decision* hooks
whose dialog blocks on this process, and anything else may be read as a
block. Unlike `claude-hook` below, it never falls back to the default
bundle-profile socket when `--socket`/`ROOST_SOCKET` is absent — that
could otherwise deliver an event into an unrelated running Roost.

## `claude install`

An alias of `roostctl agent install claude`, kept so the command
existing scripts already run keeps working. It no longer writes
`~/.config/roost/claude-settings.json` and no longer prints a shell
alias snippet — wiring goes into Claude's own `~/.claude/settings.json`,
and the installed command finds `roostctl` through `$ROOST_AGENT_HOOK`
rather than a baked-in absolute path.

```bash
roostctl claude install
```

If you followed the old instructions, `roostctl doctor`'s
`agent.claude.legacy_settings` check will tell you what is left over and
how to remove it. There is no `--force`; it exits 2.

## `claude-hook`

A thin alias of `agent-hook claude`, kept so a `~/.claude/settings.json`
an earlier Roost wrote (or a hand-installed one following
[`docs/development/claude-testing.md`](../development/claude-testing.md))
keeps working. It accepts the canonical event names plus the older
kebab-case ones (`session-start`, `prompt-submit`, `notification`,
`stop`, `session-end`) an early Roost wrote. Unlike `agent-hook`, it
falls back to the ordinary target resolver (`--target` /
`ROOST_BUNDLE_PROFILE` / auto-detect) when no explicit socket is given,
matching the by-hand invocation that doc describes. Reads the hook
payload from stdin, looks up `$ROOST_TAB_ID`, and translates lifecycle
events into a `tab.agent_report`. Always exits 0 with `{}` on stdout
(Claude treats nonzero hooks as failures). Silently no-ops when run
outside a Roost tab.

## `session` subcommands

Start, stop, and inspect the headless `roost-session` daemon (HS-1a,
plan 035) — a workspace + PTY supervisor with no UI attached, for
host-sessions (a workspace left running on a machine with nobody
watching, e.g. a remote host). See [`ipc.md`](ipc.md#session-sockets)
for the socket posture (same-UID check, `0700`/`0600`) and the session
ops these verbs drive.

```bash
roostctl session start
roostctl session status
roostctl session stop
```

These three verbs are a deliberate carve-out: a session is not a UI, so
they never go through `--target` / `ROOST_BUNDLE_PROFILE` / auto-detect
— they resolve the `Session` bundle profile's socket directly, and
`start` has to work when nothing is listening at all. Any other op
(`tab.list`, `tab.open`, …) reaches a running session only through an
explicit `roostctl --socket <path> <op>` pointed at that same socket.

`session start` spawns `roost-session start`, which daemonizes and
seeds its first project from the calling shell's cwd on a fresh state
file. It then polls `session.identify` on the socket before returning,
so exit 0 means **a session answered**, not merely that a process was
launched — both a fresh start and confirming an already-running session
print their identity and exit 0; either verdict that never gets
confirmed exits 1. The daemon binary is located next to `roostctl`
first, then on `PATH`; `ROOST_SESSION_BIN` overrides the search
outright (used by tests and a from-source `cargo run`).

`session stop` calls [`session.stop`](ipc.md#sessionstop) and prints
the reap report, then polls until the socket is actually gone before
exiting — stopping something that is not already running is a success
(`systemctl stop` style), so `session stop` always exits 0 short of a
genuine fault reaching the socket.

`session status` prints the session's identity and tab count. When no
session is listening it prints `not running (no session at …)`, reports
`not-running` on stderr and exits **3**, matching `systemctl status`'s
convention so a script can branch on the code alone without parsing
output; a socket that exists but will not answer is a real fault and
exits 1 (`connection`), not 3.

| Verb | Exit 0 | Exit 1 | Exit 3 |
|---|---|---|---|
| `session start` | a session confirmed serving (fresh or already-running) | spawn failed, or no session answered `session.identify` within the confirm window | — |
| `session stop` | stopped, or already not running | a socket exists but never answered, or the reap timed out | — |
| `session status` | a session answered; identity printed | a socket exists but would not answer | no session is running |

## `host` subcommands

Manage the **client-side** saved-host registry for [host sessions](../guides/host-sessions.md) — `add`, `list`, `remove`, `connect`, `disconnect`, `status`. Unlike `session`, a saved host is UI state (`Workspace.hosts` in the running UI's own `state.json`), not a session daemon's workspace — so every verb here addresses the ordinary UI socket (`--target` / auto-detect / `ROOST_BUNDLE_PROFILE`, same as `tab`/`project`), never `--socket` against a session. Each verb drives the matching `host.*` IPC op one-to-one, so the CLI can never diverge from what a palette click does. See [ipc.md](ipc.md) for the wire.

```bash
roostctl host add --label pop-os --target /home/charlie/.local/state/roost/roost-session.sock
roostctl host add --label pop-os --target ~/.roost-session.sock --verify
roostctl host list
roostctl host list --json
roostctl host remove --id 3f9a2b7c1d4e4f5a
roostctl host connect --id 3f9a2b7c1d4e4f5a
roostctl host disconnect --id 3f9a2b7c1d4e4f5a
roostctl host status
roostctl host status --id 3f9a2b7c1d4e4f5a --json
```

`--target` accepts an SSH destination (`workbox`, `user@host`, `ssh://user@host:port` — only the `ssh://` spelling carries an explicit port; `ssh://[::1]:22` for a literal IPv6 host), a Unix socket path (anything containing `/`), or `localhost` — see [`roost_ipc::ssh::classify`](https://github.com/charliek/roost/blob/main/crates/roost-ipc/src/ssh.rs) for the full rule table. `host add` is **registry-only by default**: it saves `--label`/`--target` without dialing anything, so a typo'd target still saves cleanly — the sidebar's connection dot is what reports it at the next connect attempt. `--verify` additionally dials `session.identify` against `--target` first (through the same target-resolution `roost_ipc::ssh::verify_transport` and the Add Host dialog's own "Add & Connect" both use) and refuses to save on an unreachable or incompatible session — the CLI equivalent of the dialog's validation, stated once so the two bars cannot drift apart. Over an SSH target this is a **mux-less probe**: a one-shot `ssh` exec outside any `ControlMaster` (`ControlMaster=no`, no `-S`), so a stale or wedged control socket from a previous attempt can never make the probe report a false positive, and nothing is left running afterward, win or lose.

`host connect` starts a new, symmetric connection alongside any this client or another already has open — reconnecting to an already-connected host claims nothing from anybody — and, on a `localhost` target, spawns the session first if nothing is listening. It returns once the attempt is under way, not once it settles — watch the sidebar or poll `host status` for the connected/needs-restart outcome. `host disconnect` never stops the session; its shells keep running and a later connect picks them back up.

`host list` prints one line per saved host — `id  label  target=…  state=…  last_connected=…`. The `state=` column is a **best-effort second call** to `host.status` after the registry read: a target that answers `unknown-op` (a session socket, the Swift app) or errors for any other reason prints `state=?` for every row rather than failing the listing. `host list --json` is deliberately the registry alone, unchanged — a script that wants connection state calls the op that owns it.

`host status` is that op. It reports, for every saved host (or just `--id`), the state the sidebar's band is drawn from: `generation` (which attempt the host is on — the monotonic edge to poll against), `state`, `reason` (the band's untruncated input), `rollup` (the band's own output), and `retry` (an armed auto-reconnect's `delay_ms`/`attempt`/`budget`/`armed_at`, absent when nothing is scheduled, plus `retry.reason` — the classified failure that armed the rung, ssh-only, read it whenever it is present rather than gating on `attempt`). A **live** connection additionally carries `connect` (`session_id`, `reduced_fidelity`, `resumed`, `from_revision?` — plan 056 §3.1/§3.2, R14/#447 and R11/#442) and `tabs` (how many rows the sidebar section is currently listing, `0` included, always present). `--json` prints the op's result verbatim and is what scripts and the functional harness assert on; the human form is `id  label  state  rollup`, followed by the host's `detail` on indented lines when there is one — a settled launch failure's full text, which the band has no room for. `retry`'s `delay_ms`/`attempt`/`budget`/`armed_at` stay **`--json`-only**, but `retry.reason` does not: while a rung is armed the band, and so the human form's `rollup`, has to read `reconnecting in 8s (3/10)`, which is exactly the moment the cause would otherwise be unreadable, so the human form prints `retry.reason` too, on its own indented line right after the `reason`/`detail` lines and before the reduced-fidelity line: `    armed because: <reason>`. Also `--json`-only are `payload_kind` and `connect`/`tabs`: `payload_kind` names what the last attach on that host is being decoded as — `vt` there means the connection is on the [reduced-fidelity fallback](../guides/host-sessions.md#when-a-build-skew-connects-instead-the-vt-fallback) a libghostty build skew lands on — but it appears only once a tab has actually attached, while `connect.reduced_fidelity` is known the moment the connection is up. That is what the human form prints instead, keyed on `connect.reduced_fidelity` rather than `payload_kind` for exactly that reason: whenever it is `true`, an indented line follows the `reason`/`detail` lines: `    reduced fidelity: this Roost and the session pin different libghostty builds; update or restart the session from the sidebar`. See [ipc.md](ipc.md) for the field-by-field wire shape.

There is no `roostctl host stop`: the palette's **Stop Session** verb goes straight onto the host's own connection as an ordinary `session.stop`, not through a client-side `host.*` op, so there is nothing yet for a CLI verb to drive. The fidelity fix-it verbs (`host:update:<id>` on an ssh host, `host:restart:<id>` on `localhost`) are the same story: **UI-only, deliberately, with no `roostctl` twin.** Plan 039 §3.5's rule is that a machine is never prompted by a connect it did not ask for; these verbs exist only on a person's already-connected, reduced-fidelity host, and their first remote activity is a consent card (ssh) or a restart confirmation (`localhost`) — exactly the shape Stop Session already set. No new IPC op backs either one.

**Swift Mac app note:** `roostctl host *` against `--target mac` answers `unknown-op` — a documented, permanent boundary, not a gap. Host sessions are iced-only (the Linux `roost` build and the experimental Roost-Iced Mac app); the Swift `Roost.app` never grows this surface.

## `rpc`

Call any IPC op directly, by name — the escape hatch for an op that has
no verb of its own yet, or for a script that would rather speak the wire
format straight through.

```bash
roostctl rpc tab.list
roostctl rpc tab.write '{"tab_id":"4","data":"bHM="}'
echo '{"tab_id":"4","data":"bHM="}' | roostctl rpc tab.write -   # params from stdin
```

| Argument | Description |
|---|---|
| `<op>` | The op name, passed through **verbatim** — no local validation against the ops this build knows. An op the server does not serve answers the server's own `unknown-op`, passed through the same way. |
| `[params]` | The op's params as a JSON **object** literal, or `-` to read one from stdin. Omitted ⇒ `{}`. Anything that is not valid JSON, or that parses to something other than an object (an array, a string, a number), is refused with `usage` (exit 2) **before** the socket is dialled. |

See [ipc.md](ipc.md#operations) for op names and their params, and
`roostctl identify --json`'s `ops` field for exactly which ops the Roost
this socket reaches serves right now.

`rpc` is always JSON, with or without `--json` on the command line: the
op's result object, pretty-printed on stdout — the same shape every
other IPC-backed verb prints under `--json`. A failure still goes
through the one [`CliError`](#exit-codes) envelope every verb uses:
`roostctl: <code>: <message>` on stderr normally, or
`{"error":{"code","message"}}` when `--json` is also given.

**It bypasses the tab policy.** `rpc` is not in the list of verbs
[Which tab a command acts on](#which-tab-a-command-acts-on) governs, and
it does not read `ROOST_TAB_ID` either — the caller writes whatever ids
`params` needs directly, so there is no `--tab` for this verb to
resolve. That also means `rpc` is **not a way around `--tab`**: a verb
that needs `--tab` or `ROOST_TAB_ID` to guard a mutation (`tab close`,
`tab send`, …) stays that named verb: `rpc tab.write` with a hand-picked
`tab_id` skips none of that verb's own checks, it just skips the CLI's
tab-resolution convenience.

## `doctor`

A **read-only** diagnostic for the Roost integration — it reports and
links, it never repairs, installs, or mutates anything, so it's safe to
run blind whenever something looks broken (a dot that won't light, hooks
that don't fire, a stale socket).

```bash
roostctl doctor
roostctl doctor --tab 7
roostctl doctor --json
roostctl doctor -v
roostctl doctor --color=always
```

| Flag | Type | Default | Description |
|---|---|---|---|
| `--tab` | int | `$ROOST_TAB_ID` / the UI's active tab | Inspect this tab instead. Doctor reads the env var itself rather than through the parse every other `--tab` command shares, so an unparsable `$ROOST_TAB_ID` becomes a diagnostic (`env.tab_id: fail`) instead of exit 2 `usage` |
| `--json` | flag | `false` | Machine-readable report (the global flag; see [JSON output](#json-output)) |
| `-v` / `--verbose` | flag | `false` | Print the full per-check report — all 39 entries with details and doc links — instead of the one-line-per-section summary. Ignored by `--json`, which always carries everything |
| `--color` | `auto` \| `always` \| `never` | `auto` | Colorize the text output. `auto` enables color only when stdout is a TTY, `NO_COLOR` is unset **or empty**, and `TERM` is not `dumb`; `always` bypasses all three checks; `never` always disables. Per <https://no-color.org/>, `NO_COLOR=` (present but empty) does **not** disable — only a non-empty value does. Ignored by `--json` |

The report is six sections. Each declares one of three scopes: **process**
(the shell/process that invoked doctor), **ui** (the Roost instance doctor
reached), **tab** (the selected tab) — a *process* fact is never used to
judge a *tab* fact unless the selected tab is doctor's own tab.

| Section | Scope | Answers |
|---|---|---|
| `env` | process | Are `ROOST_TAB_ID` / `ROOST_SOCKET` set, and valid? |
| `ui` | ui | Which UI did target resolution pick, is its socket reachable, does `identify` succeed, does its version match `roostctl`'s, and does it speak the current agent-state wire format? |
| `shell` | process (`shell.marks_observed` is tab-scoped) | Is the shell-integration contract offered, can this shell/version emit the OSC 133 marks that drive the running dot, and has a mark actually been observed on doctor's own tab? |
| `tab` | tab | Which tab is selected — the one check in the section, and it can fail if a `--tab`/`$ROOST_TAB_ID` no longer exists — then, for that tab, its four agent axes (shell state, agent lifecycle, attention, ownership), the state derived from them, and whether raw OSC 9/99/777 is currently suppressed. Those six are observations, not verdicts. |
| `claude` | process (`claude.observed` is tab-scoped) | Is `claude` on `PATH`, does `~/.claude/settings.json` parse and register every lifecycle event Roost maps, does each hook command resolve to a runnable `roostctl`, has this tab actually seen a Claude hook fire? Pre-046 checks, kept for the settings-file shape they've always covered — see the `agents` section below for the other four agents and for Claude's newer, agent-hooks-specific facts |
| `agents` | process (the `owning` checks are tab-scoped) | Is `$ROOST_AGENT_HOOK` set and executable from this tab; per agent (claude, codex, grok, cursor, opencode), is Roost's hook entry present and at the current integration version, and does that source currently own a tab on this UI; for codex, does its `trusted_hash` on disk match what codex would compute; is a legacy `~/.config/roost/claude-settings.json` or shell alias still lying around? See the [Agent Hooks](../guides/agents.md) guide |

Every entry carries a stable `id` (`env.tab_id`, `ui.socket`,
`shell.marks_capability`, `claude.hook_command`, …) and a `kind`: `check`
or `observation`.

**Checks** carry a verdict — one of `ok`, `warn`, `fail`, `skipped`.
`skipped` is not a fourth verdict so much as the *absence* of one: the
check's subject is absent (not inside a Roost tab, Claude not
configured) or doctor genuinely cannot tell (a shell whose family or
version it can't identify).

**Observations** carry no verdict at all — `status` is `null` — because
they report a fact with no correct value: the selected tab's four agent
axes (plus the state derived from them and whether raw OSC 9/99/777 is
suppressed), `ROOST_SOCKET`, and the current shell. The exception worth
naming: those same six `tab.*` axes carry `skipped` rather than `null`
when the UI predates the agent state model, because then they genuinely
cannot be observed — `tab.ownership: null` ("nothing owns it") and
`tab.ownership: "skipped"` ("can't tell") are different findings, and a
`--json` consumer shouldn't have to parse prose to know which.

Every `fail`/`warn` still prints a link to the doc page that explains
it; `skipped` entries and observations do not — there's nothing to go
read about a fact.

Real output, captured on a dev checkout with `ROOST_TAB_ID` unset and
`--target mac` pinning the target at a stale Roost.app socket left over
from an earlier crash (so the example doesn't depend on whatever else
happens to be running on the capture machine — an ordinary `roostctl
doctor` with nothing running looks the same, just with `ui.target`
saying `auto-detect` instead of `--target`). This is the default
view — one line per section, clipped to a fixed width so it stays
scannable in a narrow terminal:

```text
roostctl doctor — roostctl 0.0.19 (run `roostctl doctor -v` for the full report)

[–] Environment         not running inside a Roost tab
[✗] Roost UI            /Users/charliek/Library/Caches/Roost/roost.sock: stale — the socket file ou…
[–] Shell integration   not running inside a Roost tab
[–] Selected tab        no tab selected — pass --tab, set ROOST_TAB_ID, or give the UI an active tab
[–] Claude Code         the selected tab's ownership is unavailable (no tab.list from a running UI)
[–] Agents              not running inside a Roost tab

• 2 issues found — exit 1 (https://charliek.github.io/roost/reference/cli/#exit-codes):
    ✗ ui.socket    → https://charliek.github.io/roost/reference/cli/#environment
    ✗ ui.identify  → https://charliek.github.io/roost/reference/cli/#identify
```

Notice `Claude Code` and `Agents` read `skipped`/`–`, not `fail`, even
though no UI is reachable: everything in the `agents` section that
doesn't need a live tab (`wired`, `trust`, `legacy_settings`) is read
straight off the agents' own config files on disk, so it still has
something to say with nothing running — only the tab-scoped `owning`
checks and `agent.hook_binary` (which needs to be inside a tab at all)
come back `skipped` here.

`-v` prints all 39 entries grouped by section, with the status column
blank for `null`-status observations (not for `skipped` — that word
still prints, because it *is* a status) and a doc link under every
`fail`/`warn`. Same capture, `-v`, trimmed with `[…]` to the sections
that show every row shape — the elided one is "Shell integration",
all `skipped` here because nothing is inside a Roost tab:

```text
roostctl doctor — roostctl 0.0.19

Environment (process)
  skipped env.tab_id               not running inside a Roost tab
          env.socket               unset — target resolution falls through to --socket / --target / auto-detect

Roost UI (ui)
  ok      ui.target                --target → /Users/charliek/Library/Caches/Roost/roost.sock (auto-detect would try: /Users/charliek/Library/Caches/Roost/roost.sock, /Users/charliek/Library/Caches/Roost-linux/roost.sock, /Users/charliek/Library/Caches/Roost-iced/roost.sock)
  fail    ui.socket                /Users/charliek/Library/Caches/Roost/roost.sock: stale — the socket file outlived its listener; Roost crashed or was killed
                                   → https://charliek.github.io/roost/reference/cli/#environment
  fail    ui.identify              no connection: io error: Connection refused (os error 61)
                                   → https://charliek.github.io/roost/reference/cli/#identify
  skipped ui.version               roostctl 0.0.19 — no UI reached, nothing to compare
  skipped ui.agent_model           undetermined — tab.list failed: no connection: io error: Connection refused (os error 61)

  […]

Selected tab (tab)
  skipped tab.selection            no tab selected — pass --tab, set ROOST_TAB_ID, or give the UI an active tab
  skipped tab.shell_state          unavailable (no tab.list from a running UI)
  skipped tab.agent_lifecycle      unavailable (no tab.list from a running UI)
  skipped tab.attention            unavailable (no tab.list from a running UI)
  skipped tab.ownership            unavailable (no tab.list from a running UI)
  skipped tab.derived              unavailable (no tab.list from a running UI)
  skipped tab.raw_osc              unavailable (no tab.list from a running UI)

Claude Code (process)
  ok      claude.binary            2.1.261 (Claude Code)
  ok      claude.settings          /Users/charliek/.claude/settings.json parses
  ok      claude.hook_events       all 11 events registered
  ok      claude.hook_command      all 11 events have a current Roost command
  skipped claude.observed          the selected tab's ownership is unavailable (no tab.list from a running UI)

Agents (process)
  skipped agent.hook_binary        not running inside a Roost tab
  ok      agent.claude.wired       wired@v3
  skipped agent.claude.owning      unavailable (no tab.list from a running UI)
  ok      agent.claude.legacy_settings no leftover ~/.config/roost/claude-settings.json or shell alias found
  ok      agent.codex.wired        wired@v3
  ok      agent.codex.trust        8 trusted hashes match what codex would compute
  skipped agent.codex.owning       unavailable (no tab.list from a running UI)
  ok      agent.grok.wired         wired@v3
  skipped agent.grok.owning        unavailable (no tab.list from a running UI)
  ok      agent.cursor.wired       wired@v3
  skipped agent.cursor.owning      unavailable (no tab.list from a running UI)
  ok      agent.opencode.wired     wired@v3
  skipped agent.opencode.owning    unavailable (no tab.list from a running UI)

• 2 issues found — exit 1 (https://charliek.github.io/roost/reference/cli/#exit-codes):
    ✗ ui.socket    → https://charliek.github.io/roost/reference/cli/#environment
    ✗ ui.identify  → https://charliek.github.io/roost/reference/cli/#identify
```

Paths are this machine's own (`/Users/charliek/...`) — expect your own
absolute paths, not these. The five `agent.*.wired`/`agent.codex.trust`
`ok`s above reflect this dev checkout's own machine already having run
`roostctl agent ensure` for real — an agent whose config directory
doesn't exist at all shows `skipped: not installed` instead, and a
present-but-never-wired agent shows `warn: not wired` rather than `ok`.

`--json` carries the same facts as JSON — a top-level `schema_version: 2`,
an explicit `exit_code` (so a script never has to re-derive "did
anything fail" from the check list), and every entry's stable `id`,
`kind` (`check` or `observation`), and `status` — `null` for
observations, one of `ok`/`warn`/`fail`/`skipped` for checks. That's the
shape to script against; the text output's column widths are not.
`--json` is also unaffected by `-v` and `--color`: it always carries all
39 entries and never contains a color escape, regardless of either
flag.

## Environment

| Variable | Effect |
|---|---|
| `ROOST_SOCKET` | Override the UI socket the CLI dials |
| `ROOST_TAB_ID` | Default tab id when `--tab` is not given. A command that changes a tab refuses without one of the two ([why](#which-tab-a-command-acts-on)) |
| `ROOST_ROOSTCTL` | Set by the UI for provider scripts: absolute path to its own `roostctl`. Best-effort — may be absent if the UI can't resolve its bundled/sibling CLI, so scripts keep the `"${ROOST_ROOSTCTL:-roostctl}"` fallback (see [Where `roostctl` lives](#where-roostctl-lives)) |
| `ROOST_AGENT_HOOK` | Set by the UI (or `roost-session`) on every tab: absolute path of the `roostctl` (or `roost-session`) that understands `agent-hook <agent>`. Every hook entry Roost installs into an agent's config reads this indirectly through a shell fallback rather than calling `roostctl` directly, which is what keeps the installed command host-independent — see [Agent Hooks](../guides/agents.md#inert-outside-roost). Resolved the same sibling → bundled-`Resources/bin` → `PATH` ladder as `ROOST_ROOSTCTL`; omitted, like that variable, when nothing resolves |
| `ROOST_DEBUG` | If set, `claude-hook` and `agent-hook` write failure messages to stderr |
| `ROOST_AGENT_HOOKS_FORCE` | Set to `1` to let `agent ensure`/`install`/`uninstall` write real dotfiles while `ROOST_TEST_MODE=1` is also set — without it, the agent-hooks install engine refuses to run at all under `ROOST_TEST_MODE=1`, belt and braces against a test writing into a real machine's dotfiles |
| `ROOST_SESSION_BIN` | Overrides where `session start` looks for the `roost-session` binary (default: next to `roostctl`, then `PATH`) |
| `ROOST_SSH_BIN` | Overrides the `ssh` binary a host's SSH transport execs (default: `ssh` on `PATH`) — read by `host add --verify` against an SSH target and by the UI's own tunnel. See [`paths.md`](paths.md#ssh-scratch-directories). |

`ROOST_SOCKET` / `ROOST_TAB_ID` / `ROOST_AGENT_HOOK` are auto-set by the UI when it spawns a tab's shell. Set them by hand only when invoking the CLI from outside a Roost tab (e.g. a CI runner). The UI side also honors `ROOST_CONFIG` (config path) and `ROOST_BUNDLE_PROFILE` (`mac` / `linux` / `iced`) — see [Paths & Environment](paths.md). `roostctl` reads `ROOST_BUNDLE_PROFILE` too, as the env-var form of `--target`; an unrecognized value is a hard error there ("unknown ROOST_BUNDLE_PROFILE value … expected `mac`, `linux`, or `iced`") rather than the UI's warn-and-fall-back.

## Exit codes

Every failure prints one error line on stderr, never on stdout:

```text
roostctl: <code>: <message>
```

A command line clap refuses keeps clap's usage text after that line, and
a command given no subcommand at all (`roostctl tab`) prints its help
instead. `doctor` and `session status` still print their report on
stdout when they fail.

With `--json` (read off the raw command line even when parsing itself
failed), the line is a JSON envelope instead:

```json
{"error":{"code":"usage","message":"no --tab and ROOST_TAB_ID is unset; …"}}
```

`code` is the stable part to branch on; `message` is for people.

| Exit | `code` | Meaning |
|---|---|---|
| 0 | — | Success |
| 2 | `usage` | A bad command line, including a command that changes a tab given no `--tab` and no `ROOST_TAB_ID`. A parser error keeps clap's usage text in the message; `--help` and `--version` print to stdout and exit 0 |
| 1 | `no-target` | Auto-detect found nothing listening at any known socket |
| 1 | `ambiguous-target` | Several Roost UIs are running; pass `--target` |
| 1 | `connection` | Dialing, reading or writing the socket failed, or the stream dropped (`wait` after a second loss or a stopping session; `events` on any drop or gap) |
| 1 | *the server's own* | The server refused the request; its code is passed through verbatim (`not-found`, `invalid-param`, `unknown-op`, …) |
| 1 | `unsupported` | This server does not serve an op the command needs |
| 1 | `checks-failed` | `doctor` found a failing check; the report itself is on stdout |
| 1 | `failed` | Something on this machine outside the wire: a file that could not be written, a binary that could not be found, an unset `$HOME` |
| 3 | `not-running` | `session status` found no session running |
| 4 | `timeout` | `wait`'s condition did not hold before `--timeout` |

A command can also exit 1 **without** an error line when it has a
partial outcome to report: `agent install`/`uninstall`/`ensure`/`set`
print what they did on stdout and exit 1 if any agent (or, for `agent
set`, any connected host) failed.

`doctor` exits 1 when **any** check's status is `fail`; `warn` never
affects the exit code, and neither does `skipped` — a check that
couldn't be judged is not a check that failed. Observations don't enter
into it either, since they carry no status to fail with. Note that "no
Roost UI is running" is itself a failed check (`ui.socket` / `ui.target`),
so `roostctl doctor` exits 1 whenever nothing is listening — that's by
design, not a bug: the whole point of the `ui` section is to fail when
there's nothing there.
