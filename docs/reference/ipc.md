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
  `not-implemented`, `internal`, `shutting-down`. Clients should treat
  unknown codes as fatal for the request and surface `message` to the
  user. `shutting-down` is **session-socket only**: once
  [`session.stop`](#sessionstop) latches, every mutating op answers it
  (reads still answer normally), and a second `session.stop` on the
  same session gets it too instead of a fresh reap report. Session
  sockets add a further six, all of them about the lease and the attach
  handshake: `connect-required`, `already-connected`, `taken-over`,
  `too-many-tokens`, `unsupported-kind`, `build-mismatch` — see
  [`session.connect`](#sessionconnect) and [`tab.attach`](#tabattach).

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

Response: `{}`.

### `tab.resize`

Headless resize of a tab's PTY (issues `TIOCSWINSZ`, which fires
`SIGWINCH` to the child group).

Request: `{"params": {"tab_id": "3", "cols": 100, "rows": 24}}`.
Response: `{}`.

### `tab.dump`

Read the tab's live terminal *viewport* as text — the determinism
backbone for automated tests (assert on exact content instead of
OCR/pixel-matching a screenshot). Both UIs walk libghostty-vt's render
state on the main thread. Viewport only for now (scrollback is a planned
follow-up, so no `scrollback` param is accepted yet).

Request: `{"params": {"tab_id": "3"}}`.
Response:

```json
{"cols": 120, "rows": 30,
 "cursor": {"row": 1, "col": 14, "visible": true},
 "rows_text": ["/tmp $ echo hi", "hi", "/tmp $", ""]}
```

`rows_text` has one entry per visible row, trailing blanks trimmed (a
blank cell renders as a space so columns line up). `cursor` is omitted
when the cursor is off-viewport. Response is permissive, so per-cell
color / scrollback fields can be added forward-compatibly. CLI:
`roostctl tab dump --tab N` (plain rows) / `--json` (full result).

On a **host-session socket** this is answered from the tab's server
Terminal instead of a UI's — same request, same response shape. It
needs no lease and no attach: a session's terminal is authoritative
whether or not anybody is watching it. (Before HS-1b a session had no
terminal at all and answered `internal: no UI attached`.)

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
applied today, on either socket.)

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
relative order after the listed ones.

Response: `{}`.

### `project.reorder`

Request: `{"params": {"project_ids": ["2", "1", "3"]}}`. Order is
topmost first. Same partial-order rules as `tab.reorder`. Response:
`{}`.

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
| `attention` | `"set"` \| `"clear"` \| `"preserve"` | defaults to `"preserve"` |
| `severity` | `"info"` \| `"warn"` \| `"error"` | defaults to `"info"`. Carried on the model now so a later policy revision can have `failed` interrupt regardless of focus; v1's notification policy does not yet consult it |
| `title` / `body` | string | required when `attention == "set"` (`invalid-param` if missing), ignored otherwise |
| `detail` | string | free-form reason for the report (`"permission_prompt"`, `"background_tasks:2"`, an error name…); recorded onto the ownership record when non-empty |
| `metadata` | map<string, string> | **open extension channel.** The params struct carries `#[serde(deny_unknown_fields)]` per repo convention, so a new *named* field on this op would not actually be additive — both server implementations would need to change. `metadata` is the channel that genuinely is |

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
{"id": "agent:3", "title": "roost · slauth-refactor",
 "agent": {"effective_lifecycle": "waiting", "project": "roost",
           "name": "slauth-refactor", "status_text": "Waiting for input",
           "time_text": "2m", "metrics_text": "4f +86 -12"}}
```

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
| `clipboard.write` | `{"target": "...", "text": "..."}` | Test-only pasteboard seeding (lets a roosttest case set a known value before asserting paste behavior). Not gated: any process on the host can already write the OS clipboard. |

`roostctl` does not surface these yet — they exist for end-to-end test
coverage (`tools/roosttest/`) and as a stable surface a future scriptable
selection-driving feature (AI agent highlighting a region for the user
to confirm) could build on. Each op routes through the UI seam
(`UiRequest::Selection*` / `UiRequest::Clipboard*` on Linux, the
`UiBridge` protocol on Mac), not the workspace — pasteboard + selection
state live on the UI side.

### Host registry (`host.*`)

Client-side saved-host bookkeeping for [host sessions](../guides/host-sessions.md) — a UI-only op family, like `palette.*`: it manages `Workspace.hosts` (the `{id, label, target, last_connected}` array persisted in the UI's own `state.json`, plan 037 §3.5), not a session's own workspace. `host.connect` / `host.disconnect` additionally reach into the UI's live connection set, so they answer with the connection state the request left the host in rather than only a registry mutation — and they require a UI to be attached at all (`internal: no UI attached` on a headless engine embedder, the same honest failure every other `UiRequest`-backed op gives).

A session socket answers `unknown-op` for every verb here — the same "no shadow registry in the daemon" rule `host.add --verify`'s own dial depends on. So does the Swift Mac app's socket: **`roostctl host *` against `--target mac` is a documented, permanent `unknown-op`**, not a gap to be filled — host sessions are iced-only (plan 037 §3.1), and the Swift app never grows this surface.

| Op | Request params | Notes |
|---|---|---|
| `host.add` | `{"label": "pop-os", "target": "/home/charlie/.local/state/roost/roost-session.sock"}` | Saves a host. `target` is carried **opaquely by this op** — the workspace stores whatever string it's given, unvalidated — so classifying it (`roost_ipc::ssh::classify`: an SSH destination like `workbox` / `user@host` / `ssh://user@host:port`, only the `ssh://` spelling carrying a port; a Unix socket path, containing `/`; or the `localhost` sentinel) and refusing an unclassifiable string (empty, a leading `-`, a bare `host:port` with no scheme) is each **caller's** job, done client-side before this op is ever sent — `roostctl host add` and the Add Host dialog both classify first and never call this op on a target that fails. Registry-only beyond that — this does **not** dial `target`, so a typo'd-but-classifiable target still saves cleanly (the sidebar's dot reports it at the next connect attempt). `label` is trimmed, must be non-empty, Unicode-case-insensitive unique, and not `"local"` (the reserved LOCAL band). Response: `{"host": <Host>}`. |
| `host.remove` | `{"id": "3f9a2b7c1d4e4f5a"}` | Forgets a saved host — the registry entry and the dimmed rows its last connection left behind. Never touches the session itself: its shells keep running. Response: `{}`. |
| `host.list` | `{}` | Response: `{"hosts": [<Host>, ...]}`. |
| `host.connect` | `{"id": "3f9a2b7c1d4e4f5a"}` | Starts (or restarts) a connection. Unconditional takeover — reconnecting IS takeover on this wire — and on a **localhost** target it spawns the session first if nothing is listening. Answers as soon as the attempt is under way, with the state the request *asked for* (`"connecting"`), not the far end's eventual verdict — watch the sidebar or poll `host.list` for the settled state. Response: `{"host": <Host>, "state": "connecting"}`. |
| `host.disconnect` | `{"id": "3f9a2b7c1d4e4f5a"}` | Drops the connection. Never stops the session — its shells keep running, and reconnecting picks them back up (disconnect ≠ stop). Response: `{"host": <Host>, "state": "disconnected"}`. |

`Host` is `{"id": "<hex>", "label": "<string>", "target": "<string>", "last_connected"?: "<ISO-8601>"}`. `state` is one of the wire's connection-state spellings: `disconnected` | `connecting` | `connected` | `taken-over` | `stopped` | `needs-restart`.

`host.add` / `host.remove` are `UiRequest`-style when a UI is attached (a `roostctl host add` is visible in the sidebar immediately, no restart needed) and fall back to a direct `Workspace` mutation for a headless embedder (the engine's own tests) — both paths mint the same `WorkspaceError` wire codes, so a caller cannot tell which one answered. `host.connect` / `host.disconnect` have no headless form: connection state is the app's alone.

### `events.subscribe`

Turn this connection into a one-way event stream. **Served by a
host-session socket only**, and **lease-gated**: the caller presents
the `lease` [`session.connect`](#sessionconnect) handed it.

Request: `{"params": {"lease": "9f2c…6b83", "tab_id_filter": "0"}}`.
Response (the last request/response frame on the connection):

```json
{"id": "7", "ok": true, "result": {"revision": 42}}
```

A missing or unknown lease is `{"code": "connect-required"}`; the lease
of a client that was taken over is `{"code": "taken-over"}`. The two
instruct differently on purpose — go get a lease, versus stop, somebody
else drives this session now. Subscribing registers the connection
under the lease, which is what lets a later takeover close *this*
stream rather than leaving two clients both believing they drive the
session.

After the ack every frame is an `EventBatch` — one per workspace
commit, `{"revision": <u64>, "events": [<EventEnvelope>, ...]}`, one
per newline-delimited frame — **except** the single terminal control
envelope that ends the stream:

```json
{"event": "session.stopping", "data": {"reason": "stop"}}
```

`reason` is `"stop"` (the session is shutting down) or `"taken-over"`
(another client took the lease). It carries **no `revision`** and is
exempt from the gap check below: it is not a commit, it is the stream
saying why it is over, and it is always the last frame before the
close. The catalog of batch envelopes is [Events](#events) below.

The envelope is **best-effort**. A peer that stopped reading has
already made the write impossible once its socket buffer filled, so a
plain EOF remains the fallback signal and a client must treat an
unlabeled close exactly as it treated one before: reconnect and
resync.

Three properties make this lossless without a replay buffer:

* **The ack is a fence.** `revision` is the commit the subscription
  starts from, and the first batch is exactly `revision + 1`. Pair it
  with [`tab.list`](#tablist)'s own `revision`: snapshot, discard every
  batch `<=` it, apply the rest.
* **No gaps.** Every commit is a batch, including a commit that
  produced no events — that arrives as `{"revision": N, "events": []}`.
  A skipped number therefore always means loss, never a quiet commit.
* **The server closes rather than thins.** If a subscriber stops
  reading, falls behind the workspace broadcast, or the connection
  stalls, the server closes the connection instead of dropping events
  out of the stream. A close is the resync signal: reconnect,
  re-subscribe, re-pull `tab.list`, and fence again — the
  `session.stopping` envelope, where it arrives, only tells the client
  *why* it is resyncing.

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

**Breaking change, HS-1b (plan 036).** HS-1a served this op with no
lease at all. A client written against that form still *decodes* — the
`lease` field defaults rather than being required — and gets
`connect-required`, which names the step it skipped instead of an
envelope-shaped `invalid-param` that names nothing.
`SESSION_PROTOCOL_VERSION` bumped `1` → `2` for exactly this.

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

The order a client runs them in is `session.identify` →
`session.connect` → `session.set_theme` / `session.set_focus` →
`events.subscribe` / `tab.attach`. Only the lease part of that is
enforced: `identify` is a stateless read and nothing requires it first,
but the lease *is* required, and the ops that need it say so by name.
The two `set_*` ops are placed where they are because both state
something the session would otherwise guess wrong — its palette and
whose window is looking at it — and both are re-stated whenever the
client's own answer changes.

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
it belongs: `events.subscribe` is
[lease-gated](#eventssubscribe), `session.stop` and takeover
[label what they close](#sessionstop), and terminal-generated queries
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
  [`tab.dump_resolved`](#tabdump_resolved) are served from it.
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
  "session_protocol": 2,
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
the two version different things and move independently. It is **`2`**
as of HS-1b: the bump is breaking because
[`events.subscribe`](#eventssubscribe) and [`tab.attach`](#tabattach)
now require a lease, and a client written against `1` subscribed with
none. The [attach handshake](#data-plane) carries the same number and
refuses a mismatch before it even looks at the token.

`payload_kinds` names what this session can encode a tab's attach
payload as, in no particular order; it is an **open list of strings**,
not a closed enum — a client preserves values it does not recognize and
negotiates on the ones it does. The sample above shows a host offering
two kinds, which is what a client's decode has to tolerate; the shipped
`roost-session` advertises `["ghostty-snapshot"]` alone (`vt`, the
replay-a-byte-stream fallback, is a named kind with no implementation
behind it — architecture §4.4's escape hatch, not built).

`libghostty_build` is this session's pinned Ghostty build identity,
`ghostty-<first 16 hex of the pinned SHA>+snapshot.v<format version>`.
It is the negotiation for `ghostty-snapshot`: two libghostty builds
that disagree cannot exchange a snapshot, so `tab.attach` requires an
**exact** string match and refuses a mismatch by name
(`build-mismatch`) rather than letting it surface later as a corrupt
screen. Both fields were empty in HS-1a, when there was nothing to
attach to.

**Test seam:** with `ROOST_TEST_MODE=1` set, a session additionally
reads `ROOST_SESSION_FAKE_BUILD` and reports *that* string as
`libghostty_build` instead of its real one — and uses it for
`tab.attach`'s check #4 too, so the two stay consistent. Reproducing a
build/protocol mismatch otherwise needs a second binary built against a
second Ghostty pin, which no CI lane can produce; this makes it a
one-line fixture (plan 037 §3.7). Ignored entirely outside test mode,
so a production daemon can never be made to lie about its own build.

### `session.connect`

Claim the session's **interactive lease** — the authority to drive
tabs, not merely to read them.

Request:
```json
{"id": "3", "op": "session.connect", "params": {"takeover": true}}
```

Response:
```json
{"id": "3", "ok": true, "result": {
  "lease": "9f2c1d7a4b6e08315c0d9a72e4f16b83",
  "revision": 42
}}
```

`takeover` defaults to `false`. `lease` is 32 lowercase hex characters
of OS entropy — a **bearer credential**: never log it, never print it
in a failure dump, never echo it in an error. `revision` is the
workspace commit the lease was minted at, read under the same lock as
the snapshot, so a client can fence its first
[`tab.list`](#tablist) against the event stream without a second round
trip.

The lease is the interactive-authority boundary, and a self-declared
client id would not be one: possession of the token is what proves a
client is *the* driver. It gates [`events.subscribe`](#eventssubscribe)
and [`tab.attach`](#tabattach). Administrative ops — `tab.open`,
`tab.list`, `tab.write`, `project.*`, `tab.agent_report`, the dumps —
stay lease-free: they are same-UID control-plane use (`roostctl`, a
Claude hook), not interactive ownership.

**The lease outlives the connection it was minted on.** Dropping every
socket releases nothing; a client that reconnects is a *new* client as
far as the session is concerned. That is deliberate — a half-crashed
client still holding a data connection must not be able to keep typing
into tabs a replacement believes it owns, and the session cannot tell a
crash from a slow network.

So reconnecting is always a takeover:

| Situation | `takeover: false` | `takeover: true` |
|---|---|---|
| No lease held | mints a lease | mints a lease |
| Someone else holds it | `already-connected` | takes it |
| **You** hold it, on this very connection | `already-connected` | takes it (fresh token) |

The third row is not an oversight. A client that lost track of its own
lease is exactly the one that has to re-establish it deliberately.

A takeover, under one lock, atomically: invalidates the old lease,
closes **every connection registered under it** except the requesting
one, purges its outstanding attach tokens, and mints the new lease.
What "closes" means depends on what the connection was doing:

* a plain control connection just closes — there is no stream its peer
  is waiting on, only a reply it never asked for;
* an events connection gets the terminal
  `{"event": "session.stopping", "data": {"reason": "taken-over"}}`
  envelope, then closes;
* a data connection gets an `ERROR` frame with code `taken-over`, then
  closes.

Both labels are best-effort under a 2 s deadline: a peer that stopped
reading made the write impossible when its socket buffer filled, and
EOF is then the only signal it gets.

Purging the displaced lease's attach tokens matters for a reason that
is easy to miss: they would be refused at the handshake's lease
re-check anyway, but leaving them would let a dead client's 16
outstanding tokens hold the whole quota against the new holder for a
full TTL. A ticket purged this way answers `invalid-token` — the
session no longer recognizes it at all — while an op that presents the
*lease* answers `taken-over`.

**Exactly one tombstone.** The session remembers the most recently
displaced lease so its holder is told `taken-over` (someone else has
it; stop) rather than `connect-required` (you never connected;
reconnect). Only the most recent: after a second takeover the
first-displaced client's lease is forgotten and it falls back to
`connect-required`. It has already been told, and an unbounded graveyard
of dead leases is not a thing a process meant to run for weeks should
keep.

`session.connect` is a mutating op for stop-latch purposes — it hands
out authority — so it answers `shutting-down` once
[`session.stop`](#sessionstop) has latched.

### `session.set_theme`

Seed every tab's server Terminal with the connected client's palette — closes architecture §13's reseed gap (plan 037 §3.6). Lease-gated: only the interactive-lease holder may recolor a session.

Request:
```json
{"id": "8", "op": "session.set_theme", "params": {
  "lease": "9f2c1d7a4b6e08315c0d9a72e4f16b83",
  "osc_colors": {
    "foreground": "#ffffff", "background": "#1c1c1c", "cursor": "#98989d",
    "palette": ["#000000", "... 256 entries total ..."]
  }
}}
```

Response: `{"tabs": 3}` — the number of live tabs whose server Terminal was reseeded. `0` is a success: a session with no tabs yet still remembers the theme for the ones it opens next.

`palette` must carry exactly 256 `#rrggbb` entries (lowercase, the same spelling [`tab.dump_resolved`](#tabdump_resolved) uses); a short or long array is `invalid-param` rather than a partial application.

A client sends this **immediately after `session.connect` and before its first `tab.attach`** — attaching before the theme lands would paint the session's factory colors for one frame — and again whenever its own theme changes thereafter. Concurrent callers are last-writer-wins by design: the theme store mints a generation on every apply, so a `set_theme` racing a tab spawn is caught up at promotion rather than silently lost, and interleaved fan-outs converge on the newest theme instead of whichever send landed last.

Like `session.connect`, this answers `shutting-down` once `session.stop` has latched.

### `session.set_focus`

Tell the session which of its tabs the attached client is actually looking at. Lease-gated: focus is a property of the client driving the session, so a client that does not drive it does not get to state one.

Request:
```json
{"id": "9", "op": "session.set_focus", "params": {
  "lease": "9f2c1d7a4b6e08315c0d9a72e4f16b83",
  "focused_tab_id": "5"
}}
```

Response: `{}` — nothing to report beyond "applied".

**Why it exists.** A session is headless: it has no window, so its workspace defaults to *focused*, and its active tab is whatever its restored layout selected. The [focus-suppression rule](../guides/notifications.md#focus-policy) — `suppress := the window is focused AND this is the active tab` — is therefore permanently satisfied for one tab per session, and a **suppressed raise emits nothing at all** (no pending bit, no `notification.fired`, no badge). Without this op, that one tab's agent can never reach an attached client. With it, the client that *does* have a window states the truth and the session suppresses the tab the user is really looking at.

**`focused_tab_id` is required, and nullable.** `null` means "nothing on this session is being looked at" — the client's window lost focus, or its selection moved to another host or to a local tab. An **omitted** field is not the same statement and is refused with `missing-param`: a client that forgot to say is exactly the one that must not be guessed for, since guessing "focused" re-creates the mute this op exists to fix.

**Validation order**, for the same reason `tab.attach` pins one — each failure names a different thing to fix:

1. `missing-param` / `invalid-param` — the field is absent, or is neither a decimal-string tab id nor null. Decode comes first of necessity: the lease itself rides inside the params, so an envelope that does not decode cannot present one.
2. `connect-required` / `taken-over` — the lease gate.
3. `not-found` — a tab id this session does not have.

The apply is one workspace transaction and `not-found` leaves **nothing** applied: a client naming a tab that just closed must not flip the session to "focused" against whatever tab happened to be active. On success with an id, the session both marks itself focused and moves its own active selection (and its persisted selection) onto that tab, as [`tab.focus`](#tabfocus) would; it does **not** acknowledge the tab's notification — the client sends [`tab.clear_notification`](#tabclear_notification) for that. Re-stating a focus that is already current emits no `active.changed`, so a reconnecting client's re-assert costs other clients no re-render. `null` moves the flag alone and leaves the selection where it is, so a reconnect restores the same tab.

**A focus does not outlive the client that reported it.** The lease deliberately outlives its connections, but the focus does not: the session reverts to "nobody is looking" (the flag only — the selection stays) when

* a new lease is minted, `session.connect` takeover included;
* the connection that sent the `session.set_focus` closes; and
* the last connection registered under the live lease closes.

The middle one is what keeps the reset independent of ordering: a client re-dialing on the same lease can register before the departed client's close is noticed, and counting live connections alone would then leave a gone client's focus standing.

A client therefore re-states its focus right after `session.connect`, and again whenever its window focus or selection moves — including when the session's own `active.changed` reports a move away from the stated focus (a lease-free `tab.focus` from a script would otherwise park the suppressed slot on a tab nobody is watching until the client's next natural edge). A session one release older answers `unknown-op`, which is a refusal like any other: the connection is unaffected and the client keeps HS-2's behavior (the attached tab suppresses its own notifications). The reverse pairing — a session with this op driven by an older client that never sends it — errs the loud way: the connect-time reset leaves the session unfocused, so nothing is suppressed and the attached tab's notifications fire rather than vanish.

Like `session.connect`, this answers `shutting-down` once `session.stop` has latched — the latch is checked *before* the lease gate, so a stopping session says so rather than sending a client off to reconnect.

### `tab.attach`

Negotiate a payload kind for one tab and get a single-use ticket for
one [data connection](#data-plane). Session sockets only; a UI socket
answers `unknown-op`.

Request:
```json
{"id": "4", "op": "tab.attach", "params": {
  "lease": "9f2c1d7a4b6e08315c0d9a72e4f16b83",
  "tab_id": "5",
  "kinds": ["ghostty-snapshot"],
  "cols": 120,
  "rows": 40,
  "cell_w_px": 9,
  "cell_h_px": 18,
  "libghostty_build": "ghostty-3f6b1c9a4d2e5f80+snapshot.v1"
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
first entry it supports; a list mixing kinds this build has never heard
of with one it serves is fine.

**Validation order is part of the contract**, because each failure
tells the client to fix a different thing and an earlier one must not
be masked by a later one:

| # | Check | Error |
|---|---|---|
| 1 | lease is live | `connect-required` (unknown/absent) or `taken-over` (tombstoned) |
| 2 | tab exists with a live terminal | `not-found` |
| 3 | `kinds` contains something servable | `unsupported-kind` (message names both lists) |
| 4 | `libghostty_build` matches exactly | `build-mismatch` (message names both strings) |
| 5 | `cols` and `rows` both non-zero | `invalid-param` |
| 6 | the tab accepts the geometry | `invalid-param` |
| 7 | token quota not exhausted | `too-many-tokens` |

Zero `cell_w_px` / `cell_h_px` are legal — a headless client has no
cell metrics to report — but a zero-sized grid is not a grid.

**Attach is when the server resizes.** Between checks 5 and 7 the
session resizes the tab (server terminal *and* `TIOCSWINSZ`) to the
requested geometry and waits for that to land, so the snapshot the
data connection is about to encode is already at client size and needs
no post-READY resize. Detach never resizes back — the PTY keeps the
last attached size, so a TUI agent does not get a `SIGWINCH` because
somebody closed a laptop. This does not contradict that rule: attach is
exactly when an in-process Roost resizes too.

`attach_token` is 32 hex characters, the same bearer credential the
lease is and under the same no-logging rule. It is:

* **single-use** — consumed under the registry lock, so two connections
  presenting one token admit exactly one;
* **short-lived** — 60 s TTL (`ATTACH_TOKEN_TTL`), a protocol constant
  that is *not* scaled by `ROOST_TEST_TIMEOUT_SCALE`. A session started
  with `ROOST_TEST_MODE=1` honors `ROOST_SESSION_ATTACH_TTL_MS` to
  shorten it, which is how the expiry case is tested in seconds; a
  production daemon ignores that variable entirely;
* **quota-bounded** — at most 16 unconsumed tokens
  (`MAX_OUTSTANDING_TOKENS`) exist at once. Past that, minting is
  refused with `too-many-tokens` rather than evicting a token some
  other connection is about to present. Reaching it means a client
  minted 16 tickets inside one TTL and dialed none of them; a healthy
  attach consumes its ticket within a round trip;
* **lease-bound** — re-checked at the handshake, so a takeover between
  this reply and the dial refuses the ticket with `taken-over`;
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

Like `session.connect`, `tab.attach` answers `shutting-down` once
`session.stop` has latched.

### `session.stop`

Params: `{}`. Response: the reap report,

```json
{"reaped": ["3", "5"], "killed": ["8"], "abandoned": ["9"]}
```

Stops the session. In order: the session latches *stopping* (every
mutating op from that point answers `{"code": "shutting-down"}`, reads
keep answering, and a second `session.stop` gets `shutting-down` too);
it **labels and closes every connection the lease holder owns** — an
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
{"attach": "1a0be5c37d924f68b1c05e3a7f2d8496", "protocol_version": 2,
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
| `invalid-token` | unknown, expired, already-used, or purged by a takeover. |
| `taken-over` | the lease the token was minted under is no longer current. |
| `not-found` | the tab has no live terminal, or was respawned between `tab.attach` and this handshake. |
| `snapshot-failed` | the terminal could not be encoded right now. Re-attach is the recovery — it is about this instant, not about the client. |
| `shutting-down` | `session.stop` has latched. |
| `parse-error` | the handshake line did not decode. |
| `not-supported` | this socket serves no data connections. |

`mode` is `"snapshot"` or `"resume"`, and `seq` is the **fence**: the
client has everything up to and including it, and the first `PTY` frame
carries `seq + 1`. In snapshot mode the fence is the snapshot's own
encode point; in resume mode it is `resume_from_seq - 1`.

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
  `SNAP` frames for the same reason.
* Fixed-width payloads are exactly that width: `PTY` ≥ 9 bytes,
  `EXIT` == 12, `RESIZE` == 8.
* A zero-length payload is meaningful only for `SNAP`. An empty `INPUT`
  is a `protocol-error`.
* An unknown type byte is a `protocol-error` naming the byte. The
  framer hands it up rather than failing the decode, precisely so the
  endpoint can name it.

`EXIT` is **always the last frame** on a connection that sees one, and
`final_seq == last PTY seq + 1` — the exit consumes an ordinal of its
own, so a client that has applied `PTY` frame `final_seq - 1` knows it
missed nothing. Pixel dimensions on `RESIZE` are load-bearing, not
decoration: the server terminal's resize and mode-2048 size reports
both need them.

Ordering, once the stream is running:

1. The whole snapshot prefix **through Ghostty's READY record** goes
   out at full speed, and live `PTY` frames are absorbed but **held**
   until it has. A client has no terminal to apply a `PTY` frame to
   before READY, and making it buffer them would move this queue into
   every client. The window is tiny — that prefix is just the active
   screen.
2. After READY, `PTY` leads: a keystroke's echo must not wait behind a
   scrollback page.
3. But the snapshot cannot be starved. At least one `SNAP` frame goes
   out after 256 KiB of `PTY` payload **or** 50 ms since the last one,
   whichever comes first — a `yes`-style producer would otherwise hold
   FINISH off for as long as it kept running, and a slow-but-endless
   one would never trip a byte floor at all.
4. `EXIT` goes out only once everything before it has.

The snapshot's own record structure (GHOSTSNP: envelope, READY,
history pages, FINISH) rides *inside* `SNAP` frames as an opaque byte
stream — no record alignment, since Ghostty designed the format to be
embedded and the client's decoder buffers to record boundaries itself.
Ghostty's READY is the only READY in this design; the transport adds no
second marker.

`ERROR` frames carry a stable code:

| Code | Meaning |
|---|---|
| `desync` | the stream cannot be trusted: a gap or duplicate `seq`, a lagged tee, a snapshot that blew the attach budgets. Re-attach. |
| `overflow` | the peer is not reading — 8 MiB queued for it, or a single write past its deadline. |
| `superseded` | a newer data connection took this tab. |
| `taken-over` | another client took the session lease. |
| `shutting-down` | `session.stop` latched. |
| `protocol-error` | the client sent something the framing forbids. |

A gap is fatal rather than papered over because the client's terminal
would silently diverge and could never tell — and re-attach already
rebuilds from a fresh snapshot, so there is nothing to gain by
continuing. Every `ERROR` is best-effort: a peer that stopped reading
made the write impossible, and EOF is the accepted fallback everywhere
a label is promised.

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

#### Supersede, and one connection per tab

A tab has at most one live data connection. A second *admitted*
handshake for the same tab supersedes the first, which closes with
`ERROR superseded` — two forwarders racing one tee is not a state worth
supporting, and the client that just attached is by definition the
current one. Admission (token consume, lease re-check, registration,
supersede) happens as a single step under one lock, so a takeover
either wholly precedes an attach or wholly follows it.

Client disconnect at any point simply aborts the forwarder; the tab
keeps running and no partial state survives. That is the difference
between detaching and stopping: dropping the socket leaves the session
exactly as it was.

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
batch. It is the connection's own terminal control envelope — see
[`events.subscribe`](#eventssubscribe).

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
* `tab.effect` — `{"tab_id": "<id>", "effect": "bell" | "clipboard-write", "data"?: "<base64>", "target"?: "system" | "selection"}`
  (plan 037 §3.6). One client-directed side effect the tab's OSC scan
  produced, for whichever client is attached (§3.4's lease-holder-only
  contract) to apply — a bell BEL byte outside any escape sequence, or
  an OSC 52 clipboard write. `data` and `target` are present only for
  `clipboard-write`: `data` is the decoded payload, base64-encoded like
  every other bytes field on this wire and capped at 256 KiB decoded
  (`CLIPBOARD_EFFECT_MAX_BYTES` — an oversized write is dropped and
  debug-logged by size, **never by content**); `target` distinguishes
  OSC 52's primary-selection form (`p`/`s`) from the system clipboard
  (`c` or no selector), defaulting to `system` when absent. HS-2 ships
  exactly these two effects — every other client-local OSC effect
  (pointer shape, today) stays dropped + debug-logged in the tab task
  rather than added to this envelope.

## Dropped vs. the legacy proto

These RPCs/messages were intentionally dropped — the new architecture
makes them unnecessary:

* `StreamPty` (`PtyClientMessage`, `PtyServerMessage`, all variants).
  The UI owns the PTY; nothing crosses the wire.
* `ReportOsc`. OSC sequences are parsed in the UI; the UI updates
  its own state directly. There is nobody to round-trip to.
* `WatchEvents` (legacy event stream RPC) is replaced by the
  `events.subscribe` op + push envelopes on the same connection; see
  [`events.subscribe`](#eventssubscribe). Served by a host session
  today, still `not-implemented` on a UI socket.

Schema-only fields that survive but rename:

* Proto `TabState` enum → JSON string. Mapping:
  `TAB_STATE_NONE → "none"`, `TAB_STATE_RUNNING → "running"`,
  `TAB_STATE_NEEDS_INPUT → "needs_input"`, `TAB_STATE_IDLE → "idle"`.
  `TAB_STATE_UNSPECIFIED` is omitted; the server never returns it.

## Versioning

`identify.protocol_version` is the integer schema version. M0 ships
version `1`. Additive changes (new optional fields, new ops, new
events) do not bump the version. Breaking changes coordinate a major
version bump and updated clients.
