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
