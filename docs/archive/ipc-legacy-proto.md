# Dropped vs. the legacy proto

Archived reference. Roost's IPC was originally a protobuf schema; this
page is what changed when it moved to the [JSON wire](../reference/ipc.md)
that shipped instead. Nothing here describes current behavior — it is
kept only for anyone still holding a client written against the
protobuf era.

These RPCs/messages were intentionally dropped — the new architecture
makes them unnecessary:

* `StreamPty` (`PtyClientMessage`, `PtyServerMessage`, all variants).
  The UI owns the PTY; nothing crosses the wire.
* `ReportOsc`. OSC sequences are parsed in the UI; the UI updates
  its own state directly. There is nobody to round-trip to.
* `WatchEvents` (legacy event stream RPC) is replaced by the
  `events.subscribe` op + push envelopes on the same connection; see
  [`events.subscribe`](../reference/ipc.md#eventssubscribe). Served by a host
  session, still `not-implemented` on a UI socket.

Schema-only fields that survive but rename:

* Proto `TabState` enum → JSON string. Mapping:
  `TAB_STATE_NONE → "none"`, `TAB_STATE_RUNNING → "running"`,
  `TAB_STATE_NEEDS_INPUT → "needs_input"`, `TAB_STATE_IDLE → "idle"`.
  `TAB_STATE_UNSPECIFIED` is omitted; the server never returns it.
