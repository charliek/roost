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
on macOS, `$XDG_RUNTIME_DIR/roost-session/roost.sock` on Linux), is
**reserved** for the future `roost-session` daemon (HS-1). Nothing binds
it yet, it is not a `roostctl --target` value, and `roostctl` never probes
it — see [`paths.md`](paths.md#session-profile-reserved).

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
  serve and UI sockets do not. Catalog: [Events](#events) below.
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
  `not-implemented`, `internal`. Clients should treat unknown codes as
  fatal for the request and surface `message` to the user.

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

`argv` empty means `[$SHELL]`. `cwd` empty means use the project's
default cwd. `title` empty means derive from `cwd`. There is
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

### `tab.dump_resolved`

Companion to `tab.dump` — a richer read of the same viewport, but each
cell carries the post-resolver fg/bg the production paint path computes
(including the theme's `bold-color` accent). Ungated; useful both for
debugging "why is this row gray" and as the resolver-walk regression
op for #142.

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

### `app.notification_status` *(test-only — gated, macOS iced only)*

**Requires `ROOST_TEST_MODE=1` set in the UI's launch environment.**
Without it the server returns `not-enabled`. Reads back the macOS iced
UI's `UNUserNotificationCenter` backend state
(`crates/roost-iced/src/macos/notifications.rs`).

Request: `{"params": {}}`. Response:

```json
{"backend": "available", "reason": null, "authorized": false}
```

`backend` is `"available"` once the UN delegate has installed — a
bundled launch that has reached `window_opened` — and `"unavailable"`
otherwise: every bare-binary build (no app bundle, so UN is never
touched), and a bundled app before its first window opens. `reason`
names why it is unavailable (`"not running from an app bundle"`,
`"window not opened yet"`), or `null` once available. `authorized` is
the user's answer to the authorization prompt, always `false` while
unavailable — CI's TCC authorization state is unknowable, so the
automated suite never asserts this `true`; the real prompt/click is the
morning checklist (#285).

Implemented only by the iced UI on macOS, same as `app.menu_dump`.

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

### `events.subscribe`

Turn this connection into a one-way event stream. **Served by a
host-session socket only.**

Request: `{"params": {"tab_id_filter": "0"}}`. Response (the last
request/response frame on the connection):

```json
{"revision": 42}
```

Everything after that ack is an `EventBatch` — one per workspace
commit, `{"revision": <u64>, "events": [<EventEnvelope>, ...]}`, one
per newline-delimited frame. The catalog of envelopes is
[Events](#events) below.

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
  re-subscribe, re-pull `tab.list`, and fence again.

After the flip the connection answers nothing. Frames a client writes
on it are read and discarded (so the server still notices a peer that
goes away), never dispatched and never replied to. That read is also
why a client must keep its write half **open**: half-closing it is how
a peer says it is gone, and the server ends the stream. `session.stop`
closes every subscriber before it drains its in-flight work, so a
client watching a session sees the stream end as the session goes down.

A non-zero `tab_id_filter` is rejected with `invalid-param` rather than
ignored — HS-2 scope. Silently serving an unfiltered stream to a client
that asked for one tab would make it mis-attribute every other tab's
events.

**Provisional.** HS-1b puts the stream behind a lease, which is a
breaking change to this op. Today Roost's own tests are its only
consumer.

On a **UI socket** the op is still unimplemented: it answers
`{"ok": false, "error": {"code": "not-implemented", "message":
"events.subscribe is not yet implemented"}}` rather than a false ACK,
because a UI process pushes nothing. Callers there poll `tab.list` /
`tab.dump` instead. A UI-side stream lands with its first consumer.

## Session ops

Served **only by a host session** (`roost-session`), never by a UI
socket. A UI socket answers `unknown-op` for both, which is how a client
tells the two kinds of socket apart.

### `session.identify`

Params: `{}`. Response:

```json
{
  "app_version": "0.0.18",
  "session_protocol": 1,
  "payload_kinds": [],
  "libghostty_build": "",
  "session_id": "01K3S8TQ4F0Q9YB2K6WZ5D7XN",
  "started_at": "2026-08-27T14:03:11Z"
}
```

The handshake a client runs before anything binary exists, so every
incompatibility is caught on stable JSON. `session_protocol` is
`SESSION_PROTOCOL_VERSION` — deliberately separate from the
request/response `protocol_version` in [`identify`](#identify), because
the two version different things and move independently.

`payload_kinds` is empty and `libghostty_build` is `""` in the current
implementation: attach is not available yet, and an honest empty list is
what lets a client fall back rather than negotiate a payload the session
cannot produce. HS-1b populates both. `payload_kinds` is an open list of
strings, not a closed enum — a client must preserve values it does not
recognize.

### `session.stop`

Params: `{}`. Response: the reap report,

```json
{"reaped": ["3", "5"], "killed": ["8"], "abandoned": ["9"]}
```

Stops the session. In order: the session latches *stopping* (every
mutating op from that point answers `{"code": "shutting-down"}`, reads
keep answering, and a second `session.stop` gets `shutting-down` too);
it waits out the mutating requests already in flight, so a `tab.open`
that got past the latch completes and its tab is included below; it
flushes the workspace layout; then it hangs every PTY up, escalating to
`SIGKILL` after a soft deadline.

The three id lists partition the tabs that were live when the stop
began — each id appears in exactly one, and each is sorted. `reaped`
died on the hangup; `killed` was still live at the deadline and was
SIGKILLed; `abandoned` was still unreaped after the post-kill tail and
the session stopped waiting for it. Ids are string-encoded like every
other id on this wire.

The reply is written **before** the process-level shutdown tail runs, so
a client always gets its report even though the session is on its way
out.

## Events

Server-push only, delivered on a host-session socket after
[`events.subscribe`](#eventssubscribe). Each envelope is a
`{"event": "<name>", "data": {...}}` object inside an `EventBatch`;
several envelopes can share one batch, which is what makes a commit
atomic on the wire. The set below is exhaustive — the serializer
(`crates/roost-engine/src/event_push.rs`) is a total match over the
workspace's event enum, so a new event cannot ship without a name
here.

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
