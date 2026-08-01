# Shared Rust engine

`roost-engine` is Roost's toolkit-neutral authoritative Rust application
engine. GTK consumes it today; the Iced POC consumes it directly. A future
Swift adapter can adopt the same boundary incrementally without exposing Rust
layouts across an ABI.

## Ownership

The engine owns workspace transitions and ordered events, agent-state
derivation, persistence/restoration, PTY supervision, terminal session
lifecycle, OSC application, profile-scoped instance locking, full-state
reconciliation, and target-neutral IPC dispatch. It reuses the focused
`roost-ipc` and `roost-osc` contracts.

`roost-ui-model` owns toolkit-neutral configuration, terminal theme/color
resolution, keybinds, command palettes, providers, custom commands, agent and
notification projections, project rollups, shell escaping, and word
selection.

UI adapters retain native widgets and layout, terminal drawing and
libghostty-vt event-loop access, platform input translation, clipboard and
notifications, URL launching, screenshot capture, and event-loop marshalling.

## Boundary

`EngineCommand` reuses serializable `roost-ipc` request DTOs. `Engine::execute`
returns an owned `CommandResult` or a typed `EngineError` with a stable status
key. `EngineSnapshot` is a versioned, owned, ordered replacement projection.
Every committed workspace transition has a monotonic in-process revision;
compound events share that revision and retain their established order.

`EngineEventStream` subscribes before its initial snapshot, discards any stale
buffered deltas already represented by that snapshot, and turns broadcast lag
or a revision gap into `EngineEvent::Resync`. Event publication is synchronous
and non-blocking while the state lock establishes order. No UI callback runs
under that lock, and persistence I/O happens after it is released.

The concrete `Workspace`, `LocalClient`, and runtime APIs remain public during
the GTK migration. `roost-linux` compatibility modules only re-export shared
types; they no longer own those state machines.

## Dependency checks

```sh
cargo tree -p roost-engine -e normal
cargo tree -p roost-ui-model -e normal
cargo tree -p roost-linux -i roost-engine
```

Neither shared crate may gain GTK, libadwaita, Iced, AppKit, Cairo, Pango, or a
renderer dependency. `roost-ui-model` uses the renderer-independent
`roost_vt::ColorRgb`; enabling libghostty-vt FFI remains a UI-adapter choice.

The proposed Swift ABI, ownership and threading rules, migration slices, and
the reviewed decision to defer unsafe FFI during the walking skeleton are in
the [Iced POC plan](iced-poc-plan.md#swift-interoperability-decision).
