# JSON IPC

`roostctl` and Claude hooks drive the running Roost UI through a small
newline-delimited JSON protocol over a Unix-domain stream socket. The protocol
is local-only — there is no network deployment.

The UI binary (Swift `Roost.app` on Mac, `roost-linux` gtk4-rs binary on
Linux) is the IPC server. `roostctl` is the only first-party client; the
contract here is what any other automation should implement.

The socket path is the bundle profile's `socket_path` (see
[`paths.md`](paths.md)):

* Mac (Swift `Roost.app`): `~/Library/Caches/Roost/roost.sock`
* GTK dev mode on Mac:    `~/Library/Caches/Roost-gtk/roost.sock`
* Linux (XDG):            `$XDG_RUNTIME_DIR/roost/roost.sock`
* Linux (else):           `/tmp/roost-<uid>/roost.sock`

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
* **Event envelope** (server-push, unsolicited, only sent after
  `events.subscribe`):
  `{"event": "<dotted-name>", "data": {...}}` — no `id`, no response
  expected.
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
  dispatched onto the UI's main thread (Swift `@MainActor`; gtk4 glib
  main loop). Responses are delivered in completion order, which is
  not guaranteed to match request order. Clients correlate by `id`.
* **Schema drift mitigation:** `tests/ipc-vectors/*.json` is a directory
  of canonical message exemplars (one file per op/event). Both
  `cargo test -p roost-ipc` (Rust) and Swift's `XCTest` target load
  these vectors and assert decode → re-encode → byte-equal.
* **Errors:** stable kebab-case codes. Current set:
  `unknown-op`, `unknown-field`, `missing-param`, `invalid-param`,
  `parse-error`, `frame-too-large`, `duplicate-id`, `not-found`,
  `internal`. Clients should treat unknown codes as fatal for the
  request and surface `message` to the user.

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
    "hook_active": "<bool>"
  },
  "Project": {
    "id": "<string-int64>",
    "name": "<string>",
    "cwd": "<string>",
    "position": "<int32>",
    "created_at": "<int64-unix-seconds>",
    "tabs": ["<Tab>"]
  }
}
```

`TabState` is a JSON string with values: `"none"`, `"running"`,
`"needs_input"`, `"idle"`. The legacy `TAB_STATE_UNSPECIFIED` is not
exposed — the server always picks a concrete state.

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
word-splitting must pass `["sh", "-c", "..."]` explicitly.

Response: `{"tab": <Tab>}`.

### `tab.close`

Close a tab; the PTY child is `SIGHUP`'d and reaped.

Request: `{"params": {"tab_id": "3"}}`. Response: `{}`.

### `tab.list`

Snapshot of the workspace. Same shape as the legacy
`ListTabsResponse`.

Response: `{"projects": [<Project>, ...]}`.

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

### `tab.clear_notification`

Clears `Tab.has_notification` and emits the corresponding
`tab.notification` event with `has_pending = false`.

Request: `{"params": {"tab_id": "3"}}`. Response: `{}`.

### `tab.set_hook_active`

Marks the tab as owned by a hook session (e.g. Claude Code). While
hook-active, raw OSC 9/777 from the shell is suppressed — only the
hook drives notifications.

Request: `{"params": {"tab_id": "3", "active": true}}`. Response: `{}`.

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
empty/default). When open, `frame` is the current frame id (`commands` |
`launcher` | `themes` | `notifications`), and `items` are the filtered
rows in display order (`subtitle` present on rows that have one).

| Op | Request params | Notes |
|---|---|---|
| `palette.open` | `{"kind": "commands"}` | `kind`: `""`/`commands` → command palette; `launcher` → custom-command launcher. Other values → `invalid-param`. |
| `palette.state` | `{}` | Read the current state. |
| `palette.query` | `{"query": "theme"}` | Set the current frame's filter (resets selection to the top match). |
| `palette.activate` | `{"id": "new_tab"}` | Confirm the visible row with this id — runs its command or drills into its sub-frame. `not-found` if no palette is open or no row matches. |
| `palette.dismiss` | `{}` | Close any open palette. |

### Selection + clipboard test ops (`selection.*` / `clipboard.*`)

| Op | Params | Effect |
|---|---|---|
| `selection.set` | `{"tab_id": "1", "anchor": {"col": 3, "row": 0}, "cursor": {"col": 17, "row": 0}}` | Anchor a selection on the tab's terminal at viewport `(col, row)`. The UI converts to libghostty's `PointTag::Screen` internally so the selection survives scrolling — same flow as `mouseDown` + `mouseDragged`. `not-found` if the tab has no live terminal. |
| `selection.clear` | `{"tab_id": "1"}` | Drop the active selection (no-op if none). |
| `selection.dump` | `{"tab_id": "1"}` | Read back the selection. Response: `{"text"?: "...", "anchor_visible": bool, "cursor_visible": bool}`. `text` is omitted when no selection is active or when all selection rows have scrolled out of the viewport (the v1 partial-copy limitation). |
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

Opt-in to the event stream. After the response, the server pushes
`{"event": ..., "data": ...}` envelopes on the same connection until
the connection closes.

Request: `{"params": {"tab_id_filter": "0"}}`. A non-zero
`tab_id_filter` restricts the stream to events for that tab.

**M0 status:** stubbed. The server replies `{"ok": true, "result":
{}}` and never sends event envelopes on the connection. This is
intentional — `roostctl` does not need events for any current
subcommand, and clients that *do* want events will surface as
follow-ups against a working stub.

Response: `{}`.

## Events

Server-push only. Each is a line of the form `{"event": "<name>", "data":
{...}}`. The set below is the exhaustive list; no other event names are
emitted.

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
* `hook_active.changed` — `{"tab_id": "<id>", "active": <bool>}`.
* `notification.fired` — `{"tab_id": "<id>", "title": "<string>", "body": "<string>"}`. Mirrors the legacy proto's `NotificationEvent`; useful for tools that mirror notifications elsewhere.

## Dropped vs. the legacy proto

These RPCs/messages were intentionally dropped — the new architecture
makes them unnecessary:

* `StreamPty` (`PtyClientMessage`, `PtyServerMessage`, all variants).
  The UI owns the PTY; nothing crosses the wire.
* `ReportOsc`. OSC sequences are parsed in the UI; the UI updates
  its own state directly. There is nobody to round-trip to.
* `WatchEvents` (legacy event stream RPC) is replaced by the
  `events.subscribe` op + push envelopes on the same connection.

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
