# Shared Rust engine

`roost-engine` is Roost's toolkit-neutral authoritative Rust application
engine. GTK consumes it today; the Iced POC consumes it directly. A future
Swift adapter can adopt the same boundary incrementally without exposing Rust
layouts across an ABI.

## Ownership

The engine owns workspace transitions and ordered events, agent-state
derivation, persistence/restoration, PTY supervision, terminal session
lifecycle, streaming OSC routing/application, profile-scoped instance locking,
full-state reconciliation, and target-neutral IPC dispatch. It reuses the
focused `roost-ipc` and `roost-osc` contracts.

`roost-ui-model` owns toolkit-neutral configuration, terminal theme/color
resolution, keybinds, command palettes, providers, custom commands, agent and
notification projections, project rollups, shell escaping, and word
selection.

`roost-vt::TerminalScroll` owns the synchronous terminal-wheel policy shared
by GTK and Iced. Every live terminal has an independent accumulator and
snap-to-bottom state. Adapters normalize native units into signed rows and
retain pointer geometry, modifiers, encoders, and PTY writes; the shared model
selects mouse-report, alternate-screen key, or local viewport behavior with
mouse tracking taking precedence. Local movement reconciles against
libghostty's authoritative scrollbar rather than guessing whether a downward
request reached the live bottom. The outcome enum is also a realistic future
Swift adoption seam without exposing Rust layouts through an ABI.

UI adapters retain native widgets and layout, terminal drawing and
libghostty-vt event-loop access, platform input translation, clipboard and
notifications, URL launching, screenshot capture, and event-loop marshalling.

## Boundary

The crate boundary in production use today is the concrete API: `Workspace`,
`LocalClient`, `PtySupervisor`, and `roost_engine::ipc`'s exhaustively-matched
`UiRequest` port, consumed by both the GTK and Iced adapters.

The `facade` module (`Engine`/`EngineCommand`/`EngineSnapshot`/
`EngineEventStream`) is the **experimental** Swift-facing boundary. It has no
production consumer yet and is feature-gated (`roost-engine/facade`, tested in
CI but off by default) until a real adapter adopts it and proves the seam —
see the migration roadmap's M5. Its design: `EngineCommand` reuses
serializable `roost-ipc` request DTOs. `Engine::execute` returns an owned
`CommandResult` or a typed `EngineError` with a stable status key.
`EngineSnapshot` is a versioned, owned, ordered replacement projection.
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

`Workspace` also owns the last selected live tab for each project. Its
`preferred_tab` query falls back to display order and repairs preferences when
tabs or projects close. Adapters therefore do not need a competing
per-project selection map, and project shortcuts can resolve against a fresh
authoritative snapshot. Only the globally active project/tab position is
persisted today; inactive-project preferences are intentionally runtime-only
and rebuild during restoration, preserving the established `state.json`
schema and semantics.

Each live Rust terminal owns a `roost_engine::osc::OscRouter`. The caller feeds
PTY bytes plus an owned renderer-derived RGB/palette snapshot and receives an
ordered list of workspace, PTY-input, clipboard, and pointer actions. This
keeps split-sequence scan state and reply ordering shared while the adapters
retain libghostty-vt access and execute native clipboard/pointer ports. No UI
callback or renderer type crosses the router boundary.

## Dependency checks

```sh
cargo tree -p roost-engine -e normal
cargo tree -p roost-ui-model -e normal
cargo tree -p roost-linux -i roost-engine
cargo tree -p roost-iced -i roost-engine
```

Neither shared crate may gain GTK, libadwaita, Iced, AppKit, Cairo, Pango, or a
renderer dependency. `roost-ui-model` uses the renderer-independent
`roost_vt::ColorRgb`; enabling libghostty-vt FFI remains a UI-adapter choice.

GTK and Iced both depend on `roost-engine`; neither shared crate depends back
on either adapter, and `roost-iced` has no dependency on `roost-linux` or GTK's
native stack.

The proposed Swift ABI, ownership and threading rules, migration slices, and
the reviewed decision to defer unsafe FFI during the walking skeleton are in
the [Iced POC plan](iced-poc-plan.md#swift-interoperability-decision).
