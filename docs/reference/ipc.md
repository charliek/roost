# JSON IPC

`roostctl` and Claude hooks drive the running Roost UI through a small
newline-delimited JSON protocol over a Unix-domain stream socket. The protocol
is local-only — there is no network deployment.

The UI binary (Swift `Roost.app`, or the Rust/iced binary — `roost` as
installed on Linux, `roost-iced` from a dev tree) is the IPC server.
`roostctl` is the only first-party client; the contract here is what any
other automation should implement.

The socket path is the bundle profile's `socket_path` (see
[`paths.md`](paths.md)):

* Mac (Swift `Roost.app`):    `~/Library/Caches/Roost/roost.sock`
* Iced on Mac (`Roost-Iced.app` or a dev build): `~/Library/Caches/Roost-iced/roost.sock`
* Installed `roost` on Linux (XDG): `$XDG_RUNTIME_DIR/roost/roost.sock`
* Iced dev build on Linux (XDG):    `$XDG_RUNTIME_DIR/roost-iced/roost.sock`
* Linux fallback:             `/tmp/roost[-iced]-<uid>/roost.sock`

`roostctl --target mac|linux|iced` selects a profile explicitly. Without an
explicit selector, `roostctl` probes every distinct profile socket; if more
than one is live, it reports the actual candidates and requires selection.
(The `linux` profile also resolves on macOS, as `~/Library/Caches/Roost-linux/`,
but nothing ships or launches it there.)

A fifth socket path, `Session` (`~/Library/Caches/RoostSession/roost.sock`
on macOS, `$XDG_RUNTIME_DIR/roost-session/roost.sock` on Linux), is served
by the headless `roost-session` daemon (HS-1a, plan 035) — see
[Session sockets](#session-sockets) and [`paths.md`](paths.md#session-profile).
It is **not** a `roostctl --target` value and `roostctl` never auto-probes
it: `roostctl session start|stop|status` address the session profile's
socket directly (a pre-connect carve-out, since `start` must work when
nothing is listening yet), and any other op reaches a session only
through an explicit `--socket`. A UI socket answers `unknown-op` for
every `session.*` op and for [`tab.attach`](#tabattach), and
`not-implemented` for `events.subscribe`, byte-identical to before
`roost-session` existed.

A session socket also carries a second, **binary** protocol on its own
connections — the per-tab attach stream a client renders a remote
terminal from. It shares the socket path but not the framing; see
[Data plane](#data-plane).

## Wire format

* **Framing:** newline-delimited JSON. One JSON object per line.
  Max line length: **16 MiB**. Lines longer than that are rejected with
  `frame-too-large`. Embedded `\n` inside JSON strings is the encoder's
  responsibility (`serde_json` and `JSONEncoder` both handle this
  correctly).
* **Request envelope:**
  `{"id": "<string>", "op": "<dotted-name>", "params": {...}}`. The
  `id` is a **string-wrapped 64-bit integer**, because JSON numbers
  lose precision past 2^53; the legacy proto schema used `int64` for
  tab/project ids and we preserve that range. Rust uses
  `#[serde(with = "string_int64")]`; Swift's `Codable` uses a custom
  encoder that emits `String(describing: int64)`.
* **Response envelope (success):**
  `{"id": "<string>", "ok": true, "result": {...}}`.
* **Response envelope (error):**
  `{"id": "<string>", "ok": false, "error": {"code": "<kebab>", "message": "<string>"}}`.
* **Event envelope** (server-push, unsolicited):
  `{"event": "<dotted-name>", "data": {...}}` — no `id`, no response
  expected. Pushed inside an `EventBatch` on a connection that ran
  [`events.subscribe`](#eventssubscribe), which host-session sockets
  serve and UI sockets do not. The one exception is the terminal
  `session.stopping` control envelope, which rides bare (no batch, no
  revision) as the last frame on the stream. Catalog:
  [Events](#events) below.
* **Bytes payloads** (e.g. `tab.write.data`, and any future binary
  field): **base64-encoded strings** using the standard alphabet,
  no padding stripping. Tested for binary fidelity (`0x00..0xff`
  round-trip) in both directions.
* **Unknown fields:** strict on the **server** side (rejected with
  `unknown-field` error). Permissive on the **client** side (clients
  ignore unknown fields so the server can add fields without breaking
  older clients). Swift's `Codable` is permissive by default and the
  client-side request encoders match that policy unchanged. On Rust,
  `serde` is permissive by default — server-side request structs in
  `roost-ipc` carry `#[serde(deny_unknown_fields)]` to opt in to the
  strict server policy; client-side response structs do not, matching
  the client-side permissive policy.
* **Concurrency:** the server is single-actor — every request is
  dispatched onto the UI's main thread (Swift `@MainActor`; in the iced
  UI the handler forwards the request onto the single engine→UI feed and
  awaits a oneshot reply, so it is applied from `update` on the winit
  event-loop thread). Responses are delivered in completion order, which
  is not guaranteed to match request order. Clients correlate by `id`.
* **Schema drift mitigation:** `tests/ipc-vectors/*.json` is a directory
  of canonical message exemplars (one file per op/event). Both
  `cargo test -p roost-ipc` (Rust) and Swift's `XCTest` target load
  these vectors and assert decode → re-encode → byte-equal.
* **Errors:** stable kebab-case codes. Current set:
  `unknown-op`, `unknown-field`, `missing-param`, `invalid-param`,
  `parse-error`, `frame-too-large`, `duplicate-id`, `not-found`,
  `not-implemented`, `internal`, `too-large`, `store-full`,
  `shutting-down`, `host-unavailable`.
  Clients should treat unknown codes as fatal for the request and
  surface `message` to the user. `too-large` and `store-full` arrived
  with [`session.put_file`](#sessionput_file) and are both about **one
  file**: it is over the per-upload cap, or the host's file store has
  no room left for it. Two codes rather than one because the remedy
  differs — send something smaller, or restart the session to clear the
  store — and a status line has to say which. `host-unavailable` is
  `shutting-down`'s mirror image — **UI-socket only** — and says a
  host-routed op could not reach the session it named: the connection
  is gone, died mid-op, or its queue is full. A session's own refusal
  crossing to a UI socket keeps its code when that code is one of the
  twelve above that both sockets share; a session-scoped code (including
  `shutting-down`) or one a newer session invents folds onto
  `host-unavailable`, its own code and sentence kept in `message`, so a
  UI socket never answers a code this list does not name. `too-large`
  and `store-full` are deliberately on the shared side of that line
  (`CODES_A_UI_SOCKET_ALSO_SPEAKS`, `app/servicing.rs`): folding them
  would tell a caller the host is gone when the host is right there and
  one file did not fit.
  `shutting-down` is **session-socket only**: once
  [`session.stop`](#sessionstop) latches, every mutating op answers it
  (reads still answer normally), and a second `session.stop` on the
  same session gets it too instead of a fresh reap report. Session
  sockets add a further six: `too-many-tokens`, `unsupported-kind`,
  `build-mismatch` from [`tab.attach`](#tabattach)'s negotiation, and
  `replay-expired`, `revision-ahead`, `session-mismatch` from
  [`events.subscribe`](#eventssubscribe)'s resume. Every same-UID
  connection is otherwise symmetric — there is no per-connection
  authority code, because there is no per-connection authority.

## Shared types

```json
{
  "Tab": {
    "id": "<string-int64>",
    "project_id": "<string-int64>",
    "title": "<string>",
    "cwd": "<string>",
    "state": "<TabState>",
    "has_notification": "<bool>",
    "is_active": "<bool>",
    "user_titled": "<bool>",
    "position": "<int32>",
    "created_at": "<int64-unix-seconds>",
    "last_active": "<int64-unix-seconds>",
    "hook_active": "<bool>",
    "shell_state": "<ShellState>",
    "agent_lifecycle": "<AgentLifecycle>",
    "ownership": "<Ownership, omitted when unowned>"
  },
  "Project": {
    "id": "<string-int64>",
    "name": "<string>",
    "cwd": "<string>",
    "position": "<int32>",
    "created_at": "<int64-unix-seconds>",
    "tabs": ["<Tab>"]
  },
  "Ownership": {
    "source": "<string>",
    "session_id": "<string>",
    "last_event_at": "<int64-unix-seconds>",
    "detail": "<string>",
    "metadata": {"<string>": "<string>"}
  }
}
```

`TabState` is a JSON string with values: `"none"`, `"running"`,
`"needs_input"`, `"idle"`. The legacy `TAB_STATE_UNSPECIFIED` is not
exposed — the server always picks a concrete state.

`ShellState` is a JSON string with values: `"unknown"`, `"at_prompt"`,
`"foreground_process"` — written only by OSC 133 shell marks.
`AgentLifecycle` is a JSON string with values: `"inactive"`,
`"working"`, `"waiting"`, `"finished"`, `"failed"` — written only by
agent adapters through `tab.agent_report` (below). The two axes are
independent: an agent can be `working` while the shell itself sits at
a prompt (e.g. between tool calls). `Tab.ownership` is present only
while some source has claimed the tab (omitted otherwise); it carries
the `(source, session_id)` identity pair, `last_event_at` (the
server's receipt time of the most recently accepted report — never
caller-supplied), a free-form `detail`, and an open `metadata` string
map for forward-compatible extension (see `tab.agent_report`).

### `tab.state` / `hook_active` — derived, and the compatibility contract

`Tab.state` and `Tab.hook_active` are **derived** fields, computed
server-side from `shell_state` + `agent_lifecycle` + `ownership`, not
independently settable:

* `hook_active` = `ownership` is present with a non-empty `source`
  ("is-live").
* `state`: if ownership is live *and* `agent_lifecycle != "inactive"`,
  the agent axis wins; otherwise the shell axis does.

| Effective lifecycle | `state` |
|---|---|
| agent `inactive`, shell `unknown`/`at_prompt` | `none` |
| agent `inactive`, shell `foreground_process` | `running` |
| agent `working` | `running` |
| agent `waiting` | `needs_input` |
| agent `finished` | `idle` |
| agent `failed` | `needs_input` |

**`tab.state` stays a closed four-value enum — it does not gain a
fifth `failed` value.** This is a pinned compatibility constraint, not
an oversight: both Swift decoders (`IPCTabState`, `Workspace.TabState`)
are closed `String` enums with no fallback case, so a fifth wire value
throws a `DecodingError` on the Mac client, and the **Versioning**
section below classifies a new enum value as a breaking protocol
change. `agent_lifecycle: "failed"` therefore projects onto
`state: "needs_input"` on the wire — the closest of the four legacy
values ("this tab wants you"). The true failed state is only
observable on `agent_lifecycle`, which any client that cares (the
sidebar rollup, the per-tab dot color) reads directly instead of
re-deriving it from `state`.

`hook_active` is retained on the wire for the same reason: existing
consumers (`roostctl`, `tools/roosttest/client.py`) read it as a plain
bool and don't need to change. It is derived from `ownership`'s
liveness, and `hook_active.changed` continues to fire on every
ownership claim/release exactly as before.

## Operations

Operation names use dotted lowercase. `params` is omitted when an op
takes no parameters, but the field is permitted as `{}`.

### `identify`

Returns the running UI's identity and active selection.

Request:
```json
{"id": "1", "op": "identify",
 "params": {"client_name": "roostctl", "client_version": "0.6.0"}}
```

`params.client_name` and `params.client_version` are optional and are
logged by the server for debugging. Empty/missing is permitted.

Response:
```json
{"id": "1", "ok": true, "result": {
  "socket_path": "/Users/.../Library/Caches/Roost/roost.sock",
  "pid": 1234,
  "active_project_id": "1",
  "active_tab_id": "3",
  "app_label": "Roost",
  "app_id": "ai.stridelabs.Roost",
  "ui_version": "0.7.0",
  "protocol_version": 1
}}
```

### `tab.open`

Open a new tab in a project. If `project_id` is `"0"` and no projects
exist, the server creates a default project and opens the tab inside
it.

Request:
```json
{"id": "2", "op": "tab.open", "params": {
  "project_id": "1",
  "cwd": "",
  "argv": ["/bin/zsh"],
  "cols": 120,
  "rows": 30,
  "title": ""
}}
```

`argv` empty means `[$SHELL]`. `cwd` empty means resolve it: the
project's cwd, then `$HOME`, then `/`. `title` empty means derive from
the resolved `cwd`. There is
deliberately no opaque command string — callers wanting shell
word-splitting must pass `["sh", "-c", "..."]` explicitly. This `argv` is
reachable from the CLI as `roostctl tab open -- <cmd…>` (see
[cli.md](cli.md)).

Response: `{"tab": <Tab>}`.

### `tab.close`

Close a tab; the PTY child is `SIGHUP`'d and reaped.

Request: `{"params": {"tab_id": "3"}}`. Response: `{}`.

### `tab.list`

Snapshot of the workspace. Same shape as the legacy
`ListTabsResponse`.

Response: `{"projects": [<Project>, ...]}`.

On a **host-session socket** the response also carries
`"revision": <u64>` — the commit the snapshot was taken at, read under
the same lock as the projects. It is the fence a client pairs with
[`events.subscribe`](#eventssubscribe): discard every `EventBatch`
whose `revision` is `<=` this one, apply the rest, and the first batch
it keeps is exactly `revision + 1`. A UI socket omits the key entirely
(not `null`) — it serves no event stream, so there would be nothing to
fence against.

### `tab.write`

Headless write into a tab's PTY. `data` is base64-encoded raw bytes.

Request:
```json
{"id": "4", "op": "tab.write", "params": {
  "tab_id": "3",
  "data": "bHMK"
}}
```

`data` decodes verbatim into the PTY master fd. Binary-clean (the
test suite round-trips `0x00..0xff`). Errors `not-found` if the tab
has no live PTY.

**Raw input is open to every same-UID client, on every socket.** The
socket's UID check is the whole boundary; there is no per-connection
write fence. `TabWriteParams` carries no authority field at all —
`deny_unknown_fields`, so a request that still sends one is refused
with `unknown-field` rather than silently ignored.

Response: `{}`.

### `tab.send_file`

Send local files to a tab. On a **host** tab each one is uploaded to
that host with [`session.put_file`](#sessionput_file) and the host paths
are pasted; on a local tab nothing crosses a boundary and the escaped
local paths are pasted, which is what a file drop has always done. UI
socket only, ungated. The same function the native drop handler calls —
the op *is* the drop, minus the window event.

**Served by the iced UI only.** The Swift `Roost.app` answers
`unknown-op`: it has no host sessions, so there is nothing to upload to,
and this op's whole point is the crossing. (The protocol integer that
moved for this work is `session_protocol`, which governs *session*
sockets; the UI socket's own `protocol_version` is unchanged, so a Mac
UI is not claiming to serve this.) The Mac fold-in is described in the
plan's §3.7 and waits on Mac host sessions.

Request:
```json
{"id": "13", "op": "tab.send_file", "params": {
  "tab": "h2.7",
  "paths": ["/Users/charlie/Desktop/shot.png", "/Users/charlie/build"]
}}
```

Response:
```json
{"pasted": "/home/charlie/.cache/roost-session/files/4b9d1e7f0a3c5e21/shot.png",
 "uploads": [{"source": "/Users/charlie/Desktop/shot.png",
              "name": "shot.png",
              "path": "/home/charlie/.cache/roost-session/files/4b9d1e7f0a3c5e21/shot.png",
              "bytes": 482113}],
 "skipped": [{"path": "/Users/charlie/build", "reason": "directory"}]}
```

`tab` takes the same spelling [`tab.focus`](#tabfocus) does — a bare
engine id, or `h<host>.<id>` for a connected host's tab (DL-20 routing).
`paths` must be non-empty and every entry **absolute**: a relative path
is `invalid-param`, because the only working directory this process
could resolve one against is the *app's*, which is not the one the
caller typed it in. Duplicates are dropped first-seen. The paths are
read in the **UI process's** filesystem namespace.

**`skipped` is not a failure.** Each entry carries the path and one of
five stable `reason` strings — `directory`, `missing`, `unreadable`,
`not-regular`, `over-cap` — and the rest of the batch still goes. The
reasons are the client planner's `SkipReason` serialized
(`roost_ui_model::file_transfer`); they are a `String` on the wire so a
result a client cannot decode at all is never the price of a reason it
has no name for. `not-regular` is what a FIFO, a device node or a
procfs entry gets: `is_file()` is the only kind that uploads, because
everything else has a `len()` that lies.

**The reply lands when the paste has been *queued* client-side** — what
[`tab.capture_pty_input`](#tabcapture_pty_input-test-only-gated) sees.
Not host receipt, and not agent attachment: the data plane has no
acknowledgement and none was added for this. `uploads` is empty for a
local tab. A gesture is **all-or-nothing for the paste**: if one upload
fails nothing is pasted, and the files that already crossed stay on the
host until it is swept.

**Error precedence**, in order:

1. `not-found` — the `tab` ref resolves to no live terminal.
2. `host-unavailable` — the host is stopped, not connected, or it
   disconnected or reconnected mid-gesture; also a tab closed under the
   gesture and an app shutting down. An upload is a same-UID connection
   op like any other — every connected client may send one, so this
   code names only a dead or dying connection, never another client.
   The message names the file where there is one.
3. `invalid-param` — an empty or relative `paths`, or a request where
   **every** path was skipped, in which case the message lists each
   path with its reason (`nothing to send: /tmp/build (directory)`).
4. `too-large` / `store-full` — one file the host would not take.

Be precise about `too-large`: the client preflights against the same
`MAX_PUT_FILE_BYTES` the session enforces, so an ordinary over-cap file
never reaches the wire — it is skipped as `over-cap`, and a request of
nothing but such files answers **`invalid-param`**, not `too-large`.
The host's own `too-large` is reachable only if a file *grows* between
inspection and upload, which the upload lane catches by reading through
`take(cap + 1)`. `too-large` is also the client's own answer to a batch
whose regular files sum past the 256 MiB per-drop limit (`That drop is
540 MiB, over the 256 MiB per-drop limit`) — the same judgement the host
makes per file, made once for the gesture. `store-full` is reachable
normally, and means what it says on
[`session.put_file`](#sessionput_file).

**Every terminal path answers.** The handler replies through a oneshot
the gesture queue owns rather than blocking the winit thread, so the UI
socket keeps serving other frames while a long upload runs, and a
`roostctl` caller can never hang: a refusal, an upload timeout, a
disconnect, a frozen frame, a closed tab and app shutdown all answer. A caller that disconnects first drops its receiver and the
gesture completes anyway.

Note the shape of that concurrency. Frames on **one** connection are
served serially — `roost-ipc`'s server awaits each handler inline per
connection — so a caller that wants to do something else while a
send-file is in flight opens a **second** connection. What the deferred
reply buys is that the UI itself never stalls and other connections stay
served.

**No new security boundary**, stated the way
[`clipboard.write`](#selection-clipboard-test-ops-selection-clipboard)
states its own: the UI now *reads* a local file on behalf of a socket
caller, which is new — but the socket is `0600` inside a `0700`
directory and reachable only by the same user, and `tab.write` /
[`tab.open`](#tabopen) already let that same caller type or run
anything.

See [`cli.md`](cli.md#tab-send-file) for `roostctl tab send-file`, and
the [host-sessions guide](../guides/host-sessions.md#pasting-images-and-files-into-a-host-tab)
for what the user sees.

### `tab.resize`

Headless resize of a tab's PTY (issues `TIOCSWINSZ`, which fires
`SIGWINCH` to the child group).

Request: `{"params": {"tab_id": "3", "cols": 100, "rows": 24}}`.
Response: `{}`.

**A tab is sized by the last geometry-bearing interaction with it**
(plan 057, R15) — the rule that lets two clients at different sizes
share one tab without either permanently shrinking the other. Four
things carry geometry, and each one sizes the tab:

* this op;
* [`tab.attach`](#tabattach) with `focus: true` (the default), which
  resizes during negotiation;
* a data-plane `INPUT` frame, which applies its connection's declared
  geometry ahead of the bytes when it differs — typing is how a client
  says which viewport it is looking at;
* a data-plane `RESIZE` frame, which applies and becomes that
  connection's declared geometry.

Nothing else does. [`tab.dump`](#tabdump), an
[`events.subscribe`](#eventssubscribe) stream, a
[`tab.write`](#tabwrite) (a control-plane write has no viewport behind
it) and an idle client that attached with `focus: false` all leave the
size exactly where it was.

Geometry is the four numbers `(cols, rows, cell_w_px, cell_h_px)`
compared together, not just the grid: libghostty's mode-2048 in-band
size reports quote the pixel dimensions, so the same grid at different
cell metrics is a different viewport. Unchanged geometry is not
re-applied, so two clients typing at the same size cost nothing. This
op states the grid only and leaves the cell metrics where the last
geometry-bearing client left them; before any client has declared them
they are unset, and the size report quotes `0x0` pixels.

Simultaneous interactions **linearize in the order the tab receives
them**, not in wall-clock order: every one of them is a command on the
tab's single channel. The consequence is the rule working as intended —
two clients alternating keystrokes at different sizes flip the PTY's
size each time. The tab tells nobody it was resized under them (see
[Data plane](#data-plane)); a client at another size sees wrapping
until it interacts again.

### `tab.dump`

Read the tab's live terminal *viewport* as text — the determinism
backbone for automated tests (assert on exact content instead of
OCR/pixel-matching a screenshot) — plus however many rows of history
above it the request asks for. Both UIs walk libghostty-vt's render
state on the main thread.

Request: `{"params": {"tab_id": "3"}}`, or with history:
`{"params": {"tab_id": "3", "scrollback": 50}}`.
Response:

```json
{"cols": 120, "rows": 30,
 "cursor": {"row": 1, "col": 14, "visible": true},
 "scrollback_rows": 812,
 "scrollback_text": ["…", "make: nothing to be done", "/tmp $ echo hi"],
 "rows_text": ["/tmp $ echo hi", "hi", "/tmp $", ""]}
```

`rows_text` has one entry per visible row, trailing blanks trimmed (a
blank cell renders as a space so columns line up). `cursor` is omitted
when the cursor is off-viewport. Response is permissive, so per-cell
color fields can be added forward-compatibly. CLI:
`roostctl tab dump --tab N` (plain rows) / `--json` (full result).

**Scrollback (plan 053, #421).** `scrollback` is an optional row count,
`0` (the default) meaning none. It is **omitted from the request when
unset**, so a viewport-only ask stays byte-identical to what clients
have always sent — which matters because `TabDumpParams` is strict: a
server predating the key refuses the whole request with `unknown-field`
rather than ignoring the field. Ask for more than
`MAX_DUMP_SCROLLBACK` (10 000) and the count is **clamped, never
refused**, so a client can ask for "everything" without knowing the
tab's retention; a maximum exists at all because the rows are formatted
synchronously on the thread that owns the terminal.

Two response fields answer it, both `serde(default)` so a client built
against this shape still decodes a pre-053 server's response:

* `scrollback_rows` — how many history rows sit above the **current
  viewport**, always present. This is the anchor for the whole
  contract: it counts rows above what `rows_text` is showing, not above
  the live bottom, so the two halves stay adjacent however far a UI tab
  is scrolled up. A session socket never scrolls, so there it equals
  the whole history; on the alternate screen it is `0`.
* `scrollback_text` — the last `min(scrollback, scrollback_rows)` of
  those rows, top to bottom, omitted when empty. Its final entry is
  **always** the row immediately above `rows_text[0]`, at the bottom,
  scrolled to the middle, or scrolled to the top. Rows are trimmed of
  trailing spaces the same way `rows_text` and a copy of the same
  selection are (libghostty's own trim is deliberately not used — it
  eats a space carrying a combining mark), and blank rows are preserved
  as `""` rather than dropped, so the array is exactly as long as the
  count of rows returned and its numbering never silently shifts.

Both halves describe **one instant**: the iced path refreshes its
render snapshot before reading either, rather than straddling a PTY
chunk. CLI: `roostctl tab dump --tab N --scrollback 50`.

On a **host-session socket** this is answered from the tab's server
Terminal instead of a UI's — same request, same response shape. It
requires no `tab.attach`: a session's terminal is authoritative
whether or not anybody is watching it.

On a **UI socket**, `tab_id` also accepts the host-qualified
`h<host>.<id>` spelling (plan 037 §3.4) — `"h3.7"` reads tab `7` of
whichever connected host this UI process has minted connection id `3`
for. It resolves to the **client-side** hydrated
Terminal of an attached host tab (the same one `TerminalWidget`
paints), not the session's own copy — so this is how a test or
`roostctl` observes the client half of an attach independently of the
server half above. A **session socket** refuses the qualified form
with `invalid-param` ("host-qualified tab refs are a UI-socket form;
session tab ids are bare") rather than silently narrowing it to some
unrelated numbered tab — a session's own ids are one bare id-space by
design. `roostctl tab dump --tab h3.7` passes the spelling straight
through.

### `tab.dump_resolved`

Companion to `tab.dump` — a richer read of the same viewport, but each
cell carries the post-resolver fg/bg the production paint path computes.
Ungated; useful both for debugging "why is this row gray" and as the
resolver-walk regression op for #142. (The only theme-derived input to
the resolver is the default fg/bg pair; no `bold-color` accent is
applied today, on either socket.) Viewport only — it takes no
`scrollback` param, and its params are strict, so passing one is
`unknown-field`.

Request: `{"params": {"tab_id": "3"}}`.
Response (truncated):

```json
{"cols": 80, "rows": 24,
 "cells": [
   {"row": 0, "col": 0, "text": "h", "fg": "#ffffff", "bg": "#1c1c1c",
    "has_explicit_bg": false, "bold": true, "italic": false, "inverse": false},
   {"row": 0, "col": 1, "text": "i", "fg": "#ffffff", "bg": "#1c1c1c",
    "has_explicit_bg": false, "bold": true, "italic": false, "inverse": false}
 ]}
```

`fg` / `bg` are `#RRGGBB` strings (lowercase). `has_explicit_bg`
distinguishes a default-bg cell (false) from an SGR-bg cell (true) so
a test can pin paint behavior without reasoning about the canvas
fallback. `text` is `" "` for blank cells.

Also served on a **host-session socket**, from the server Terminal.
Both sockets run the same resolver — the densifier and `resolve_colors`
live in `roost-vt` and the iced UI imports them — so a session's answer
and a UI's cannot drift. A session has no theme, so its default
foreground/background are the server Terminal's own (white on black
until a program changes them; see [`session.set_theme`](#sessionset_theme)
for how an attached client recolors it).

On a **UI socket**, `tab_id` accepts the host-qualified `h<host>.<id>`
spelling too, resolving to an attached host tab's client-side Terminal
exactly as [`tab.dump`](#tabdump) does above; a session socket refuses
it the same way.

### `tab.feed_pty_bytes` *(test-only — gated)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Injects raw bytes into a
tab's PTY-output drain as if the supervisor had emitted them; the OSC
scanner + libghostty + the input-reply path process them identically
to real shell output. No shadow drain — same channel the real
`TabSession` writes to. See
`docs/development/test-automation.md` §5.4.

Request:
```json
{"params": {"tab_id": "3", "data": "G10xMTtyZ2I6MDAvMTEvMjIH"}}
```

`data` is base64-encoded raw bytes. Response: `{}`.

**Ordering:** the bytes are applied the moment the UI services the op.
They are *not* serialized against PTY output still in flight from the
shell, so an injection sent right after a tab attaches can be applied
*before* the shell's startup bytes — the prompt then lands on top of
(or appended to) whatever was just seeded. Harnesses must wait for the
tab to go quiet before seeding: attach is not enough, the predicate has
to observe the shell painting and then stopping (`tools/roosttest`'s
`util.wait_tab_quiet` — non-empty `tab.dump` text, byte-identical
across consecutive polls).

On a **host-session socket** the op is served the same way, gated on
the same `ROOST_TEST_MODE=1` (in the *session's* launch environment)
and routed into the tab task's pipeline — so injected bytes are
seq-assigned, teed to attached clients, and ringed exactly like real
child output. They are **chunked to 4096 bytes** on the way in, the
same granularity the PTY reader produces: a chunk is the unit a seq is
assigned to, and one unchunked megabyte would be a single PTY frame
past the data plane's 1 MiB frame cap. A large injection therefore
arrives as several PTY frames, and the ordering caveat above applies
unchanged. Unlike the UI path this op is a **mutation** on a session —
it writes into the authoritative terminal — so it answers
`shutting-down` once `session.stop` has latched.

### `tab.capture_pty_input` *(test-only — gated)*

**Requires `ROOST_TEST_MODE=1` at UI launch.** Returns (and by default
drains) the bytes the UI has queued onto this tab's PTY-input channel
since the last drain — keystrokes, paste payloads, OSC-reply
synthesised replies. Combined with `tab.feed_pty_bytes` this lets a
test exercise the full OSC reply round trip end-to-end.

Request: `{"params": {"tab_id": "3", "drain": true}}`. `drain`
defaults to `false` (peek). Response:

```json
{"data": "G10xMTtyZ2I6MDAwMC8xMTExLzIyMjIH"}
```

On a **host-session socket** it reads the same buffer one level down:
the bytes the tab task queued for the child's PTY, which is where a
session's terminal replies (DA/DSR/color queries) and any `INPUT`
frames from an attached data connection both land, in the order the
task produced them. `drain` is honored identically (consume vs. peek).
This is how the exactly-once reply rule is asserted headlessly — one
answer in the capture per query, no matter how many clients are
attached.

`tab_id` accepts the host-qualified `h<host>.<id>` spelling on a **UI
socket** too — same rule as [`tab.dump`](#tabdump): it reads an
attached host tab's client-side terminal-reply buffer, and a session
socket refuses the qualified form.

### `tab.feed_ime` *(test-only — gated)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Drives an IME
preedit/commit/session-boundary event through the same production
path (`ime_preedit` / `ime_commit` / `ime_session_boundary`) a real
input-method event takes.

Request:
```json
{"params": {"tab_id": "3", "action": "preedit", "text": "こ",
            "cursor_start": 0, "cursor_end": 3}}
```

`action` is `"preedit"` (update the composed-text buffer), `"commit"`
(finalize `text` and send it to the PTY), or `"clear"` (cancel any
in-flight composition — the session-boundary path a real IME takes
between compositions). `cursor_start` / `cursor_end` are optional byte
offsets into `text` marking the preedit cursor/underline span; they
must be given together (either alone is rejected with
`invalid-param`), and `cursor_start > cursor_end` is rejected the same
way. Response: `{}`.

The op routes by the UI's active keyboard route, not directly by
`tab_id`: `tab_id` must match the tab currently holding the route, or
the call fails `invalid-param` rather than silently feeding the wrong
tab. Implemented by the iced UI only; the Swift Mac app has no case for
this op and answers `unknown-op` (it does support IME input, via AppKit's
own `interpretKeyEvents` — there is just no IPC op to drive it).

### `project.create`

Request: `{"params": {"name": "", "cwd": "/tmp"}}`. `name` empty means
the server picks `"Untitled <n>"`.

Response: `{"project": <Project>}` — `tabs` is empty.

### `project.rename`

Request: `{"params": {"project_id": "1", "name": "Roost"}}`. Response: `{}`.

### `project.delete`

Cascades; tabs in the project are closed and their PTYs reaped before
the project is dropped. Subscribers see `tab.closed` for each child
tab followed by `project.deleted`.

Request: `{"params": {"project_id": "1"}}`. Response: `{}`.

### `tab.reorder`

Request:
```json
{"params": {"project_id": "1", "tab_ids": ["3", "2", "1"]}}
```

Order is leftmost first. Ids not belonging to `project_id` are rejected
with `invalid-param`. Tabs in the project not listed keep their
relative order after the listed ones. Ids are the **canonical**
spelling: a non-canonical integer (`"+4"`, `"04"`) is refused rather
than normalized. That holds on the **iced UI socket** and on a
**session socket** — the two that parse a ref which may carry the
`h<host>.<id>` form. The Swift Mac socket diverges, and it is
documented rather than promised: it decodes both reorder ops through
its plain string↔`Int64` codec (`@StringInt64` / `@StringInt64Array`),
and Swift's `Int64(String)` accepts a leading `+` and leading zeros, so
`"+4"` still normalizes there. Canonical traffic is byte-identical
everywhere, which is what the vectors pin; a caller that wants one
answer from both should send canonical ids.

Response: `{}`.

On a **UI socket**, both ids also accept the host-qualified
`h<host>.<id>` spelling (plan 044 §3.1) — `{"project_id": "h3.4",
"tab_ids": ["h3.9", "h3.7"]}` reorders project `4`'s tabs on whichever
connected host this UI process has minted connection id `3` for, by
sending the session the same op over its op queue. It is the drag
gesture's own path, as an op. The session is authoritative: the
sidebar's new order comes from the `tabs.reordered` event that follows,
not from this reply, and the local workspace is untouched.

### `project.reorder`

Request: `{"params": {"project_ids": ["2", "1", "3"]}}`. Order is
topmost first. Same partial-order rules as `tab.reorder`. Response:
`{}`.

Takes the same host-qualified form: `{"project_ids": ["h3.4", "h3.2"]}`
reorders that host's sidebar section, and the same canonical-spelling
rule.

### The reorder routing matrix

Both reorder ops carry a whole order in one id-space, so a request names
one instance or the other and never both:

| Request | Goes to |
|---|---|
| every ref bare (an empty list included) | the local workspace, exactly as before |
| a `Host` project with every tab `Host` on the **same** incarnation (an empty `tab_ids` included) | that host's session |
| every `project_id` `Host` on one incarnation | that host's session |
| two different incarnations; a `Host` project with a bare tab; a **bare** project with a qualified tab; a bare project among qualified ones | `invalid-param`, naming the rule |
| any qualified ref on a **session socket** | `invalid-param` ("host-qualified … refs are a UI-socket form") |
| the host form on a socket with no UI | `invalid-param` ("needs a UI: host connections are client state") |

A host-routed request answers with the session's own error code when
that code is one a UI socket also speaks, so `invalid-param` from a
session's partial-order rules reads the same as the local engine's. A
connection that is not there (disconnected, dropped mid-op, its queue
full) is `host-unavailable`, and so is a refusal whose code a UI socket
does not speak — a session with a latched `session.stop` answers
`shutting-down`, which is session-socket-only, so it folds with its own
code and sentence kept in `message`. An incarnation this client is not
connected to is `not-found`.
Duplicates and unlisted rows keep whatever the answering instance
decides — the partial-order rules above apply unchanged on both sides.

`roostctl tab reorder` / `project reorder` take numeric ids and always
build the local form; the host form is reachable through the op.

### `tab.focus`

Sets the active (project, tab) selection.

Request: `{"params": {"tab_id": "3"}}`. Response:
`{"previous_project_id": "1", "previous_tab_id": "2"}`.

Focusing a tab also acknowledges it: the focused tab's pending
notification is cleared, so `tab.notification` with
`has_pending: false` follows `active.changed` in the same batch — and
only when the tab actually carried one. The badge, the project rollup
and the notification-inbox row all derive from that bit, so they go
with it. Focusing the already-active tab acknowledges it too.

`tab_id` also accepts the host-qualified `h<host>.<id>` spelling on a
**UI socket** — the plan 037 §3.4 wire spelling `roostctl tab focus`
and the attach path both drive. Focusing a host tab is client
selection state, not a workspace mutation, so the response's two
`previous_*` fields are always `"0"`: the host's own workspace owns its
active row, and this client only moved which one it is looking at. A
**session socket** answers `invalid-param` for the qualified form
("a host-qualified tab.focus needs a UI: host selection is client
state") — there is no UI there to hold a selection.

### `tab.set_title`

Manual rename. Sets `Tab.user_titled = true` so subsequent OSC 0/1/2
sequences from the shell do not overwrite it.

Request: `{"params": {"tab_id": "3", "title": "build"}}`. Response: `{}`.

### `tab.set_state`

Request: `{"params": {"tab_id": "3", "state": "running"}}`. Response: `{}`.

Internally this claims agent ownership as `source: "manual"` (an
empty `session_id`) — the same `tab.agent_report` machinery any agent
adapter uses — which is why setting state manually **supersedes** a
live agent's ownership: a real agent's own reports are dropped until
its next claim (its next session start). `state: "none"` additionally
**releases** ownership rather than claiming an inactive one, so the
tab falls through to shell-derived state — a tab with a live
foreground process now reads `running` under `none`, not
unconditionally `none`. See
[`docs/guides/notifications.md`](../guides/notifications.md#manual-override-tab-set-state).

### `tab.clear_notification`

Clears `Tab.has_notification` and emits the corresponding
`tab.notification` event with `has_pending = false`.

Request: `{"params": {"tab_id": "3"}}`. Response: `{}`.

### `tab.set_hook_active` *(deprecated — use `tab.agent_report`)*

Kept working as an alias for backward compatibility; new integrations
should call `tab.agent_report` directly. `active: true` claims
ownership as `source: "legacy"` with an empty `session_id`
(equivalent to `tab.agent_report` with `ownership_action: "claim"`
and no lifecycle change); `active: false` releases it the same way a
matching `release` would. `hook_active.changed` fires exactly as
before.

Request: `{"params": {"tab_id": "3", "active": true}}`. Response: `{}`.

### `tab.agent_report`

The one op every agent adapter writes through — Claude's
`roostctl claude-hook` today, and any future agent. A report carries
**explicit patch intent** rather than a full state so a stateless
adapter never has to read current state to describe an event: which
axis changes, and how, is spelled out field-by-field; anything
omitted means "unchanged."

Request:
```json
{"id": "9", "op": "tab.agent_report", "params": {
  "tab_id": "5",
  "source": "claude",
  "session_id": "abc123",
  "ownership_action": "preserve",
  "lifecycle": "waiting",
  "attention": "set",
  "severity": "warn",
  "title": "Claude Code",
  "body": "Needs your permission to run a command",
  "detail": "permission_prompt",
  "metadata": {"model": "claude-opus-5"}
}}
```

| Field | Type | Notes |
|---|---|---|
| `tab_id` | string-int64 | required |
| `source` | string | open string identifying the agent (`"claude"`, `"manual"`, `"legacy"`, a third-party agent's own name…); must be non-empty (`invalid-param` otherwise) |
| `session_id` | string | opaque per-source session id; empty for sources with no session concept (`manual`, `legacy`). Ownership identity is the **pair** `(source, session_id)` — not `session_id` alone, since two agents could otherwise collide on an opaque id |
| `ownership_action` | `"claim"` \| `"preserve"` \| `"release"` | **required, no default** — "take the tab" and "I already own it" have opposite failure modes, so there's no safe implicit choice |
| `lifecycle` | `AgentLifecycle` | **optional; omitted means "leave the current lifecycle unchanged."** Present only on events that actually move it |
| `lifecycle_if` | array of `AgentLifecycle` | **optional; omitted means unconditional** (every pre-046 client). A guard on the tab's *current* lifecycle — see [Guarded reports](#guarded-reports) below |
| `attention` | `"set"` \| `"clear"` \| `"preserve"` | defaults to `"preserve"` |
| `severity` | `"info"` \| `"warn"` \| `"error"` | defaults to `"info"`. Carried on the model now so a later policy revision can have `failed` interrupt regardless of focus; v1's notification policy does not yet consult it |
| `title` / `body` | string | required when `attention == "set"` (`invalid-param` if missing), ignored otherwise |
| `detail` | string | free-form reason for the report (`"permission_prompt"`, `"background_tasks:2"`, an error name…); recorded onto the ownership record when non-empty |
| `metadata` | map<string, string> | **open extension channel** — the one field a client may extend without coordinating with the server. The params struct carries `#[serde(deny_unknown_fields)]` per the strict-server convention above, so a new *named* field on this op is a **request-schema change**, not an additive one: an older UI or `roost-session` answers `unknown-field` and drops the whole report, attention included. `lifecycle_if` was added that way in plan 046 — both server implementations changed together, and `protocol_version` did not move because `roostctl` and the UI ship as one build. Anything an adapter can express as data belongs in `metadata` instead. Adapter-stamped keys already in use, and the `<product>.` prefix rule for a key owned by one product, are documented in the agents guide ([`agents.md#gx`](../guides/agents.md#gx)) |

`ownership_action` semantics, enforced under one lock so the check and
the mutation can't race a concurrent report:

* **`claim`** always takes ownership, replacing any existing owner
  unconditionally — the only path that can take a tab from a live
  owner (a `SessionStart`, or a manual override via `tab.set_state`).
* **`preserve`** requires the report's `(source, session_id)` to match
  the current owner; a mismatch is dropped (see `accepted` below).
  `detail`/`metadata` **merge** onto the existing owner rather than
  replacing it — an empty field means "this event says nothing about
  it," not "clear it," because there is no delete channel in v1 and
  metadata is expected to accumulate across a session (e.g. `model` at
  `SessionStart`, a cron count at `Stop`).
* **`release`** also requires a match; it clears ownership and forces
  `lifecycle` to `"inactive"`.

#### Guarded reports

`lifecycle_if` makes a report conditional on where the tab already
stands, so an adapter can say "this only means something if the turn was
still running" without reading state first:

```json
{"id": "10", "op": "tab.agent_report", "params": {
  "tab_id": "5",
  "source": "claude",
  "session_id": "abc123",
  "ownership_action": "preserve",
  "lifecycle": "waiting",
  "lifecycle_if": ["working"],
  "attention": "set",
  "severity": "info",
  "title": "Claude Code",
  "body": "Claude is waiting for your input",
  "detail": "idle_prompt"
}}
```

* Current lifecycle **in** the set — the report applies in full.
* Current lifecycle **not in** the set — the `lifecycle` patch **and**
  any `attention: "set"` are dropped. A transition that did not happen
  is not news: agents nag on a timer (Claude's `idle_prompt` ~60s after
  a turn ended, cursor's repeated `stop`), and an unguarded nag
  re-banners a turn the user already saw finish. `detail` and `metadata`
  still merge, and `attention: "clear"` still applies.
* A `release`'s **lifecycle** is exempt: it clears ownership and forces
  `"inactive"` wherever the lifecycle stood — a guard cannot keep a
  departed agent's dot alive. Its `attention: "set"` is **not** exempt
  and is gated like any other, so an adapter can say "announce the
  session ending only if the turn was still running".
* An omitted `lifecycle_if` is unconditional — the behaviour every
  client had before plan 046.

The interaction worth knowing: the OSC 133 failsafe (`A`/`B`/`D` drops
the lifecycle to `"inactive"` while keeping ownership as a label) leaves
a tab whose agent is gone but still owned. A later report guarded on
`["working"]` then correctly no-ops instead of re-lighting a dead
agent's dot.

Response:
```json
{"id": "9", "ok": true, "result": {
  "accepted": true,
  "tab": {"...": "the full post-report <Tab>"}
}}
```

`accepted` is `false` when the report lost the ownership-matching
check above — `tab` is then the tab **unchanged**. The full `Tab` is
always returned so an adapter never needs a follow-up `tab.list` to
see what its own report did.

A report vetoed by `lifecycle_if` is **not** a rejected one: `accepted`
stays `true` (the ownership check passed) and the returned `tab` simply
shows the unchanged `agent_lifecycle`. `accepted` answers "did this
report belong to the tab's owner", never "did the lifecycle move" —
compare the returned `agent_lifecycle` for that.

### `notification.create`

Fire a system notification for a tab.

Request:
```json
{"params": {"tab_id": "3", "title": "Build", "body": "passed"}}
```

Response: `{}`.

### `app.screenshot`

Render the running UI's whole window (sidebar + tab bar + active
terminal) to a PNG, **in-process** — the UI re-draws its own view tree
rather than capturing the screen, so it needs no screen-recording
permission and works even when the window is unfocused, occluded, or
offscreen. Backs `roostctl screenshot`.

Request:
```json
{"params": {"scale": 1}}
```

`scale` is the pixel multiplier — `1` (default) renders at logical
window size, `2` super-samples. Values outside `1..=2` are rejected
with `invalid-param`.

Response:
```json
{"png": "<base64-png>", "width": 1100, "height": 700, "scale": 1}
```

`png` is the PNG bytes base64-encoded (see **Bytes payloads** above);
`width`/`height` are the pixel dimensions actually rendered
(== logical size × `scale`). The response rides the same 16 MiB frame
ceiling as every other op — a normal window PNG is well under it.

Errors: `internal` when there is no window to capture, the window is
minimized (Mac) or not yet realized (Linux), or PNG encoding fails;
`invalid-param` for an out-of-range `scale`.

### `app.window_metrics`

Read logical application-content geometry for screenshot and pointer drivers.
Request: `{"params": {}}`.

```json
{"window_width":1100.0,"window_height":700.0,"sidebar_width":220.0,
 "sidebar_collapsed":false,"terminal_top":34.0,"terminal_font_family":"Berkeley Mono"}
```

`terminal_top` and `terminal_font_family` are optional for wire compatibility
(omitted, not `null`, when an adapter has nothing to report). The iced UI
reports the exact application-owned top edge of its terminal viewport (the
chrome-band height above it), always; the Mac UI reports the AppKit terminal
view's top offset, measured from the content view's top edge. Consumers that
require exact coordinates must reject a missing, non-finite, or non-positive
value instead of copying a chrome-height constant. `terminal_font_family` is
the resolved family the live terminal is actually rendering with
(post-fallback-chain, not a config echo). Both fields are reported by both
adapters once a terminal is live — the Mac adapter omits both until a terminal
view is mounted (fresh launch, no tabs). This operation is ungated and
read-only.

### `app.sidebar_dump`

Read the sidebar's **last-rendered** agent rows, per project, plus the
`show-sidebar-agents` toggle. Both UIs keep an explicit
`rendered_agents` cache per project, written in the same refresh pass
that rebuilds the sidebar widgets; this op reads that cache rather than
re-deriving the rows from the workspace snapshot, so a refresh a UI
forgot to run is a wire-visible test failure instead of an invisible
one.

Request: `{"params": {}}`.

Response:
```json
{ "agents_visible": true,
  "projects": [ { "project_id": "1",
                  "agents": [ { "tab_id": "7", "name": "slauth-refactor",
                                "lifecycle": "waiting", "status_text": "Waiting for input",
                                "time_text": "2m", "is_active": false } ] } ] }
```

All ids are string-wrapped int64s, matching every other op. `agents_visible`
reflects the config/feature toggle only — nothing else. **All** projects
appear, in sidebar order, including projects with zero agents.
`projects[].agents` stays populated even when the toggle is off or a
project drag is in progress: hiding the rows and flattening the sidebar
during a drag are transient UI state, not part of this contract.

#### `hosts` — the sections below LOCAL

`projects` is this UI's own workspace. The host sections come back
beside it, in sidebar order:

```json
{ "agents_visible": true,
  "projects": [],
  "hosts": [ { "id": "hs-2f1c", "label": "workbench", "state": "connected",
               "projects": [ { "key": "h3.4", "name": "roost",
                               "tabs": [ { "key": "h3.9", "title": "zsh" } ] } ] } ] }
```

`id` is the saved host's stable id — what a `host.*` verb is addressed
to — and `state` is the section's own spelling, the same string
`host.status` reports. Every `key` is the host-qualified
`h<host>.<id>` form the UI socket's `tab.focus`, `tab.reorder` and
`project.reorder` take, so a caller that read this dump can drive those
ops without probing for the incarnation first.

The order is the **authoritative** one — the mirror's, as the session
last published it. The sidebar may be drawing a held drag preview for a
moment after a drop; this op never reports it. The host views are
refreshed before the read, so the answer cannot lag the mirror by an
event. A dimmed (disconnected) section is listed with the rows it
retained, still under the incarnation that published them; a host that
has never connected has an empty `projects`.

`hosts` is **omitted entirely** when there are none, so read it as
absent-tolerant: host sessions are iced-only, and the Swift Mac app
answers this op without the field at all. A UI with no host sections
therefore returns byte-identical bodies on both.

Ungated, read-only — always available, matching `app.window_metrics`.

### `app.render_stats` *(iced UI only)*

Read the running UI's render-path counters. This is the only way to
measure the real draw path: `TerminalWidget::draw` needs a live
renderer, which unit tests cannot construct.

Implemented by the iced UI; the Swift Mac app has no case for this op
and answers `unknown-op`.

Request: `{"params": {"reset": false}}`. `reset` defaults to `false`,
so `{"params": {}}` — or no `params` at all — is a plain read.

Response:
```json
{"refresh_calls": "412", "refresh_nanos": "51500000",
 "rows_rebuilt": "9888", "cells_walked": "790400",
 "draw_calls": "377", "draw_nanos": "94250000",
 "fill_text_calls": "9048", "view_calls": "412", "view_nanos": "3300000",
 "elide_calls": "0", "elide_nanos": "0"}
```

Every counter is a **string-wrapped int64** (the same
`string_int64` convention the envelope `id` uses, above):
the nanosecond accumulators pass 2^53 after roughly 104 days of
measured render time, and the rest ride the same convention so the
shape is uniform. All are running totals since process start, or since
the last `reset: true`.

`refresh_calls` / `refresh_nanos` cover the snapshot rebuild that walks
libghostty's render state; `rows_rebuilt` and `cells_walked` are what
that walk touched. `draw_calls` / `draw_nanos` / `fill_text_calls`
cover the widget draw pass. `fill_text_calls` counts glyph draws the
pass emitted — a sprite-rendered cell (box drawing, blocks) *replaces*
a glyph draw and counts as one. `view_calls` / `view_nanos` cover the
whole `App::view()` rebuild; `elide_calls` / `elide_nanos` cover
`chrome::elide_to_width`, the tab-pill title eliding it calls. Those
four default to `0` on decode when absent, so a client parsing a
response from an adapter that doesn't instrument them still works.

`reset: true` zeroes the counters **after** the read, so a caller can
read-reset, run a workload, then read the delta directly.

Caveat: `app.screenshot` re-renders the window, so taking a screenshot
inflates the three draw counters. Read before capturing, or reset
after.

Ungated — always available, matching `tab.dump_resolved`. Not
read-only: `reset: true` reads the counters and then zeroes them.
CLI: `roostctl render-stats [--reset]`.

### `app.dock_badge` *(test-only — gated, macOS iced only)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Reads the macOS Dock
tile's live badge label — the parity port of `App.swift`'s
`refreshDockBadge()`, which mirrors the notification-inbox count onto
`NSApp.dockTile.badgeLabel` and writes `nil` at zero.

Request: `{"params": {}}`. Response:

```json
{"label": "3"}
```

`label` is `null` when the badge is cleared. The handler reads AppKit
on the main thread and deliberately does **not** re-derive the label
from the inbox first: recomputing would assert the count→label mapping
(which unit tests already pin) while proving nothing about whether the
write reached the Dock. Because the badge write rides the update loop
asynchronously, callers poll rather than reading once —
`tools/roosttest/test_dock_badge.py` is the reference use.

Implemented only by the iced UI on macOS. The iced UI on Linux answers
`not-implemented`, and the Swift Mac app has no case for it at all, so
its dispatcher answers `unknown-op`. There is no Dock off macOS, and
answering a plausible `null` there would read as "the badge is cleared"
and pass a test that never ran.

### `app.menu_dump` *(test-only — gated, macOS iced only)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Reads back the macOS
iced UI's live native menu bar — walks the actual `NSApp.mainMenu`
AppKit holds, not a re-derivation from the keybind table, so the
e2e suite can assert table↔menu agreement rather than trusting the
menu-building code to have gotten it right.

Request: `{"params": {}}`. Response:

```json
{
  "menus": [
    {
      "title": "File",
      "items": [
        {
          "title": "New Tab",
          "key_equivalent": "t",
          "modifiers": ["super"],
          "enabled": true,
          "state": "off",
          "separator": false,
          "action": "new_tab"
        },
        {
          "title": "",
          "key_equivalent": "",
          "modifiers": [],
          "enabled": true,
          "state": "off",
          "separator": true,
          "action": null
        }
      ]
    }
  ]
}
```

`modifiers` uses the fixed vocabulary `["shift","ctrl","alt","super"]`,
always in that order. `key_equivalent` is the raw `keyEquivalent`
string AppKit holds (empty when the item has none, or while the
gating seam has blanked it — see `sync_gating` in
`crates/roost-iced/src/macos/menu.rs`). `state` is `"on"` or `"off"`;
`NSControlStateValueMixed` never appears — nothing in this menu bar
ever sets it, and a dump that saw it would fail with `internal`.
`action` is `KeybindAction::to_wire_name()` for a table-bound item, a
`"select_project:<id>"` / `"select_tab:<id>"` marker for a Window-menu
row (by stable id, not position), the `"quit"` marker for the App
menu's Quit item, the `"check_for_updates"` marker for the App menu's
Sparkle item, `"appkit:<selector>"` for a standard AppKit item
(About, Hide, Minimize, Zoom, …), or `null` for an inert item (Cut,
Select All, and every separator). The App menu's title is the profile
display name (set at install time — no separate runtime substitution to
account for).

Implemented only by the iced UI on macOS, same as `app.dock_badge`;
the iced UI on Linux answers `not-implemented`.

### `app.menu_activate` *(test-only — gated, macOS iced only)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Resolves a title path
through the live native menu bar (the same tree `app.menu_dump`
reads) and fires it via `performActionForItemAtIndex:` — the same
dispatch a real click takes, so the op exercises the full
AppKit → channel → update-loop path, not a shortcut around it.

Request:

```json
{"params": {"path": ["File", "New Tab"]}}
```

Response: `{}`.

Titles carry real ellipsis characters (U+2026, e.g. `"Rename Tab…"`),
not three literal periods. `performActionForItemAtIndex:` performs no
validation of its own (Apple's docs), so the handler checks the
resolved item's `isEnabled` itself and errors rather than firing a
greyed-out item. Errors (`invalid-param`): an unknown path, an
ambiguous one (two items sharing a title at the same level — the
dynamic Window menu's project/tab rows can collide, so seed unique
names), or a disabled item. Because the fired `MenuEvent` rides the
same async engine-feed channel a real click does, its effect lands on
a later update turn — callers must condition-wait on the observable
result (e.g. `tab.list` growing), never assert synchronously on the
reply.

Implemented only by the iced UI on macOS, same as `app.menu_dump`.

### `app.update_status` *(test-only — gated, macOS iced only)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Reads back the Sparkle
updater's state from the macOS iced UI's seam
(`crates/roost-iced/src/macos/sparkle.rs`).

Request: `{"params": {}}`. Response:

```json
{
  "framework_loaded": true,
  "updater": "started",
  "reason": null,
  "check_id": 1,
  "last_check": {"outcome": "found", "version": "99.0.0", "detail": null}
}
```

`framework_loaded` is whether `Contents/Frameworks/Sparkle.framework/
Sparkle` was found beside the executable and `dlopen`ed — false for
every bare-binary build, because the framework only ever ships inside
`Roost-Iced.app`. `updater` is `"started"` once `-startUpdater:`
succeeded and `"unavailable"` otherwise, with `reason` carrying the
why (no framework, a refused start). `last_check` is `null` until a
check completes; its `outcome` is `"found"` (a newer version is in the
appcast, `version` set from `SUAppcastItem.displayVersionString`),
`"none"` (the feed parsed and offered nothing newer) or `"error"` (no
feed, an unreachable one, a malformed appcast), with `detail` carrying
the reporting error's `localizedDescription`.

`check_id` increments once per **completed** check. Condition-wait on
it advancing rather than on `last_check` becoming non-null: the latter
can pass on a previous check's result.

The "Check for Updates…" menu item's enabled state mirrors
`updater == "started"` plus Sparkle's own `canCheckForUpdates`, so
`app.menu_dump` and this op agree by construction.

Implemented only by the iced UI on macOS, same as `app.menu_dump`.

### `app.update_check` *(test-only — gated, macOS iced only)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Starts a non-interactive
`-[SPUUpdater checkForUpdateInformation]`: feed fetch, appcast parse
and version comparison, with **no** UI panel and no download. (The
menu item drives the interactive `checkForUpdates` instead; nothing
automated does.)

Request: `{"params": {}}`. Response: `{}`.

The reply returns as soon as the check is dispatched. Results land in
`app.update_status` through the updater delegate's callbacks, so
callers condition-wait on `check_id` advancing. Errors (`internal`)
when the updater is unavailable.

In test mode the seam's updater delegate overrides the feed URL from
`ROOST_SPARKLE_FEED_URL`, which is how `tools/roosttest/test_sparkle.py`
points a check at a loopback appcast. **Both** conditions are required
(`ROOST_TEST_MODE=1` at launch *and* the variable): a production bundle
ignores the variable entirely.

Implemented only by the iced UI on macOS, same as `app.menu_dump`.

### `app.notification_status` *(test-only — gated, macOS only)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Reads back the calling
UI's `UNUserNotificationCenter` backend state — the iced UI from
`crates/roost-iced/src/macos/notifications.rs`, the Swift app from
`DesktopNotifications.status()` in `mac/Sources/Roost/DesktopNotifications.swift`.

Request: `{"params": {}}`. Response:

```json
{"backend": "available", "reason": null, "authorized": false}
```

On iced, `backend` is `"available"` once the UN delegate has
installed — a bundled launch that has reached `window_opened` — and
`"unavailable"` otherwise: every bare-binary build (no app bundle, so
UN is never touched), and a bundled app before its first window opens.
`reason` names why it is unavailable (`"not running from an app
bundle"`, `"window not opened yet"`), or `null` once available. The
Swift app has no such gate — its `UNUserNotificationCenter` delegate is
installed at construction, and a build that could not do that would
have aborted at launch rather than produced a running process to query
— so it always reports `backend: "available"` with a `null` `reason`.
`authorized` is the user's answer to the authorization prompt on both
UIs, always `false` while `backend` is `"unavailable"` — CI's TCC
authorization state is unknowable, so the automated suite never asserts
this `true`; the real prompt/click is the morning checklist (#285).

Implemented by both UIs, macOS only — unlike `app.menu_dump` above and
the other macOS-gated ops around it, which are iced only and have no
Swift counterpart.

### `sidebar.set_width` *(test-only — gated)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Sets the projects
sidebar's logical width — the programmatic twin of dragging the seam,
so the e2e suite can pin resize + persistence without a real pointer.

Request: `{"params": {"width": 260.0}}`. Response: `{}`.

The UI routes the width through the workspace, which **clamps** it to
`[160, 400]` and persists it, so an out-of-band width (`90`, `1000`)
succeeds and lands on the nearest bound rather than erroring. Read the
applied value back with `app.window_metrics`.

`width` must be finite and positive; zero, negative, and non-finite
values are rejected with `invalid-param` before reaching the UI.

While the sidebar is collapsed the op still succeeds: it persists the
width and updates what expanding will reveal. `app.window_metrics`
reports a collapsed sidebar as `sidebar_width < 1.0` until it is
expanded: the iced UI reports a literal `0.0`, while the Mac UI reports
the collapsed pane's real frame width. Assert `< 1.0`, not `== 0.0`.

Two authority caveats, mirroring `window.resize`'s "the compositor
remains authoritative" stance: the persisted value is the *requested*
logical width, and a window too narrow to honor it may render the
seam narrower — on macOS `NSSplitView`'s `constrainMaxCoordinate`
clamps the divider to the split view's allocation, so the reported width
follows what was actually laid out.
`app.window_metrics` reports the live width, so assert against that.
And the op is not defined concurrent with a live pointer drag of the
seam: a drag in flight re-anchors on its press-time width and its
release wins. Harnesses drive one or the other, never both at once.

### Host bootstrap test ops (`app.dialog_dump` / `app.dialog_answer` / `app.keybind_dispatch`) *(test-only — gated)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it every op in this group errors. `tools/roosttest/` drives a
real UI over this socket and nothing else, so without a seam onto the
host dialog family (`HostDialog::{Add, ConfirmStop, ConfirmRestart,
Bootstrap}` — [Host sessions (development)](../development/host-sessions.md#bootstrap-installupgrade-over-ssh))
the consent card the SSH bootstrap flow (plan 039) gates on could not
be exercised at all. Unlike the upgrade prompt, whose button composes
ops a test can already send directly, the bootstrap job is
deliberately UI-only and has no such back door.

**These are a test seam and not a production surface.** Nothing but
`ROOST_TEST_MODE=1` can reach them, `roostctl` grows no verb for any
of them, and the rule they exist to protect is the opposite of a
remote-control API: a modal must never be raised at — or answered by —
a machine.

`app.dialog_dump` reads which host modal is on screen and what it
says — the *rendered* strings, not the state behind them, so a test
asserting "the user is told the right thing" isn't re-deriving the
copy rule a second time.

Request: `{"params": {}}`. Response:

```json
{
  "dialog": "bootstrap",
  "variant": "install",
  "title": "Install roost-session on workbox?",
  "body": "roost-session 0.0.19 (ghostty-abcdef0…) will be installed to ~/.local/bin/roost-session on workbox, from this Roost's own roost-session.",
  "buttons": ["Cancel", "Install"],
  "host": "3f9a2b7c1d4e4f5a"
}
```

`dialog` is `"add" | "confirm_stop" | "confirm_restart" | "bootstrap"`,
or absent (with every other field defaulted/empty) when no host modal
is open. `variant` is present only for `"bootstrap"` —
`"install" | "update" | "start"` — and `null`/absent otherwise.
`buttons` lists every button in render order, the dismissing one
first, exactly as the card draws them. `host` is the saved host's
**id** — the opaque hex `host.add` minted and `host.list` reports, the
same value `host.connect` takes — not its label, even though the
rendered `title` and `body` above interpolate the label. Absent when
the dialog is not about a saved host.

`app.dialog_answer` presses the visible modal's primary button, or
dismisses it — through the same production handlers a real click or
Enter/Escape takes, so every guard the button itself has (re-reading
state at confirm, the mutation claim, refusing to run twice) applies
here too.

Request: `{"params": {"action": "confirm"}}` (or `"cancel"`). Response:
`{}`.

`action` outside `"confirm" | "cancel"` is rejected `invalid-param`
before anything else runs. Every other refusal is `internal`, carrying
a human-readable reason: no host dialog is open; `"confirm"` sent to a
dialog with no confirming action (a
remote host whose `NeedsRestart` dialog can only offer the
docs-pointer copy, `RestartAction::None`); or `"confirm"` sent to the
Add Host dialog while it's already dialing a verify. A dialog with no
primary action refuses `confirm` rather than silently dismissing — a
test that thinks it pressed a button that isn't there should fail
loudly, not pass by accident.

`app.keybind_dispatch` runs a named keybind-table action through the
same dispatcher a real key press or native menu click uses. It exists
because paste has no other IPC back door — unlike a palette row, it's
reachable only from a real key event — so the harness needed a seam to
drive it at all, including issue #376's frozen-host-frame refusal.

Request: `{"params": {"action": "paste"}}`. Response: `{}`.

**Not a general keybind dispatcher.** `action` accepts only the
literal `"paste"`; every other `KeybindAction` name (`"close_tab"`,
`"new_tab"`, `"copy"`, …) is rejected — an arbitrary IPC client isn't
trustworthy with a route that can close a live terminal, mutate
workspace state, or write the system clipboard. Like
`app.dialog_answer`'s `action` check, this is rejected `invalid-param`
before anything else runs. Widening the allowlist happens one name at
a time, alongside a concrete test need.

### Command palette (`palette.*`)

Drive the command-palette overlay — open it, read its rows, filter,
activate a row, dismiss. UI-only: routed to the UI like `app.screenshot`,
not the workspace. A command row's id **is** its KeybindAction id, so
activating a row runs the same dispatch its hotkey would; activating a
sub-frame row (e.g. `select_theme`) drills in. Backs `roostctl palette`.

All five ops reply with the resulting palette state, so a driver needs no
follow-up `palette.state`:

```json
{"open": true, "frame": "commands", "query": "tab", "selection": 2,
 "items": [{"id": "new_tab", "title": "New Tab"},
           {"id": "select_theme", "title": "Select Theme…"}]}
```

`open` is false when no palette is up (the other fields are then
empty/default). When open, `frame` is the current frame id — `commands` |
`launcher` | `custom` | `themes` | `fonts` | `notifications` | `present` |
`agents` (a provider drill-in sub-frame gets a generated
`provider:items:<n>` id instead) — and `items` are the filtered rows in
display order (`subtitle` present on rows that have one).

The **agents** frame (`kind: "agents"`) lists one row per tab an agent
owns, ordered by urgency (running Claude/Codex/etc. sessions; excludes
tabs Roost itself claimed via `manual`/`legacy` ownership). Its rows carry
an additional `agent` object, absent on every other frame's rows:

```json
{"id": "agent:3", "title": "Claude Code · roost · slauth-refactor",
 "agent": {"effective_lifecycle": "waiting", "agent": "Claude Code",
           "project": "roost", "name": "slauth-refactor",
           "status_text": "Waiting for input", "time_text": "2m",
           "metrics_text": "4f +86 -12"}}
```

`agent` is the display name derived from the tab's ownership source —
`Claude Code` / `Codex` / `OpenCode` / `Grok` / `Cursor` for the five
built-in adapters, and the raw source string verbatim for anything
else (adding a sixth adapter never requires touching this table) —
folded into `title` as its leading `·`-separated segment.
`effective_lifecycle` is one of `working` / `waiting` / `finished` /
`failed` / `inactive` — the same value the tab pill and sidebar rollup
render, so this row can never disagree with them. `metrics_text` is
**absent while the row's git-metrics probe is still pending** and always
present once resolved — `"—"` for a clean repo, a non-repo cwd, or any
probe failure/timeout; otherwise `"<n>f +<adds> -<dels>"` (minus is
ASCII `-`) — so pending vs. resolved is observable on the wire. Activating
an `agent:<id>` row jumps to that tab (revealing the sidebar if it was
collapsed); the empty-state row (`"agents:empty"`) is not actionable.

| Op | Request params | Notes |
|---|---|---|
| `palette.open` | `{"kind": "commands"}` | `kind`: `""`/`commands` → command palette; `launcher` → custom-command launcher; `custom` → the script-backed provider palette; `agents` → the agent-jump palette. Other values → `invalid-param`. |
| `palette.state` | `{}` | Read the current state. |
| `palette.query` | `{"query": "theme"}` | Set the current frame's filter (resets selection to the top match). |
| `palette.activate` | `{"id": "new_tab"}` | Confirm the visible row with this id — runs its command or drills into its sub-frame. `not-found` if no palette is open or no row matches. |
| `palette.dismiss` | `{}` | Close any open palette. |
| `palette.present` | `{"title": "Open shed", "items": [{"id": "web", "title": "shed: web"}]}` | Open the palette on a caller-supplied list and **block** until the user picks a row or dismisses. Replies `{"selected_id"?, "dismissed"}` — `selected_id` is omitted on dismissal. `invalid-param` if `items` is empty. The programmatic twin of the command palette; items are `{id, title, subtitle?}` (the `actionable` flag a [provider](../guides/extending.md#3-dynamic-providers) can set is *not* carried here — present rows are always selectable in v1). An `agent` object on a supplied item is ignored — including a malformed one, which decodes leniently to absent rather than erroring — so present rows always render generic, never the agent layout. v1 limitation: if the client disconnects while blocked, the palette stays open until the user dismisses it (no server-side cancellation yet). |

### Selection + clipboard test ops (`selection.*` / `clipboard.*`)

| Op | Params | Effect |
|---|---|---|
| `selection.set` | `{"tab_id": "1", "anchor": {"col": 3, "row": 0}, "cursor": {"col": 17, "row": 0}}` | Anchor a selection on the tab's terminal at viewport `(col, row)`. The UI pins each endpoint with a libghostty *tracked* grid ref, so the selection follows its content through scrolling, scrollback eviction and reflow — same flow as `mouseDown` + `mouseDragged`. `not-found` if the tab has no live terminal. |
| `selection.clear` | `{"tab_id": "1"}` | Drop the active selection (no-op if none). |
| `selection.dump` | `{"tab_id": "1"}` | Read back the selection. Response: `{"text"?: "...", "anchor_visible": bool, "cursor_visible": bool}`. `text` carries the **whole** selection, including rows scrolled out of the viewport (#249). It is omitted when no selection is active, and also when an active selection currently resolves to nothing — its rows were evicted from scrollback, or it belongs to the screen (primary/alternate) that is not on display. Those cases are reported as an *absent* `text`, never as another row's text (#334); an alt-screen one starts reporting text again once its screen is active. `anchor_visible` / `cursor_visible` stay viewport-truthful on purpose — they answer "is this endpoint on screen right now", which is a different question from what `text` contains, and the pixel-level tests rely on it; a discarded or inactive-screen endpoint reads `false`. |
| `clipboard.dump` | `{"target": "system" \| "selection"}` | Read the host pasteboard. Response: `{"text"?: "..."}`. `system` is the ⌘V / Ctrl+V target; `selection` is the named per-app pasteboard on Mac / X11 PRIMARY on Linux. Unknown targets → `invalid-param`. |
| `clipboard.write` | `{"target": "...", "text": "..."}` or `{"target": "system", "image_png": "<base64>"}` | Test-only pasteboard seeding (lets a roosttest case set a known value before asserting paste behavior). The text form is not gated: any process on the host can already write the OS clipboard. The image form is — see below. |

**`clipboard.write` takes exactly one of `text` / `image_png`.** `text`
was required until the image form existed and is optional now; both at
once is `invalid-param` (silently preferring `text` would drop an image
the caller believed it had written) and neither is `missing-param`.
`image_png` exists for one reason: the host-file-paste end-to-end lanes
need a *real* image on the clipboard, and an IPC-only harness has no
other way to put one there. It is therefore **gated on
`ROOST_TEST_MODE=1`** at UI launch — outside test mode it answers
`not-supported`, the same "this seam is not here" answer a lane skips
on. `target` must be `"system"`: PRIMARY carries text by convention and
the paste path never probes it for an image, so a `selection` image
write would seed a clipboard nothing reads (`invalid-param`). The PNG is
decoded before it is written, so bytes that are not a PNG, that do not
reduce to 8-bit RGBA, or that exceed the paste path's own 40 MP pixel
cap are `invalid-param` too.

The iced UI writes the image through `arboard`, holding one clipboard
handle for the life of the process — X11 has no clipboard *content*,
only an owning window, and a handle dropped at the end of the call takes
the image with it. Under **Wayland** the write is attempted and only its
*failure* is classified: a compositor that implements
`wlr-data-control` (COSMIC does) works, and one that does not — the
headless compositors the lanes run under — fails with `not-supported`
naming [#302](https://github.com/charliek/roost/issues/302), which is a
lane's cue to skip rather than to report a paste bug.

**Mac is text only.** `Roost.app`'s handler names `target` and `text`
and nothing else, so an `image_png` key is `unknown-field` there, and a
request carrying neither field is `invalid-param` (a decode failure, not
`missing-param`).

`roostctl` does not surface these yet — they exist for end-to-end test
coverage (`tools/roosttest/`) and as a stable surface a future scriptable
selection-driving feature (AI agent highlighting a region for the user
to confirm) could build on. Each op routes through the UI seam
(`UiRequest::Selection*` / `UiRequest::Clipboard*` on Linux, the
`UiBridge` protocol on Mac), not the workspace — pasteboard + selection
state live on the UI side.

### Host registry (`host.*`)

Client-side saved-host bookkeeping for [host sessions](../guides/host-sessions.md) — a UI-only op family, like `palette.*`: it manages `Workspace.hosts` (the `{id, label, target, last_connected}` array persisted in the UI's own `state.json`, plan 037 §3.5), not a session's own workspace. `host.connect` / `host.disconnect` / `host.status` additionally reach into the UI's live connection set, so they answer with connection state rather than only a registry mutation — and they require a UI to be attached at all (`internal: no UI attached` on a headless engine embedder, the same honest failure every other `UiRequest`-backed op gives).

A session socket answers `unknown-op` for every verb here — the same "no shadow registry in the daemon" rule `host.add --verify`'s own dial depends on. So does the Swift Mac app's socket: **`roostctl host *` against `--target mac` is a documented, permanent `unknown-op`**, not a gap to be filled — host sessions are iced-only (plan 037 §3.1), and the Swift app never grows this surface.

| Op | Request params | Notes |
|---|---|---|
| `host.add` | `{"label": "pop-os", "target": "/home/charlie/.local/state/roost/roost-session.sock"}` | Saves a host. `target` is carried **opaquely by this op** — the workspace stores whatever string it's given, unvalidated — so classifying it (`roost_ipc::ssh::classify`: an SSH destination like `workbox` / `user@host` / `ssh://user@host:port`, only the `ssh://` spelling carrying a port; a Unix socket path, containing `/`; or the `localhost` sentinel) and refusing an unclassifiable string (empty, a leading `-`, a bare `host:port` with no scheme) is each **caller's** job, done client-side before this op is ever sent — `roostctl host add` and the Add Host dialog both classify first and never call this op on a target that fails. Registry-only beyond that — this does **not** dial `target`, so a typo'd-but-classifiable target still saves cleanly (the sidebar's dot reports it at the next connect attempt). `label` is trimmed, must be non-empty, Unicode-case-insensitive unique, and not `"local"` (the reserved LOCAL band). Response: `{"host": <Host>}`. |
| `host.remove` | `{"id": "3f9a2b7c1d4e4f5a"}` | Forgets a saved host — the registry entry and the dimmed rows its last connection left behind. Never touches the session itself: its shells keep running. Response: `{}`. |
| `host.list` | `{}` | Response: `{"hosts": [<Host>, ...]}`. |
| `host.connect` | `{"id": "3f9a2b7c1d4e4f5a"}` | Starts (or restarts) a connection — a new, symmetric connection alongside any this client or another already has open, never a claim on one — and on a **localhost** target it spawns the session first if nothing is listening. Answers as soon as the attempt is under way, with the state the request *asked for* (`"connecting"`), not the far end's eventual verdict — watch the sidebar or poll `host.status` for the settled state. Response: `{"host": <Host>, "state": "connecting"}`. |
| `host.disconnect` | `{"id": "3f9a2b7c1d4e4f5a"}` | Drops the connection. Never stops the session — its shells keep running, and reconnecting picks them back up (disconnect ≠ stop). Response: `{"host": <Host>, "state": "disconnected"}`. |
| `host.status` | `{}` or `{"id": "3f9a2b7c1d4e4f5a"}` | Every saved host's live connection state — the read-side twin of `host.connect`'s reply, and what a script polls instead of scraping the UI log. Bare `{}` answers for every saved host in registry order; `id` narrows it to one, and an unknown id is `not-found` exactly as `host.connect` gives. Response: `{"hosts": [<HostStatus>, ...]}`. Not test-mode gated: this is a read of state the user can already see in the sidebar. |

`Host` is `{"id": "<hex>", "label": "<string>", "target": "<string>", "last_connected"?: "<ISO-8601>"}`. `state` is one of the wire's connection-state spellings: `disconnected` | `connecting` | `connected` | `stopped` | `needs-restart`.

`HostStatus` is `Host`'s four registry fields (so no second op is needed to correlate) plus what the sidebar's band is drawn from:

```json
{"id": "3f9a2b7c1d4e4f5a", "label": "workbox", "target": "ssh://workbox",
 "last_connected": "2026-09-01T17:40:02Z",
 "generation": 3, "state": "disconnected",
 "reason": "reconnecting in 8s (3/10)",
 "rollup": "disconnected — reconnecting in 8s (3/10)",
 "retry": {"delay_ms": 8000, "attempt": 3, "budget": 10, "armed_at": "2026-09-01T18:02:11Z",
           "reason": "connecting to workbox failed: ssh: connect to host workbox port 22: Connection refused"}}
```

A **live** host additionally carries `connect` and `tabs`:

```json
{"id": "3f9a2b7c1d4e4f5a", "label": "workbox", "target": "ssh://workbox",
 "generation": 4, "state": "connected",
 "connect": {"session_id": "a1b2c3d4", "reduced_fidelity": true,
             "resumed": true, "from_revision": 4312},
 "tabs": 5}
```

- `generation` — which connection attempt this host is on. Bumped once per attempt **started** — an explicit `host.connect`, a launch auto-reconnect, or one rung of the ssh retry ladder — `0` before the first, and **kept across a disconnect**. This is the monotonic edge to wait on: two consecutive attempts can fail with byte-identical reasons, so "state is `disconnected` and `reason` is set" cannot tell attempt N from N−1, but reading `generation` before a connect and waiting for it to advance can. It counts attempts rather than connections on purpose: an ssh host whose handshake never succeeds reaches no connection at all, and a ten-rung ladder that left the number flat would be no edge.
- `state` — the same spellings `host.connect` answers with, from the same classifier the band's dot reads. A host that has never connected is `disconnected`.
- `reason` — the connection's own one-line reason, **untruncated**: the band's *input*. Absent when there is none.
- `detail` — the long form behind `reason`, when there is one the band has no room for. One thing fills it: a **localhost session that could not be started**, where `reason` is the ≤45-character band line (`"cannot find roost-session"`, `"roost-session failed to start"`) and `detail` is what actually happened — the launch ladder's three rungs verbatim, the exec error, or the daemon's own start verdict. Such a host carries no `retry`: no retry could find a binary, so it settles once and waits for ↻ Reconnect.
- `rollup` — the band's *output*, verbatim from the sidebar's reducer, capped at 60 characters with an ellipsis. For a **connected** host this is the agent count (`"3 agents"`), not state text; absent when the band shows no rollup at all. It is what the next frame draws — nothing here asserts a frame was painted.
- `retry` — a `RetrySchedule`, absent unless an auto-reconnect is armed. `delay_ms` is the delay the timer was armed with (not what is left) and `armed_at` is when, so a caller can compute the remainder. `attempt` (1-based, the `3` in the band's `(3/10)`) and `budget` come with the **ssh** ladder only: a localhost retry is the connection task's own backoff whose counter never leaves the task, so it reports `delay_ms` alone.
- `payload_kind` — what the last attach this client accepted on this host is being **decoded as**, one of [`payload_kinds`](#sessionidentify)' spellings. `"vt"` is the fallback a libghostty build skew lands on; such a host connects normally, the dot deliberately stays green, at that payload's [documented fidelity](#payload-kinds). Absent until a tab has actually attached over the *live* connection — it reports what is being decoded, never what could be — and it goes when that connection does, so a reconnect to a matching daemon cannot keep claiming a fallback it is no longer on.
- `connect` — a `HostConnectStatus`, present only while the connection is live, naming what the prologue established **before any tab attaches** — this is what the sidebar's `reduced fidelity` indicator, the update/restart offers, and the CLI's fidelity line all key on, rather than `payload_kind`, which is lazy until an attach. `session_id` is the session's own id from `session.identify`. `reduced_fidelity` is `true` when this client and the session pin different libghostty builds and the connection fell back to `vt` — the same condition `payload_kind` will read `"vt"` for once something attaches. `resumed` says whether this connection's prologue replayed missed events (`events.subscribe {from_revision, session_id}`, R11/#442) instead of taking a fresh `tab.list`; `from_revision` is the revision it resumed from, as the session's subscribe ack attested it, present only when `resumed` is `true`. None of this bumped `SESSION_PROTOCOL_VERSION` — both fields are additive on the **UI** socket only.
- `tabs` — how many tab rows this host's sidebar section is listing right now, always present (`0` included). The "5 tabs" a person reads, and the observable a poller uses to confirm a reconnect never blanked the section (R11: the client keeps drawing the carried mirror through `Connecting` rather than purging it before the fresh one lands).
- `retry.reason` — **why** this rung is armed: the classified failure's own copy, in the same words the give-up line uses for it. It is a separate field from the `reason` above because that one is the band's input and `rollup` is derived from it — while a rung is armed the band has to read `reconnecting in 8s (3/10)`, so the family needs its own slot or it is unreadable until the attempt settles. ssh-only, like `attempt`/`budget`. **The rule for a caller is simply: read it when it is present.** Do not gate on `attempt` — the number is not a proxy in either direction. Absence is ordinary rather than a fault: the drop that *starts* an outage is usually the live connection dying, a bare bridge EOF with nothing to classify, and the classified copy arrives with the next dial's failure; a later rung armed by another connection coming up and dying reads absent again for the same reason. And presence is not confined to later rungs — a suspend/wake resets the ladder to `attempt: 1` while deliberately carrying the family it already had, because it is still the same outage.

Every optional field is omitted rather than `null`, so a host that has never connected is `{"id", "label", "target", "generation": 0, "state": "disconnected", "tabs": 0}` and nothing else — `tabs` is the one field that is always present (`0` included) rather than omitted, since a caller polling it across a reconnect needs a number every time, not a key that comes and goes.

`host.add` / `host.remove` are `UiRequest`-style when a UI is attached (a `roostctl host add` is visible in the sidebar immediately, no restart needed) and fall back to a direct `Workspace` mutation for a headless embedder (the engine's own tests) — both paths mint the same `WorkspaceError` wire codes, so a caller cannot tell which one answered. `host.connect` / `host.disconnect` / `host.status` have no headless form: connection state is the app's alone, and a field that spelled "unknown" and "disconnected" the same way would be exactly the lie the op exists to remove.

### `events.subscribe`

Turn this connection into a one-way event stream. **Served by a
host-session socket only.** Every subscriber receives every event —
there is no per-connection classification of the stream, and there
never needs to be: reading is not authority, so a client that only
wants to watch and one that also types get the same feed.

Request: `{"params": {"tab_id_filter": "0"}}`. Two more fields, both
optional and, per the wire's usual rule, omitted rather than sent as
`null` when unset:

* **`from_revision`** — resume instead of starting fresh: *the client
  already has everything at or below this revision*, the same sentence
  the response's own `revision` already means. Served out of a bounded
  replay ring behind the fence (bounds below); absent means a fresh
  subscribe at the current revision. `0` means "I have nothing, replay
  from 1", valid exactly while the whole history is still retained; a
  value equal to the current revision is a valid resume with an empty
  replay.
* **`session_id`** — the incarnation the client fenced against
  ([`session.identify.session_id`](#sessionidentify)). **A resume must
  name it**: revisions restart at `0` in every process, so a fence
  carried across a session restart would otherwise be served a
  *different* history that still passes the client's own gap check.
  Accepted without `from_revision` too — a client may always state who
  it thinks it is talking to.

Response (the last request/response frame on the connection):

```json
{"id": "7", "ok": true, "result": {"revision": 42}}
```

On a resume the ack echoes `from_revision` back as `revision`, so **"the
first batch is `revision + 1`" holds unchanged**: the replayed batches
run consecutively into the live ones, with a fresh subscribe's
`revision` (the current one) as the case with nothing to replay.

Four refusals answer on the ack, before anything is spawned or
registered — validation runs ahead of the replay cut, the relay task and
the stream registration, so a refusal leaves the connection exactly the
request/response connection it was, and a client may simply subscribe
again on it:

* **`invalid-param`** — `from_revision` without `session_id`: "a resume
  names the session it fenced against: from_revision requires
  session_id, which session.identify reports".
* **`session-mismatch`** — `session_id` names an incarnation that is not
  this one's: "this is session `<id>`, not `<named>`: revisions restart
  with the process, so a fence from another incarnation cannot be
  resumed; snapshot with tab.list and subscribe afresh".
* **`replay-expired`** — `from_revision` is older than the ring still
  holds: "revision `N` is outside the replay window (oldest resumable:
  `M`, current: `C`); snapshot with tab.list and subscribe afresh".
* **`revision-ahead`** — `from_revision` is past the current revision, a
  fence from another incarnation or a client bug: "this session never
  produced revision `N` (current: `C`): a different incarnation, or a
  client bug; compare session.identify.session_id, then snapshot".

Past validation, the stream is live: every workspace batch, plus
[`notification.fired`](#events) and [`tab.effect`](#events) (bells,
OSC 52 clipboard writes) alike — the same feed for every subscriber,
resumed or fresh. Nothing about this op classifies the connection; a
script that only ever calls `events.subscribe` sees exactly what the
attached UI does. What each *client* then does with a `tab.effect` is
its own decision, not the wire's — a bell is worth marking on any tab
that owns one, while a clipboard write should land only on whatever tab
that client is actually looking at, so a background watcher's own
clipboard is never overwritten by a tab it merely subscribes to. See
[`tab.attach`](#tabattach) and the guide's
[Several windows on one session](../guides/host-sessions.md#several-windows-on-one-session)
for that policy from the client side; the wire itself fans every effect
out to every subscriber unfiltered.

Subscribing registers the stream in the session's connection set, which
is what lets [`session.stop`](#sessionstop) find and close every one of
them; nothing about a stream's registration depends on any other
connection this client holds.

After the ack every frame is an `EventBatch` — one per workspace
commit, `{"revision": <u64>, "events": [<EventEnvelope>, ...]}`, one
per newline-delimited frame — **except** one envelope that rides
outside the batch discipline entirely, and is always the last frame on
the stream:

```json
{"event": "session.stopping", "data": {"reason": "stop"}}
```

`reason` is always `"stop"` — the session is shutting down, the only
way this wire ever ends a stream deliberately. It carries **no
`revision`** and is exempt from the gap check below: it is not a
commit, it is the stream saying why it is over, and it is always the
last frame before the close.

The catalog of batch envelopes is [Events](#events) below.

**Replay window.** A subscribe that supplies `from_revision` is served
out of a bounded ring of committed batches the session keeps behind the
fence: **at most** `REPLAY_WINDOW` commits (1024) **and**
`REPLAY_BUDGET_BYTES` bytes (4 MiB) of their serialized JSON, whichever
binds first — stated as a maximum, never as a guaranteed window. A busy
session can exhaust either one (an agent's tab churn burning the count,
or a single 1 MiB `notification.fired` body eating the whole budget at
once — which **clears the ring** rather than leave a hole in the middle
of it, so every fence below it also expires); the answer either way is
`replay-expired`, which is the designed fallback, not a failure — the
client snapshots with `tab.list` and subscribes afresh.

**Effects are live-only.** [`tab.effect`](#events) (bells, OSC 52
clipboard writes — [DL-18](../development/vision.md#dl-18-hosts-ux-attach-on-focus-effects-theme-reseed-and-the-mac-gate-2026-08-29))
is stripped before a batch ever enters the ring, so **no resumed stream
ever receives one from its gap, for any subscriber**: replaying a
clipboard write from thirty seconds ago is wrong regardless of who
resumes into it. A commit whose only events were effects still occupies
a revision and replays as the same empty `{"revision": N, "events": []}`
batch the gap rule already requires. `notification.fired` is not an
effect and **is replayed** — it is a workspace fact, and a watcher that
blinked wants the one it missed.

Three properties made this lossless before the replay ring existed, and
still do — the ring just means fewer clients ever need the third one:

* **The ack is a fence.** `revision` is the commit the subscription
  starts from, and the first batch is exactly `revision + 1`. Pair it
  with [`tab.list`](#tablist)'s own `revision`: snapshot, discard every
  batch `<=` it, apply the rest.
* **No gaps.** Every commit is a batch, including a commit that
  produced no events, or whose only events a replay's effect-stripping
  filtered out — both arrive as `{"revision": N, "events": []}`. A
  skipped number therefore always means loss, never a quiet commit.
* **The server closes rather than thins.** If a subscriber stops
  reading, falls behind the workspace broadcast, or the connection
  stalls, the server closes the connection instead of dropping events
  out of the stream. A close is still the resync signal: reconnect, and
  either re-subscribe fresh and re-pull `tab.list`, or — with a fence and
  a `session_id` in hand — resume, and let the replay window catch up
  what the gap cost instead of re-snapshotting. Exactly two things ask
  for the close on this wire — `session.stopping`, and an EOF —
  and `session.stopping` only says *why* the stream that is already
  ending ended.

After the flip the connection answers nothing. Frames a client writes
on it are read and discarded (so the server still notices a peer that
goes away), never dispatched and never replied to. That read is also
why a client must keep its write half **open**: half-closing it is how
a peer says it is gone, and the server ends the stream. `session.stop`
labels and closes every subscriber before it drains its in-flight work,
so a client watching a session sees the stream end — with a reason — as
the session goes down.

A non-zero `tab_id_filter` is rejected with `invalid-param` rather than
ignored — HS-2 scope. Silently serving an unfiltered stream to a client
that asked for one tab would make it mis-attribute every other tab's
events.

This op has never gated on anything but the socket's own same-UID
check, and at `SESSION_PROTOCOL_VERSION` `5` there is no other
classification left to describe — every subscriber gets every event.
The history of how this op's shape moved across earlier generations is
in [Versioning](#versioning) and CHANGELOG.

On a **UI socket** the op is still unimplemented: it answers
`{"ok": false, "error": {"code": "not-implemented", "message":
"events.subscribe is not yet implemented"}}` rather than a false ACK,
because a UI process pushes nothing. Callers there poll `tab.list` /
`tab.dump` instead. A UI-side stream lands with its first consumer.

## Session ops

Served **only by a host session** (`roost-session`), never by a UI
socket. A UI socket answers `unknown-op` for every one of them —
including [`tab.attach`](#tabattach), which is a `tab.*` name but a
session-only op — which is how a client tells the two kinds of socket
apart.

There is no connect step, and no order the server enforces: a client
dials, calls `session.identify` to run the compatibility gate (below),
and from there every other op is available immediately, to every
same-UID connection, in any order. What Roost's own client actually
sends is `session.identify` → `session.set_theme` / `session.set_focus`
→ `events.subscribe` / `tab.attach`, with `session.set_agent_hooks`
queued behind them — not because the wire requires that shape, but
because each `set_*` op states something the session would otherwise
guess wrong (its palette, whose window is looking at it, whether the
user wants agent hooks on this machine), and each is re-stated whenever
the client's own answer changes. A client that only wants to watch runs
a shorter sequence — `session.identify` → `events.subscribe` →
`tab.list` — and never calls the `set_*` ops or `tab.attach` at all;
nothing distinguishes that client from any other at the wire level, it
simply chose to send less. `set_agent_hooks` is last and off the
critical path on purpose: it is the only one that touches the
filesystem, so a client queues it rather than waiting on it.

### Session sockets

`roost-session` is a headless daemon: it owns a workspace + PTY
supervisor exactly like a UI does, but with no window and no renderer
attached. It resolves the `Session` bundle profile
([`paths.md`](paths.md#session-profile)) — same `~/Library/Caches/
RoostSession/roost.sock` (macOS) / `$XDG_RUNTIME_DIR/roost-session/
roost.sock` (Linux) socket path `identify.socket_path` names above,
`RoostSessionDev` / `roost-session-dev` in debug builds. Startup installs
`umask 0077` before creating anything, so the state dir, log dir, and
socket directory it creates itself land at `0700` and `state.json` at
`0600`; the socket file gets the same `0600` `IpcServer::bind` chmods
every profile's socket to. On top of that file-mode posture, a session
socket is the one IPC server in this codebase that also checks the
**peer's UID** at accept time (`IpcServer::require_same_uid`) and drops
any connection from a different user — a UI socket does not enforce
this, relying on the `0700`/`0600` directory posture alone. Before the
locks, `validate_runtime_dir` rejects (rather than repairs) a socket
directory some other mode or owner already created, so a session never
silently inherits a loosened directory. On shutdown the session unlinks
its socket only if the path still resolves to the `(dev, ino)` it bound
— a guard against removing a different, later session's live socket at
the same path.

**One session per bundle profile per machine.** Precisely: one per
resolved socket namespace (`XDG_RUNTIME_DIR`, or its fallback) and state
namespace (`ROOST_STATE_DIR`) — the two are pointed elsewhere on purpose
whenever a test, or a second profile, needs an isolated session of its
own. Uniqueness is enforced by two flocks, taken in a load-bearing
order: the socket lock beside the socket first, then the state lock
beside `state.json` (`crates/roost-engine/src/single_instance.rs`). A
second `roost-session start` against the same two namespaces finds the
socket lock already held, reports `already-running`, and exits 0
without touching the state a live session owns; `roostctl session
start` does not take that verdict on faith — it polls `session.identify`
on the socket afterward and only reports success once a session
actually answers.

There is deliberately no `session.list` / `session.create` /
`session.kill`: one profile is one session, so there is nothing to
enumerate or select among. The seam for more than one session on a
single host — a **named** session such as `workbox:agents` (HS-4e) — is
one more path component in the profile resolver (`BundleProfile` →
socket dir + state dir) plus a `--name` flag on `session start` / `stop`
/ `status`; it is not a registry op, and nothing on this wire needs to
change to support it.
`session.identify.session_id` distinguishes **process incarnations** of
one session (it changes across a restart) — it names a run, not a
session.

**The wire is byte-identical over SSH.** A host session reached over an SSH target (host-sessions HS-3) is not a distinct protocol — the client's local bridge socket and the far side's `roost-session client-bridge` are a pure byte pump between this socket and the client's control/events/data connections, so every op and frame on this page crosses SSH exactly as written here. See [Host sessions (development) → Transport: SSH hosts](../development/host-sessions.md#transport-ssh-hosts) for the transport itself (per-connection `ssh` exec over a shared `ControlMaster`, the classified-failure surface); the classifier deciding whether a saved host's `target` is an SSH destination, a socket path, or `localhost` is [`host.add`'s](#host-registry-host) concern, not this socket's.

`roostctl session start|stop|status` address this socket directly; they
are a pre-connect carve-out (`session start` has to work when nothing is
listening at all) and are deliberately **not** reachable through
`--target` / `ROOST_BUNDLE_PROFILE` / auto-detect. Any other op reaches a
session only via an explicit `--socket <path>`. See [`cli.md`](cli.md)
for the verb-level contract (exit codes, `ROOST_SESSION_BIN`).

A session's default tab size is `120x40` (`DEFAULT_TAB_COLS` /
`DEFAULT_TAB_ROWS`) for both restored and freshly-opened tabs, since
there is no window to measure a size from — a UI socket keeps its usual
`80x24` default. On start, a session **hydrates** its saved
`state.json` layout headlessly: it re-opens each tab's saved `{title,
cwd}` as a fresh shell (same "layout, not live state" contract as a UI —
[DL-7](../development/vision.md#dl-7-tabs-persist-as-layout-not-live-state-revised-2026-05-24))
and attaches a per-tab OSC drain to each one so title/cwd/notification
facts keep flowing with no terminal or renderer present. `SIGTERM` and
`SIGINT` converge on the same shutdown path as an IPC-driven
`session.stop`: the signal handler dials the session's own socket and
drives the identical latch → drain → flush → reap sequence described
below. Only if that self-dial fails outright (broken socket) or exceeds
its budget does the daemon fall back to a direct finalization — flush,
socket/lock cleanup, exit — without the mutation barrier or a reap
report; the children then die with the process.

HS-1a shipped with three documented deviations from
[`discovery/host-sessions-architecture.md`](https://github.com/charliek/roost/blob/main/discovery/host-sessions-architecture.md);
**HS-1b (plan 036) resolved all three**, and each is now described where
it belongs: `events.subscribe` was made
[lease-gated](#eventssubscribe) (plan 049, R1 later re-cut this —
subscribing is leaseless again, and the lease classifies the stream
instead), `session.stop` and takeover
[labeled what they closed](#sessionstop) (R15 later left a takeover
closing nothing at all), and terminal-generated queries
are answered by the tab's own server Terminal (below).

Every tab a session spawns now has an authoritative **server Terminal**
behind it — a real libghostty terminal fed synchronously by a per-tab
task, with 2000 lines of scrollback and VT continuation tracking on
from construction. Three consequences a client can observe:

* **Queries are answered, detached.** A program that asks the terminal
  who it is (DA/DA2), where the cursor is (DSR), or what a palette
  entry is set to gets its reply on any tab, whether or not anybody is
  attached. The *server* answers, exactly once; a client that is
  attached and runs its own terminal over the same bytes must discard
  its own reply buffer, or the child sees the answer twice.
* **The terminal is readable.** [`tab.dump`](#tabdump) and
  [`tab.dump_resolved`](#tabdump_resolved) are served from it — and
  since plan 053 `tab.dump` reaches into those 2000 lines of history,
  not just the viewport.
* **The terminal is streamable.** [`tab.attach`](#tabattach) plus a
  [data connection](#data-plane) hand a client the whole terminal as a
  snapshot and then keep it live.

Flow control is the shape of the pipeline, not a policy on top of it:
the PTY reader feeds the tab task over a bounded channel, so a tab that
falls behind stalls its own reader and the child blocks on `write`. The
authoritative terminal never loses bytes; a runaway child pays for it
in backpressure rather than the session paying for it in memory.

### `session.identify`

Params: `{}`. Response:

```json
{
  "app_version": "0.0.18",
  "session_protocol": 5,
  "payload_kinds": ["ghostty-snapshot", "vt"],
  "libghostty_build": "ghostty-3f6b1c9a4d2e5f80+snapshot.v1",
  "session_id": "01K3S8TQ4F0Q9YB2K6WZ5D7XN",
  "started_at": "2026-08-27T14:03:11Z"
}
```

The handshake a client runs before anything binary exists, so every
incompatibility is caught on stable JSON. `session_protocol` is
`SESSION_PROTOCOL_VERSION` — deliberately separate from the
request/response `protocol_version` in [`identify`](#identify), because
the two version different things and move independently. It is **`5`**.
There is no `features` field: at `5` the integer is the whole
negotiation, and there is no other capability a client needs to
detect. **The compatibility rule is one comparison:** a conforming
client checks `session_protocol` for exact equality against its own
constant before anything else, and refuses to proceed on a mismatch —
the [attach handshake](#data-plane) carries the same integer and
enforces it server-side too, before it even looks at the token. Every
op on this page assumes that equality already holds; what came before
`5` — the interactive lease, `session.connect`, per-connection
classification, and the `features` list that once carried
intra-generation capabilities — is history, not this build's contract,
and lives in CHANGELOG rather than here.

`payload_kinds` names what this session can encode a tab's attach
payload as, in no particular order; it is an **open list of strings**,
not a closed enum — a client preserves values it does not recognize and
negotiates on the ones it does. The shipped `roost-session` advertises
both kinds the sample shows, as of plan 053: `ghostty-snapshot`, the
full-fidelity binary format, and `vt`, the replay-a-byte-stream
fallback that architecture §4.4 named as the escape hatch and that is
now built (see [Payload kinds](#payload-kinds) for what each carries).
A pre-053 session advertises `["ghostty-snapshot"]` alone, which is
what makes the list, not the build string, the thing a client's
compatibility gate reads first.

`libghostty_build` is this session's pinned Ghostty build identity,
`ghostty-<first 16 hex of the pinned SHA>+snapshot.v<format version>`.
It is the negotiation for `ghostty-snapshot` **and for that kind
only**: two libghostty builds that disagree cannot exchange a binary
snapshot, so `tab.attach` requires an exact string match before it will
serve one and refuses a mismatch by name (`build-mismatch`) rather than
letting it surface later as a corrupt screen. `vt` has no such
requirement — it is bytes any VT parser replays — so a session
advertising it can still be attached to across a skew, at that
payload's [documented fidelity](#payload-kinds). Both fields were empty
in HS-1a, when there was nothing to attach to.

**Test seam:** with `ROOST_TEST_MODE=1` set, a session additionally
reads `ROOST_SESSION_FAKE_BUILD` and reports *that* string as
`libghostty_build` instead of its real one — and uses it for
`tab.attach`'s check #4 too, so the two stay consistent. Reproducing a
build/protocol mismatch otherwise needs a second binary built against a
second Ghostty pin, which no CI lane can produce; this makes it a
one-line fixture (plan 037 §3.7). Ignored entirely outside test mode,
so a production daemon can never be made to lie about its own build.

A second variable under the same double gate, `ROOST_SESSION_LEGACY_KINDS=1`,
makes the session advertise `["ghostty-snapshot"]` alone — the pre-053
shape. It exists because the `vt` fallback *removes* the state the
restart flow hangs off: with `vt` on offer a skewed client connects
instead of refusing, so without this knob the only end-to-end test of
the "needs restart" path would have had nothing left to reach.

### History: the interactive lease {: #sessionconnect }

Protocol generations 1 through 4 required a client to claim an
**interactive lease** before it could drive a session: a
`session.connect` op minted a bearer token that decided whose
`events.subscribe` stream received `tab.effect`, whose
`session.set_focus` was the one a session suppressed notifications
against, and which connection the `session.set_theme` /
`session.set_agent_hooks` / `session.put_file` ops would accept.
Reconnecting was always a takeover — the same op, with `takeover: true`,
displaced whoever held the lease — and a takeover left every control
and data connection open, demoting only the displaced client's event
stream to a **`session.driver_changed`**-notified observer.

Protocol 5 retired all of it. There is no `session.connect` op, no
`lease` field on any op, and no `session.driver_changed` event; every
same-UID connection is symmetric, as [`session.identify`](#sessionidentify)
states and every section below describes. See CHANGELOG for the
protocol-5 migration entry and [Versioning](#versioning) for the
generation history.

### `session.set_theme`

Seed every tab's server Terminal with the connected client's palette — closes architecture §13's reseed gap (plan 037 §3.6). Same-UID, last-writer-wins: no per-op gate, any connection may send it.

Request:
```json
{"id": "8", "op": "session.set_theme", "params": {
  "osc_colors": {
    "foreground": "#ffffff", "background": "#1c1c1c", "cursor": "#98989d",
    "palette": ["#000000", "... 256 entries total ..."]
  }
}}
```

Response: `{"tabs": 3}` — the number of live tabs whose server Terminal was reseeded. `0` is a success: a session with no tabs yet still remembers the theme for the ones it opens next.

`palette` must carry exactly 256 `#rrggbb` entries (lowercase, the same spelling [`tab.dump_resolved`](#tabdump_resolved) uses); a short or long array is `invalid-param` rather than a partial application.

A client sends this **right after connecting and before its first `tab.attach`** — attaching before the theme lands would paint the session's factory colors for one frame — and again whenever its own theme changes thereafter. Concurrent callers are last-writer-wins by design: the theme store mints a generation on every apply, so a `set_theme` racing a tab spawn is caught up at promotion rather than silently lost, and interleaved fan-outs converge on the newest theme instead of whichever send landed last — including between two different clients' palettes.

Answers `shutting-down` once `session.stop` has latched.

### `session.set_focus`

Tell the session which of its tabs *this connection* is actually looking at. Same-UID, no gate: any connection may state its own focus, and a tab is muted while **any** connection is looking at it (union muting) — see [DL-26](../development/vision.md#dl-26-there-is-no-lease-2026-09-12).

Request:
```json
{"id": "9", "op": "session.set_focus", "params": {
  "focused_tab_id": "5"
}}
```

Response: `{}` — nothing to report beyond "applied".

**Why it exists.** A session is headless: it has no window, so nothing tells it which tab a connected client's terminal widget is actually showing. Without this op the session cannot suppress a notification for the tab you are looking at, so an agent's attention would always fire even while you watch it happen. With it, each connected client states the truth for itself, and the session suppresses a tab's notification while **any** connection says it is looking at that tab — `attention_suppressed_by_focus(tab) = (window_focused && active_tab_id == tab) || focused.values().any(|t| *t == tab)`, where the first clause is the local UI's own window-focus rule and the second is every session connection's stated focus, unioned. Two clients on two different tabs therefore both get muted, correctly, with no coordination between them and no further traffic once each has stated its own tab.

**`focused_tab_id` is required, and nullable.** `null` means "this connection is looking at nothing on this session" — the client's window lost focus, or its selection moved to another host or to a local tab. An **omitted** field is not the same statement and is refused with `missing-param`: a client that forgot to say is exactly the one that must not be guessed for, since guessing "focused" re-creates the mute this op exists to fix.

**Validation order**, for the same reason `tab.attach` pins one — each failure names a different thing to fix:

1. `missing-param` / `invalid-param` — the field is absent, or is neither a decimal-string tab id nor null.
2. `not-found` — a tab id this session does not have.

`not-found` leaves **nothing** applied: a client naming a tab that just closed must not flip its own focus statement onto whatever tab happened to be active. **This op moves nothing else.** It records only this connection's own viewing statement — the session's active selection and persisted selection are untouched, and move only through [`tab.focus`](#tabfocus). Two symmetric clients focusing different tabs therefore never fight over which tab is "the" active one; each simply mutes its own. It does **not** acknowledge the tab's notification either — the client sends [`tab.clear_notification`](#tabclear_notification) for that. Re-stating a focus that is already current is a no-op.

**A connection's focus does not outlive the connection.** The session forgets this connection's statement — reverting its slot in the union to "not looking" — when this same connection sends `focused_tab_id: null`, or when the connection itself closes. Nothing else clears it: another client connecting, focusing a different tab, or disconnecting changes nothing about this connection's own statement.

A client therefore states its focus right after connecting, and again whenever its own window focus or selection moves. A session one release older answers `unknown-op`, which is a refusal like any other: the connection is unaffected and the client keeps the older behavior (the attached tab suppresses its own notifications, headless-default-focused).

Answers `shutting-down` once `session.stop` has latched.

### `session.set_agent_hooks`

Bring the host's agent hook entries in line with the connected client's `agent-hooks` configuration. Same-UID, last-writer-wins: this one writes files under the session user's `$HOME`, and any connected client may send it — the socket's UID check is the only gate.

Request:
```json
{"id": "11", "op": "session.set_agent_hooks", "params": {
  "mode": "auto",
  "skip": ["cursor"],
  "client": "charlie-mbp"
}}
```

Response:
```json
{"wired": ["claude", "codex"], "refreshed": [], "removed": [],
 "skipped": [{"agent": "cursor", "reason": "skip-list"},
             {"agent": "grok", "reason": "not installed"}],
 "errors": []}
```

`mode` is `auto` or `off` — the same two values `agent-hooks` takes in [`config.md`](config.md#agent-hooks); any other spelling is `invalid-param` rather than a silent default in either direction. `skip` is the client's `agent-hooks-skip` list **verbatim**: the host resolves the names and reports back any it does not recognise as a skip with reason `no agent named that (…)`, because only the host can tell a typo from an agent a newer client knows about, and neither is a reason to refuse the run. `skip` may be omitted (an empty list); `client` may not — it is recorded as `by` in the host's state record, which is what makes two clients of one host tellable apart.

**`off` removes, it does not abstain.** On the client's own machine `agent-hooks = off` means "wire nothing", and the UI never opens an agent's config file. Here it means "unwire": a host has no `config.conf` of its own to consult, so the client is the only authority that can tell it to come clean, and an `off` that did nothing remotely would leave a host's entries in place with no way to remove them short of an ssh session. Off is off everywhere.

**`wired` is the toast list, not this call's writes.** It names the agents this host has wired and has never announced to *any* client — the session flips its record's `noticed` for exactly what it reports here, in the same locked write that recorded the wiring, so the sentence "Roost wired agent hooks on ‹host›" appears at most once per agent per host even when two clients connect at the same moment, and including for a wiring done by `roostctl agent ensure` on the host itself. A reconnect, or a second client, gets an empty `wired`. `refreshed` and `removed` *are* this call's writes.

**A per-agent failure is reported, never raised.** A `config.toml` the host could not parse, a read-only file, a file that changed underneath the plan: each is an entry in `errors` beside a successful reply, because the wiring is not what the client dialed in for and must not cost it the session it just attached to. Only a whole-run failure — no `$HOME`, an unwritable state record, an install lock another writer held past its deadline — is an error frame (`internal`).

**The install engine holds one advisory lock per home across plan and apply**, so this op can wait behind another writer's run; neither dropping the client's connection nor the client's own 15 s timeout cancels a run already under way on the host. The wait for that lock is itself bounded — a lock nobody releases is a whole-run `internal` failure rather than a request that never answers, because this op holds the mutation barrier [`session.stop`](#sessionstop) waits on.

**A client sends this right after connecting**, with its own config values, because the op is idempotent and a config edit made since the last connect has no other way to reach the host. It is *queued* rather than chained into the connection sequence: an error there fails the whole attempt, and an ensure on a network-mounted `$HOME` would hold hydration up behind file I/O nothing is waiting on. A session that predates the op answers `unknown-op`, which is a refusal like any other — the connection is unaffected, and the client logs one line, once, for as long as it keeps dialling that host. It is deliberately not one line per connection: the op is re-sent on every connect and a dropped localhost session reconnects on a 250 ms ladder, so the latch outlives the connection, exactly like the fact it records.

**Two clients that disagree flip the files on every reconnect.** Last writer wins, by design: the record stores `by` and `wired_at`, the session logs each run, and `roostctl agent status` on the host shows who did what. Reconciling them is future work.

No new authority: any same-UID client can already [`tab.open`](#tabopen) an arbitrary command on the host.

Served only by a session built with an install backend. A session without one answers `not-supported` rather than reporting an empty success; a UI socket answers `unknown-op` like every other `session.*` op.

Answers `shutting-down` once `session.stop` has latched — it is in the latched set deliberately, because entries wired after a stop would point at a `roostctl` reporting to a socket the session is about to unlink.

### `session.put_file`

Land one client-supplied file on the host and answer with a path a
shell on that host can be told to read. This is the leg underneath
[`tab.send_file`](#tabsend_file): the client uploads each file over a
connection of its own, then pastes the returned paths into the tab.
**Available to every same-UID connection** — uploads land in separate
private directories keyed by random entropy (below), so two clients
uploading at once never collide.

Request:
```json
{"id": "12", "op": "session.put_file", "params": {
  "name": "roost-image-1757083567-8f3a1d0e5b7c42c2.png",
  "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB…"
}}
```

Response:
```json
{"path": "/home/charlie/.cache/roost-session/files/4b9d1e7f0a3c5e21/roost-image-1757083567-8f3a1d0e5b7c42c2.png",
 "bytes": 69}
```

`data` is base64 like every other bytes field. **One frame per file, no
chunking**: the raw cap is 10 MiB (`roost_ipc::MAX_PUT_FILE_BYTES`),
which base64-encodes to ~13.4 MiB and still fits the 16 MiB frame with
the envelope around it — that headroom is the whole reason the op needs
no chunked form. Over the cap is `too-large`, checked twice: the encoded
string's length before the typed decode (the frame and its
`serde_json::Value` already exist by then, so what the early check
spares is the decoded `Vec<u8>`), and the decoded length again after.
The early check is deliberately coarse — base64 carries three bytes per
four characters, so a file one or two bytes over encodes to exactly the
cap's own length and is caught by the exact check instead. Both answer
the same code.

**`name` is the client's, and is never repaired.** It must be a bare
basename: 1–128 bytes of `[A-Za-z0-9._-]`, not `.` or `..`, not starting
with `-`. Anything else is `invalid-param` — not sanitized, not escaped,
not quoted. The reason is the whole point of the op: the path this
answers with is about to be **pasted bare** into an agent's composer,
and bare is the one spelling every agent unquotes identically. A name
that would need quoting is not a name this store can land, so the client
sanitizes a dropped file's basename to that charset before it asks
(`roost_ui_model::file_transfer::sanitize_name`), and a clipboard image
keeps the `roost-image-<nanos>-<16hex>.png` shape the local paste
already uses.

**The whole returned path is paste-safe, and the client re-checks it.**
The file lands at `<files_dir>/<16 hex chars>/<name>` (eight bytes of OS
entropy, one directory per file); the random
directory is what keeps two drops of one name from colliding while the
basename survives, because the agents show that basename in their chips.
At start the session checks its **entire** resolved `files_dir` path
against `^/[A-Za-z0-9._/-]+$` and falls back to a `/tmp` root of its own
if a `$HOME` or `$XDG_CACHE_HOME` put a space, a unicode character or a
non-UTF-8 byte anywhere in it — see [`paths.md`](paths.md#session-profile).
So the op never returns a path an agent cannot parse. The client still
refuses to paste a reply that is not absolute, not in that grammar,
whose final component is not exactly the `name` it sent, or whose
`bytes` is not what it sent: a malformed or hostile reply must never
become typed input.

**The store admits, and never evicts.** One serialized `FileStore` per
session holds a **512 MiB** admission cap
(`roost_engine::ipc::FILE_STORE_CAP_BYTES`); accounting and directory
allocation both happen under one lock, so two connections can never each
be told their file fits the same remaining room. When the next file does
not fit, the op answers `store-full` and **deletes nothing** — a path
this op has handed back may sit unsubmitted in an agent's composer for
an hour, and Roost removing it under the agent is exactly the bug
eviction would have introduced. Clearing a full store means stopping or
restarting the session. The write itself (private temp file, rename,
`0600` under `0700` directories) runs on the blocking pool, because a
session's cache can sit on a network mount and a stalled write must not
take the tokio worker serving other connections with it; a write that
fails removes the whole upload directory, so no partial file is ever
visible under a path this op never returned.

**Lifetime.** A returned path stays valid until the session stops
cleanly or starts again. `roost-session` sweeps `files_dir` **entirely
at start** — a crash or a `SIGKILL` leaves it behind, and the next start
is where that gets cleaned up — and again on the clean wire-stop path,
after the reap. The SIGTERM direct-finalize fallback does not sweep:
its children may still be alive, and the next start sweeps anyway.

`session.put_file` is in the session's **mutating** set, so a slow write
holds the mutation barrier and a racing [`session.stop`](#sessionstop)
waits for it. That is the accepted price of never handing back a path
the sweep has already removed. Once a stop has latched, the op answers
`shutting-down` like every other mutating op.

Served only by a session built with a file store. One that could not
open its root answers `not-supported` rather than refusing to start —
the session still serves everything else. A **UI socket answers
`unknown-op`**, like every other `session.*` op.

### `tab.attach`

Negotiate a payload kind for one tab and get a single-use ticket for
one [data connection](#data-plane). Session sockets only; a UI socket
answers `unknown-op`.

Request:
```json
{"id": "4", "op": "tab.attach", "params": {
  "tab_id": "5",
  "kinds": ["ghostty-snapshot", "vt"],
  "cols": 120,
  "rows": 40,
  "cell_w_px": 9,
  "cell_h_px": 18,
  "libghostty_build": "ghostty-3f6b1c9a4d2e5f80+snapshot.v1",
  "focus": true
}}
```

Response:
```json
{"id": "4", "ok": true, "result": {
  "attach_token": "1a0be5c37d924f68b1c05e3a7f2d8496",
  "kind": "ghostty-snapshot",
  "server_epoch": 6032428321756423947,
  "tab_generation": 3
}}
```

`kinds` is the client's preference order and the server serves the
first entry that is both **servable** and **eligible**; a list mixing
kinds this build has never heard of with one it serves is fine.
*Servable* is what [`session.identify`](#sessionidentify) advertised in
`payload_kinds` — the advertisement is the contract the client
negotiated against, so a kind absent from it is never served even when
the code could produce it. *Eligible* is the kind's own requirement,
and only `ghostty-snapshot` has one: an exact `libghostty_build` match.
`vt` requires nothing, so a client offering
`["ghostty-snapshot", "vt"]` across a build skew lands on `vt` rather
than on a refusal — which is the whole point of the fallback, decided
server-side so a third-party client need not compare build strings
itself. Reversing that preference order gets `vt` on a *matching*
build too; nothing forces the binary format on a client that would
rather replay bytes.

**Validation order is part of the contract**, because each failure
tells the client to fix a different thing and an earlier one must not
be masked by a later one:

| # | Check | Error |
|---|---|---|
| 1 | tab exists with a live terminal | `not-found` |
| 2 | `kinds` contains something servable | `unsupported-kind` (message names both lists) |
| 3 | the negotiated kind's own requirement holds | `build-mismatch` (message names both strings) |
| 4 | `cols` and `rows` both non-zero | `invalid-param` |
| 5 | the tab accepts the geometry (a focused attach only — an unfocused one resizes nothing) | `invalid-param` |
| 6 | token quota not exhausted | `too-many-tokens` |

**An attach takes no authority beyond the socket's own UID check.**
`TabAttachParams` carries no lease or authority field at all: attaching
is reading plus raw input, and both are open to every same-UID client,
as many at a time as it likes.

Checks 2 and 3 stay two separate walks over the offered list rather
than one predicate, because a single pass cannot tell "nothing
servable" from "nothing eligible" and those instruct the client
differently: `unsupported-kind` means *offer something else*, while
`build-mismatch` means *the one kind we could have served needs the
same libghostty on both ends* — the answer a pre-`vt` client's whole
restart flow hangs off. Since plan 053 the second is reachable only
when the client offers no kind but `ghostty-snapshot`, or is talking to
a session too old to advertise `vt`.

Zero `cell_w_px` / `cell_h_px` are legal — a headless client has no
cell metrics to report — but a zero-sized grid is not a grid. That
holds for an unfocused attach too: `cols`/`rows` are this connection's
**declared geometry** either way, and its first `INPUT` or `RESIZE`
frame applies them.

**`focus` says whether this attach claims the tab's geometry.** It
defaults to `true`, and a focused attach is when the server resizes:
between checks 4 and 6 the session resizes the tab (server terminal
*and* `TIOCSWINSZ`) to the requested geometry and waits for that to
land, so the snapshot the data connection is about to encode is already
at client size and needs no post-READY resize. Detach never resizes
back — the PTY keeps the last attached size, so a TUI agent does not
get a `SIGWINCH` because somebody closed a laptop. This does not
contradict that rule: attach is exactly when an in-process Roost
resizes too.

`focus: false` resizes **nothing** — the point of it is a client that
wants to watch a tab without shrinking the one that is typing (a phone
glancing at a desktop's session). Its geometry still counts the moment
it interacts; see the sizing rule beside [`tab.resize`](#tabresize).

**`focus` is required and always spelled out on the wire** — no
default, no omit-when-true shim. `TabAttachParams` is strict, so an
attach that leaves it out is a malformed request, not an unfocused one.

**The accepted handshake reports the geometry its bytes were written
for.** `snapshot_cols` / `snapshot_rows` on the [data
connection's](#the-handshake) accepted reply name that size — the
payload's encode geometry in snapshot mode, the tab's own grid at the
handoff in resume mode — and are present on every accepted reply,
focused or not, either mode. A focused attach did resize the tab, but on
the control connection and before this data connection was dialed; raw
input is open, so another client's geometry-bearing frame can land in
between and resize the tab again, and only the server knows what the
bytes ended up saying. A `vt` client needs the answer: that payload
replays into a terminal *of the attach geometry*, and replaying it at
another width wraps lines and misplaces absolute cursor moves, so such a
client hydrates at this size and resizes its own terminal afterwards. A
resuming client replays the ring into the terminal it kept, which has
the same problem for the same reason. When the size matches what was
asked for the fields change nothing. Both keys are additive — a client
that has never heard of them ignores them, and one reading an older
session's reply simply finds neither.

`attach_token` is 32 hex characters of OS entropy — a **bearer
credential**: never log it, never print it in a failure dump, never
echo it in an error. It is:

* **single-use** — consumed under the registry lock, so two connections
  presenting one token admit exactly one;
* **short-lived** — 60 s TTL (`ATTACH_TOKEN_TTL`), a protocol constant
  that is *not* scaled by `ROOST_TEST_TIMEOUT_SCALE`. A session started
  with `ROOST_TEST_MODE=1` honors `ROOST_SESSION_ATTACH_TTL_MS` to
  shorten it, which is how the expiry case is tested in seconds; a
  production daemon ignores that variable entirely;
* **quota-bounded** — at most 16 unconsumed tokens
  (`MAX_OUTSTANDING_TOKENS`) exist at once, and at most 8
  (`MAX_TOKENS_PER_CONNECTION`) of them on any one control connection.
  Past either bound, minting is refused with `too-many-tokens` rather
  than evicting a token some other connection is about to present.
  Reaching one means a client minted tickets inside one TTL and dialed
  none of them; a healthy attach consumes its ticket within a round
  trip. The per-connection share is what keeps a single looping client
  from answering everybody else's attach with `too-many-tokens`;
* **connection-bound** — reclaimed when the connection that minted it
  closes, which is what keeps the quota above from being held for a
  whole TTL by a client that minted its share and vanished. A token is
  bound to the connection that minted it, never to anything else, so
  nothing about another client connecting, or this one reconnecting,
  purges it early;
* **pipeline-bound** — stamped with the `tab_generation` below, so a
  respawn in the same window is a clean `not-found` rather than a
  stream from a different terminal under the old identity.

`server_epoch` and `tab_generation` are the **resume identity**. The
epoch is a random value minted once per session process; the generation
counts tab pipelines within it. A client that later wants to resume a
stream hands both back, and the randomness is what makes a restarted
session's streams unresumable *by construction* rather than by luck — a
monotonic counter would collide across a restart and silently accept a
stale stream. Both ride as **bare JSON numbers**, not the
string-wrapped int64 ids use: they are counters, not ids. Neither can
exceed `i64::MAX` — the epoch is deliberately 63 random bits, not 64,
because a top-bit-set value round-trips imprecisely through a decoder
that falls back to `Double`, and the whole point of the field is an
exact match.

`tab.attach` answers `shutting-down` once `session.stop` has latched.

### `session.stop`

Params: `{}`. Response: the reap report,

```json
{"reaped": ["3", "5"], "killed": ["8"], "abandoned": ["9"]}
```

Stops the session. In order: the session latches *stopping* (every
mutating op from that point answers `{"code": "shutting-down"}`, reads
keep answering, and a second `session.stop` gets `shutting-down` too);
it **labels and closes every connection the session ever admitted** —
every control connection, every data connection on every tab, and every
event stream, regardless of which client opened it — an
events connection gets the terminal
`{"event": "session.stopping", "data": {"reason": "stop"}}` envelope, a
data connection gets an `ERROR` frame with code `shutting-down`, a
plain control connection just closes; it waits out the mutating
requests already in flight, so a `tab.open` that got past the latch
completes and its tab is included below; it flushes the workspace
layout; then it hangs every PTY up, escalating to `SIGKILL` after a
soft deadline.

The labeling comes **before** the relays are torn down, on purpose: cut
the relay first and the peer gets a bare EOF it cannot tell from a
crash. It is still best-effort under a 2 s deadline — a peer that
stopped reading gets EOF, which remains a valid signal.

The three id lists partition the tabs that were live when the stop
began — each id appears in exactly one, and each is sorted. `reaped`
died on the hangup; `killed` was still live at the deadline and was
SIGKILLed; `abandoned` was still unreaped after the post-kill tail and
the session stopped waiting for it. Ids are string-encoded like every
other id on this wire.

The reply is written **before** the process-level shutdown tail runs, so
a client always gets its report even though the session is on its way
out.

### Data plane

One connection per attached tab, carrying one tab's terminal: a
snapshot of what is on screen now, then everything that happens next.
It shares the session's socket path with the JSON control plane but not
its framing — after a one-line handshake the wire turns binary and
stays that way.

Why a second connection at all: the control plane is serial
request→response, so keystrokes would queue behind their own acks and a
slow op like [`tab.dump`](#tabdump) would head-of-line-block typing.
The data connection is unacknowledged and bidirectional, which is what
keeps input latency flat under control-plane load.

#### The handshake

The **first line** a connection writes decides what it is. A JSON
object carrying `attach` and **no** `op` is a data handshake; anything
else — an op-carrying envelope (even one that also has `attach`), a
non-object, malformed JSON — stays a request connection and behaves
exactly as it did before the data plane existed. The test applies to
the first line only, so a request stream can never be diverted
mid-flight by a payload that happens to look like a handshake.

```json
{"attach": "1a0be5c37d924f68b1c05e3a7f2d8496", "protocol_version": 3,
 "resume_from_seq": 8814, "server_epoch": 6032428321756423947,
 "tab_generation": 3}
```

`attach` and `protocol_version` are required; the resume triple is
optional and all-or-nothing in practice (see [Resume](#resume) below).
Decode is **permissive** — a newer client may carry fields this build
has never heard of, and refusing the whole handshake over one would
turn an additive change into a hard incompatibility.

**Scope:** this sniff exists on Rust-served sockets only. The Mac UI's
Swift IPC server has no data plane and is untouched — a handshake line
there gets the `parse-error` it always got. A Rust **UI** socket
recognizes the shape and answers `not-supported`, which is a different
and more useful thing to tell a client than "your JSON is bad".

The reply is one JSON line. Accepted:

```json
{"ok": true, "kind": "ghostty-snapshot", "mode": "snapshot",
 "seq": 8813, "server_epoch": 6032428321756423947, "tab_generation": 3}
```

Rejected — then the connection closes, and **nothing binary is ever
written**, so a client that got a refusal never has to guess whether
the bytes after it are frames:

```json
{"ok": false, "error": {"code": "invalid-token", "message": "..."}}
```

| Code | Meaning |
|---|---|
| `protocol-mismatch` | wrong `protocol_version`. Checked **before** the token: the two ends disagree about what a token even is, and `invalid-token` would send the client hunting for the wrong bug. |
| `invalid-token` | unknown, expired, already-used, or minted by a control connection that has since gone away (see [`tab.attach`](#tabattach)). |
| `not-found` | the tab has no live terminal, or was respawned between `tab.attach` and this handshake. |
| `snapshot-failed` | the terminal could not be encoded right now. Re-attach is the recovery — it is about this instant, not about the client. For `vt` this also covers a terminal whose VT parser sits mid-sequence with no retained continuation for the whole attach budget: the encode is parked and retried after each further chunk rather than emitting a payload that would desync the client, and the budget is what bounds that wait. |
| `shutting-down` | `session.stop` has latched. |
| `parse-error` | the handshake line did not decode. |
| `not-supported` | this socket serves no data connections. |

`mode` is `"snapshot"` or `"resume"`, and `seq` is the **fence**: the
client has everything up to and including it, and the first `PTY` frame
carries `seq + 1`. In snapshot mode the fence is the snapshot's own
encode point; in resume mode it is `resume_from_seq - 1`.

`kind` is the kind [`tab.attach`](#tabattach) actually negotiated,
carried here on the token rather than assumed — **this reply is the
authoritative one**, and it is what a client selects its decoder from.
The control-plane `TabAttachResult.kind` must agree; a client that sees
them disagree treats it as `protocol-error` and re-attaches rather than
guessing which to believe.

`snapshot_cols` and `snapshot_rows` name **the size the bytes that
follow were written for** — the snapshot's own encode geometry in
`"snapshot"` mode, the tab's grid at the handoff in `"resume"` mode —
and are present on every accepted reply, `focus: true` included, because
a focused attach resizes the tab from the control connection and any
other client may resize it again before the encode or the handoff runs.
A `vt` client builds its terminal at that size before replaying, then
resizes it to its own; a resuming client replays the ring into the
terminal it kept, and the geometry it was away for may not be the one it
left. See [`tab.attach`](#tabattach). Absent only from a reply a session
predating `open_input` writes.

#### Preamble and frames

After an accepted reply the server writes the 8-byte magic
`ROOSTDP2` — a client that reads anything else has negotiated with a
host it cannot talk to and must not try to parse what follows — and
then frames flow both ways:

```text
frame := u32-LE payload length | u8 type | payload
```

| Type | Dir | Payload |
|---|---|---|
| `0x01` `SNAP` | S→C | the next bytes of the encoded snapshot stream |
| `0x02` `PTY` | S→C | `u64-LE seq` \| raw PTY bytes |
| `0x03` `EXIT` | S→C | `u64-LE final_seq` \| `i32-LE` exit code |
| `0x0F` `ERROR` | S→C | JSON `{code, message}`; the connection closes after it |
| `0x11` `INPUT` | C→S | raw encoded key/paste bytes, ordered and unacknowledged |
| `0x12` `RESIZE` | C→S | `u16-LE cols` \| `rows` \| `cell_w_px` \| `cell_h_px` |

Rules, all fatal (best-effort `ERROR`, then close). Only the first is
the framer's — it has to be, because it bounds the allocation made
before anyone sees the frame; the rest belong to the endpoint that
knows the protocol state, which is why a client validates the widths of
what the server sends and vice versa:

* **1 MiB per payload** (`MAX_DATA_FRAME_BYTES`), both directions,
  reader and writer. A client with a bigger paste **splits it across
  `INPUT` frames** — this is the client's job, not something the server
  will do for it. The server splits an oversized snapshot record across
  `SNAP` frames for the same reason. A `vt` payload is split at a
  smaller **64 KiB** (`VT_SNAP_FRAME_BYTES`), which is a hold-window
  fix rather than a wire-limit change — see [Payload
  kinds](#payload-kinds).
* Fixed-width payloads are exactly that width: `PTY` ≥ 9 bytes,
  `EXIT` == 12, `RESIZE` == 8.
* A zero-length payload is meaningful only for `SNAP`, where it is the
  `vt` payload's terminator (below). An empty `INPUT` is a
  `protocol-error`.
* An unknown type byte is a `protocol-error` naming the byte. The
  framer hands it up rather than failing the decode, precisely so the
  endpoint can name it.

`EXIT` is **always the last frame** on a connection that sees one, and
`final_seq == last PTY seq + 1` — the exit consumes an ordinal of its
own, so a client that has applied `PTY` frame `final_seq - 1` knows it
missed nothing. Pixel dimensions on `RESIZE` are load-bearing, not
decoration: the server terminal's resize and mode-2048 size reports
both need them, and they are part of the geometry the tab compares
against (see [`tab.resize`](#tabresize)). A `RESIZE` naming zero cols
or zero rows is **ignored**, not fatal and not applied —
[`tab.attach`](#tabattach) refuses a zero-sized grid and the two state
the same client's geometry, so they have to agree about what a grid is.

Both `INPUT` and `RESIZE` size the tab: a `RESIZE` applies and becomes
the connection's declared geometry, and an `INPUT` applies that
geometry ahead of its bytes when the tab is at another one. Neither is
answered, and the server never tells an attached client that somebody
else resized the tab under it — a client at another size sees wrapping
until it next interacts.

Ordering, once the stream is running:

1. The whole **ready prefix** goes out at full speed, and live `PTY`
   frames are absorbed but **held** until it has. A client has no
   terminal to apply a `PTY` frame to before that point, and making it
   buffer them would move this queue into every client. For
   `ghostty-snapshot` the prefix ends at Ghostty's own READY record and
   the window is tiny — that prefix is just the active screen. For
   `vt` the prefix is the **whole payload**: a replayed byte stream has
   no partial-render marker, so the hold lasts as long as the payload
   does at the client's link speed.
2. After the ready prefix, `PTY` leads: a keystroke's echo must not
   wait behind a scrollback page.
3. But the snapshot cannot be starved. At least one `SNAP` frame goes
   out after 256 KiB of `PTY` payload **or** 50 ms since the last one,
   whichever comes first — a `yes`-style producer would otherwise hold
   FINISH off for as long as it kept running, and a slow-but-endless
   one would never trip a byte floor at all.
4. `EXIT` goes out only once everything before it has — the `vt`
   terminator included, even for a tab that exits mid-payload.

`ERROR` frames carry a stable code:

| Code | Meaning |
|---|---|
| `desync` | the stream cannot be trusted: a gap or duplicate `seq`, a lagged tee, a snapshot that blew the attach budgets. Re-attach. |
| `overflow` | the peer is not reading — 8 MiB queued for it, or a single write past its deadline. |
| `shutting-down` | `session.stop` latched. |
| `protocol-error` | the client sent something the framing forbids. |

A gap is fatal rather than papered over because the client's terminal
would silently diverge and could never tell — and re-attach already
rebuilds from a fresh snapshot, so there is nothing to gain by
continuing. Every `ERROR` is best-effort: a peer that stopped reading
made the write impossible, and EOF is the accepted fallback everywhere
a label is promised.

#### Payload kinds

Two, negotiated at [`tab.attach`](#tabattach) and stated back on the
[handshake](#the-handshake). They differ in fidelity and in how a
client knows the payload has ended.

**`ghostty-snapshot`** is libghostty's own binary format. Its record
structure (GHOSTSNP: envelope, READY, history pages, FINISH) rides
*inside* `SNAP` frames as an opaque byte stream — no record alignment,
since Ghostty designed the format to be embedded and the client's
decoder buffers to record boundaries itself. Ghostty's READY is the
only READY in this design; the transport adds no second marker, and
FINISH is how the client knows it has the whole thing. Both ends must
be the same `libghostty_build`, which is what makes it a full-fidelity
mirror and also what makes it refusable.

**`vt`** is a plain VT byte stream: escape sequences and text that any
conforming parser replays into a fresh terminal of the attach geometry
to reproduce the server's active screen — its scrollback, viewport,
cursor, pen, tabstops, scrolling region and an allowlist of modes,
plus the parser's unfinished input so the first live `PTY` frame
completes a sequence the fence cut exactly as it does on the server. It
carries no build requirement, so it is what a client whose libghostty
disagrees with the session's attaches with instead of being refused.

Three transport consequences follow from `vt` having no internal
framing of its own:

* **One zero-length `SNAP` frame terminates it.** A byte stream has no
  end marker, so the transport supplies one. It is written in the same
  step as the final payload frame — never a later pass, because a later
  pass could flush held `PTY` or write `EXIT` first, and both "no `PTY`
  precedes the terminator" and "`EXIT` is last" have to hold. It is
  sent unconditionally. A non-empty `SNAP` after it is a
  `protocol-error`; re-attach. `ghostty-snapshot` sends no such frame,
  and a client under that kind already treats an empty `SNAP` as a
  no-op.
* **Frames are capped at 64 KiB** (`VT_SNAP_FRAME_BYTES`), not the
  wire's 1 MiB. The pump awaits a whole frame's write inline and drains
  the tab's output tee only between writes, so holding `PTY` for a
  1 MiB frame lets a busy child lag the tee and trip `desync` — the
  unrecoverable code — where the smaller frame raises the
  child-to-link ratio the tee tolerates 16× and pushes slower producers
  into the bounded, recoverable `overflow` path instead.
* **First paint waits for the whole payload**, since rule 1's prefix is
  all of it. On a slow link that trails what `ghostty-snapshot` would
  have painted at READY.

**What a `vt` attach does not carry.** Stated here because a client on
this kind is showing a real screen with real gaps in it, not a
degraded-but-equivalent one:

* the **inactive screen** — a tab attached while the alternate screen
  is up gets an empty primary when the program exits alt mode
  (`ghostty-snapshot` carries both);
* **soft-wrap flags** — a wrapped row replays as a hard row, so copy
  and reflow-on-resize differ from the server's;
* **per-cell hyperlinks** — VT content emits `OSC 8` only for HTML, so
  only the pen's link survives and **link hover and click stop working
  on such a tab**. Roost injects `FORCE_HYPERLINK=1` into every tab, so
  this is a visible loss, not a theoretical one;
* **text-free background-only rows in history** — a row erased under a
  background color with no text in it. Viewport rows of that shape are
  refilled explicitly; history rows cannot be, there being no cursor
  addressing into history;
* **the saved cursor** (`DECSC`) and the **kitty-keyboard stack** —
  only the live cursor and the current kitty flags are carried;
* **modes outside the carried allowlist**, deliberately: `DECCOLM`
  would resize the client away from the attach geometry, `?1048` moves
  its cursor, mode `2048` and the visibility report *write a reply to
  the PTY* on enable, and `?2026` can latch synchronized output and
  freeze rendering mid-frame. A program that set two members of one
  mouse-tracking or mouse-format family replays as the last of them,
  since libghostty collapses each family onto a single field;
* **Kitty images** — same as `ghostty-snapshot`;
* **pending-wrap under origin mode** — restoring `DECOM` homes the
  cursor in libghostty, so the repositioning that follows clears the
  flag. This is the one cursor state `vt` cannot reproduce;
* a **program color override that happens to equal the server's own
  default** is indistinguishable from "unset" and is not carried. Only
  the program's overrides ride the payload at all — the client keeps
  its own theme.

#### Resume

A client that already holds a tab's stream up to some seq can ask for
the rest instead of a whole new snapshot: send `resume_from_seq`
together with the `server_epoch` and `tab_generation` the original
`tab.attach` returned. On a hit the reply says `mode: "resume"`, no
`SNAP` frames are sent at all, and the tab's replay ring (2 MiB,
oldest evicted) plays back as ordinary `PTY` frames ahead of the live
ones — through the same contiguity walk, so a hole in the ring is as
fatal as a hole in the live stream.

Resume is honored only when **all** of these hold:

* `server_epoch` matches this session process exactly. A restarted
  session mints a fresh random epoch, so a pre-restart stream cannot
  match — by construction, not by luck.
* `tab_generation` matches the tab's current pipeline, so a respawned
  tab's seq space is never streamed under the old one's identity.
* `ring_front <= resume_from_seq <= last_assigned + 1`. The upper bound
  is inclusive: `last_assigned + 1` is a valid **empty-slice** resume —
  the client missed nothing and simply carries on.

Every miss — `resume_from_seq` of `0`, a seq past the end, an evicted
range, an identity mismatch, a tab task that went away — falls back to
`mode: "snapshot"` and a full attach **in the same reply**. A resume
failure is never an error, and a client never has to handle one: it
reads `mode` and does what it says.

#### Many connections per tab

A tab admits as many data connections as clients dial. Each gets its
own receiver on the tab's output tee, its own fence, its own snapshot
and its own [budgets](#budgets); nothing is shared, so a peer that
stops reading is cut on its own lag and takes none of the others with
it. Every admitted connection receives the same PTY bytes and every one
of them may send `INPUT`. There is no supersede: a second attach never
closes the first, and connecting from anywhere else never closes
anything that was already open.

**The tab is sized by whichever of them interacted last**, per the rule
beside [`tab.resize`](#tabresize): a client that only watches (attached
with `focus: false`, never typing) never changes the size out from
under the one that is working.

What bounds the count is not a per-tab limit: it is the token quota (16
unconsumed tickets per TTL window, 8 of them per control connection —
see [`tab.attach`](#tabattach)) and the session's 4 concurrent snapshot
encodes. Over time the number of
admitted connections is open, which is the honest statement — the
per-attach machinery is what keeps that affordable.

Client disconnect at any point simply aborts that one forwarder; the
tab keeps running, the other connections keep streaming, and no partial
state survives. That is the difference between detaching and stopping:
dropping the socket leaves the session exactly as it was.

#### Budgets

The budgets are behavior a client can hit, not tuning knobs:

| Budget | Value | On breach |
|---|---|---|
| Snapshot half of an attach, in time | 60 s from the fence | `ERROR desync` |
| Snapshot half of an attach, in bytes | 512 MiB (snapshot + live PTY written alongside it) | `ERROR desync` |
| Queued-but-unwritten PTY bytes | 8 MiB | `ERROR overflow` |
| A single stalled write | the push write deadline | `ERROR overflow` |

A slow consumer therefore gets cut off deterministically instead of
growing the session's memory, and re-attaching immediately afterwards
works — there is no thrash loop and no cooldown.

The 60 s budget now also covers the **encode**, not just the streaming
after it: the fence awaits the encoder under that timeout, so a
terminal that never reaches an encodable state fails the attach with
`snapshot-failed` instead of hanging it. That is what makes the wait a
`vt` encode can take (parking mid-sequence, above) bounded — and it
fixed the same hole for `ghostty-snapshot`, where a wedged encode
previously awaited forever.

**One ordering caveat, documented rather than solved:** control-plane
[`tab.write`](#tabwrite) and data-plane `INPUT` frames have no
cross-channel ordering guarantee. They arrive at the tab task in
whatever order they reach it. `tab.write` is administrative,
low-frequency scripting; interleaving it with live typing is not a
supported pattern.

## Events

Server-push only, delivered on a host-session socket after
[`events.subscribe`](#eventssubscribe). Each envelope is a
`{"event": "<name>", "data": {...}}` object inside an `EventBatch`;
several envelopes can share one batch, which is what makes a commit
atomic on the wire. The set below is exhaustive — the serializer
(`crates/roost-engine/src/event_push.rs`) is a total match over the
workspace's event enum, so a new event cannot ship without a name
here.

`session.stopping` is deliberately **not** in this set: it is not a
workspace event, it carries no `revision`, and it never rides inside a
batch — it is the connection's own terminal control envelope, delivered
outside the batch discipline, the last frame before the stream closes.
See [`events.subscribe`](#eventssubscribe) for the full envelope shape.

* `tab.opened` — `{"tab": <Tab>}`.
* `tab.closed` — `{"tab_id": "<id>"}`.
* `tab.state_changed` — `{"tab_id": "<id>", "state": "<TabState>"}`.
* `tab.title_changed` — `{"tab_id": "<id>", "title": "<string>"}`.
* `tab.cwd_changed`   — `{"tab_id": "<id>", "cwd": "<string>"}`.
  Note: when an OSC 7 (or `tab.set_cwd`-equivalent) lands on a tab
  whose `user_titled` is false, the workspace also re-derives the
  title from the basename of the new cwd. Subscribers will see a
  `tab.cwd_changed` immediately followed by a `tab.title_changed`
  (in that order, cause-then-effect) for that single op — treat
  them as a pair, not as one-event-per-op. On shells with the
  shipped integration, a further `tab.title_changed` arrives a
  prompt cycle later (OSC 0 → tilde-abbreviated full path).
* `tab.notification`  — `{"tab_id": "<id>", "has_pending": <bool>}`.
* `project.created`   — `{"project": <Project>}` (tabs empty).
* `project.renamed`   — `{"project_id": "<id>", "name": "<string>"}`.
* `project.deleted`   — `{"project_id": "<id>"}`.
* `active.changed`    — `{"project_id": "<id>", "tab_id": "<id>"}` (either may be `"0"`).
* `tabs.reordered`    — `{"project_id": "<id>", "tab_ids": ["<id>", ...]}`. The full post-reorder display order for that project, not a diff.
* `projects.reordered` — `{"project_ids": ["<id>", ...]}`. The full post-reorder sidebar order.
* `hook_active.changed` — `{"tab_id": "<id>", "active": <bool>}`.
* `notification.fired` — `{"tab_id": "<id>", "title": "<string>", "body": "<string>"}`. Mirrors the legacy proto's `NotificationEvent`; useful for tools that mirror notifications elsewhere.
* `agent_report.changed` — `{"tab_id": "<id>", "shell_state": "<ShellState>", "agent_lifecycle": "<AgentLifecycle>", "ownership": "<Ownership, omitted when unowned>", "state": "<TabState>", "hook_active": <bool>}`.
  Fires whenever an accepted `tab.agent_report` or an OSC 133 shell
  mark changes the agent record. `tab.state_changed` and
  `hook_active.changed` still fire for their (derived) slices, so
  existing subscribers keep working unmodified — this event carries
  what those two projections lose: which lifecycle, whose session, and
  the shell axis underneath. `state` is included pre-derived so a
  subscriber never has to re-run the projection itself.
* `tab.effect` — `{"tab_id": "<id>", "effect": "bell" | "clipboard-write", "data"?: "<base64>", "target"?: "system" | "selection"}`.
  One side effect the tab's OSC scan produced, fanned out to **every**
  subscriber unfiltered — a bell BEL byte outside any escape sequence,
  or an OSC 52 clipboard write. What a client does with it is its own
  policy: a bell is worth marking on any tab that owns one, while a
  clipboard write should be applied only to the tab that client is
  actually viewing (see [`events.subscribe`](#eventssubscribe) and the
  guide's [Several windows on one session](../guides/host-sessions.md#several-windows-on-one-session)).
  `data` and `target` are present only for
  `clipboard-write`: `data` is the decoded payload, base64-encoded like
  every other bytes field on this wire and capped at 256 KiB decoded
  (`CLIPBOARD_EFFECT_MAX_BYTES` — an oversized write is dropped and
  debug-logged by size, **never by content**); `target` distinguishes
  OSC 52's primary-selection form (`p`/`s`) from the system clipboard
  (`c` or no selector), defaulting to `system` when absent. HS-2 ships
  exactly these two effects — every other client-local OSC effect
  (pointer shape, today) stays dropped + debug-logged in the tab task
  rather than added to this envelope.

## Versioning

Two integers version this wire independently. `identify.protocol_version`
is the UI socket's schema version — currently **`1`**
(`roost_ipc::PROTOCOL_VERSION`); it is reported by
[`identify`](#identify) but nothing compares it, so the UI socket has no
handshake gate. `session.identify.session_protocol` is the session
sockets' — currently **`5`** (`roost_ipc::messages::SESSION_PROTOCOL_VERSION`),
covering both the session JSON ops and the binary [data
plane](#data-plane); conforming clients check it for equality before
anything else and the [attach handshake](#tabattach) refuses a mismatch.

**The rule, stated once:** an addition bumps the session integer only
when a pre-bump peer could not refuse it meaningfully; everything else
is a plain additive change (a new optional field, a new op, a new event
name an old client can ignore) and moves nothing. There is no
intra-generation capability channel today — the integer is the whole
negotiation. The generation-by-generation history of what each bump
changed, including the lease's own retirement at `5`, is in CHANGELOG,
not here: this page describes the current contract, and a bump's
rationale belongs with the change that made it, once.

**The consumption and compatibility policy lives in
[`ipc-compatibility.md`](ipc-compatibility.md)** — what is additive in
which direction, which enums are closed, when either integer bumps, and
how an external project depends on `roost-ipc`.

## See also

[`docs/archive/ipc-legacy-proto.md`](../archive/ipc-legacy-proto.md)
has the mapping from the pre-JSON protobuf protocol to this one, for
anyone still holding a client written against it.
