# Proto schema changelog

Every change to `roost.proto` lands here. Schema is additive: new fields or
RPCs may be added in any minor entry; breaking changes (field renumbering,
type changes, removed RPCs) require a major bump and coordinated client +
daemon releases.

## Unreleased

### Phase 6a step 2 — explicit project lifecycle

The UI sidebar needs to add / rename / remove projects without going
through a tab. `OpenTab` with `project_id=0` continues to auto-create
a default project on first use; these RPCs are additive.

**New RPCs**

- `CreateProject(name, cwd) -> Project` — create a project with an
  explicit name. Empty `name` yields a daemon-picked `"Untitled <n>"`.
- `RenameProject(project_id, name) -> {}`.
- `DeleteProject(project_id) -> {}` — cascade-deletes the project's
  tabs; clients observe one `TabDeletedEvent` per tab followed by the
  terminal `ProjectDeletedEvent`.

**New events** (added to `Event.kind`):

- `ProjectCreatedEvent` (field 12) — fired when a project is created
  by any client. Carries the full `Project` snapshot.
- `ProjectRenamedEvent` (field 13) — fired on rename. `(project_id, name)`.
- `ProjectDeletedEvent` (field 14) — fired after the project's tabs
  have been individually deleted.

### Pre-1.0 schema tightening (Phase 3 follow-up)

Schema is still pre-1.0 with no released clients, so these are fixes rather
than evolutions — the bumps here will collapse into v1.0.

- **`OpenTabRequest.command` (string) → `OpenTabRequest.argv` (repeated string).**
  A single opaque string forced the daemon into shell parsing, which
  pushes shell-injection risk onto every client. Argv is the safe shape;
  clients that genuinely want shell-style word splitting send
  `["sh", "-c", "..."]` explicitly. Field number unchanged (3); type
  changed from `string` to `repeated string`. (CodeRabbit review.)
- **`Tab.hook_active` (bool, field 12) added.** The hook-suppression
  state is exposed in snapshots so a UI joining mid-session can render
  it without polling. Mirrors the runtime-only flag the daemon has been
  tracking since Phase 3.
- **`Event.kind` gains three variants:**
  - `TabOpenedEvent` (field 9) — fired when a tab is opened by any
    client. Carries the full `Tab` snapshot so peers can splice it
    directly into their model.
  - `ActiveChangedEvent` (field 10) — fired on focus changes. Carries
    `(project_id, tab_id)` together so clients never observe a stale
    project pairing.
  - `HookActiveChangedEvent` (field 11) — mirrors `Tab.hook_active` for
    clients that prefer reacting to flips without re-snapshotting.

### v0.1.0 — Initial schema (Phase 2)

Defines the wire contract from scratch, mirroring the JSON-RPC methods in
`internal/ipc/protocol.go` and adding streaming RPCs that the in-process Go
code did not need.

**RPCs**

- `Identify` — handshake; returns daemon pid, socket path, active tab,
  protocol version.
- `OpenTab`, `CloseTab`, `ListTabs` — tab lifecycle (new; the Go binary
  manages tabs in-process).
- `StreamPty` — bidirectional stream for PTY I/O. Client opens with
  `PtyAttach{tab_id}`, then sends input + resize; server streams output
  until exit.
- `WatchEvents` — server-stream of workspace mutations (notifications,
  state changes, structural reorders, title/cwd changes).
- `CreateNotification`, `SetTabTitle`, `FocusTab`, `SetTabState`,
  `ClearTabNotification`, `SetHookActive` — control RPCs mirroring the
  current JSON-RPC methods.
- `ReportOsc` — UI-to-core upcall when the UI's in-process VT parse
  detects an OSC sequence relevant to routing (9, 777, 7, etc.).

**Events**

- `NotificationEvent`, `TabStateChangedEvent`, `TabNotificationEvent`,
  `TabDeletedEvent`, `ProjectsReorderedEvent`, `TabsReorderedEvent` —
  same set as the in-process Go event channel.
- `TabTitleChangedEvent`, `TabCwdChangedEvent` — added so OSC-driven
  changes propagate to all UI clients consistently.

**Tab state enum**

- `TAB_STATE_NONE`, `TAB_STATE_RUNNING`, `TAB_STATE_NEEDS_INPUT`,
  `TAB_STATE_IDLE` — matches the four agent states currently encoded as
  Go strings.

**Open questions**

- Whether `Identify.protocol_version` should be a semver string vs. a
  monotonic uint32. Going with uint32 for handshake simplicity; revisit
  if we need to express skew tolerance.
- Whether `ReportOsc` should batch sequences (UI sees many OSCs per
  redraw). Starting with one-call-per-OSC; benchmark in Phase 5 and
  batch if it becomes hot.
