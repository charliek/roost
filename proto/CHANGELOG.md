# Proto schema changelog

Every change to `roost.proto` lands here. Schema is additive: new fields or
RPCs may be added in any minor entry; breaking changes (field renumbering,
type changes, removed RPCs) require a major bump and coordinated client +
daemon releases.

## Unreleased

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
