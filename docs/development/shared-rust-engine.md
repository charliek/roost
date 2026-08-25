# Shared Rust engine

`roost-engine` is Roost's toolkit-neutral authoritative Rust application
engine. It is the core of the Linux UI — `roost-iced` is an adapter over it,
not a second implementation of it — and it is the seam a future convergence
would build on, whichever way
[that question resolves](vision.md#direction-under-evaluation): either an iced
shell on both platforms sits on this engine, or a Swift shell adopts it
incrementally without exposing Rust layouts across an ABI.

That the engine has exactly one consumer today is a fact about the present,
not a licence to fold it back into the UI. Its whole value is that the
authoritative state machine is separable from the toolkit.

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

`roost-vt::TerminalScroll` owns the synchronous terminal-wheel policy. Every
live terminal has an independent accumulator and snap-to-bottom state. The
adapter normalizes native units into signed rows and retains pointer geometry,
modifiers, encoders, and PTY writes; the shared model selects mouse-report,
alternate-screen key, or local viewport behavior with mouse tracking taking
precedence. Local movement reconciles against libghostty's authoritative
scrollbar rather than guessing whether a downward request reached the live
bottom. The outcome enum is deliberately a plain value type — a realistic
adoption seam for a second adapter without exposing Rust layouts through an
ABI.

The UI adapter retains native widgets and layout, terminal drawing and
libghostty-vt event-loop access, platform input translation, clipboard and
notifications, URL launching, screenshot capture, and event-loop marshalling.

## Boundary

The crate boundary in production use today is the concrete API: `Workspace`,
`LocalClient`, `PtySupervisor`, and `roost_engine::ipc`'s exhaustively-matched
`UiRequest` port. The exhaustive match is the mechanism, not a style
preference: a new capability cannot reach the UI without the compiler naming
every adapter that has to answer it. Never add a wildcard arm.

`events::subscribe` bridges `Workspace`'s broadcast channel into an unbounded
mpsc the adapter drains on its own main-loop task — for iced, the `EngineFeed`
wake subscription — folding lag or the startup gap into a full-state `Resync`
the adapter reconciles against.

The `facade` module (`Engine`/`EngineCommand`/`EngineSnapshot`/
`EngineEventStream`) is the **experimental** Swift-facing boundary. It has no
production consumer, is feature-gated (`roost-engine/facade`, compiled and
tested in CI but off by default), and stays unproven until a real adapter
adopts it — tracked as
[#286](https://github.com/charliek/roost/issues/286), held at "don't invest,
don't delete" while the Mac-shell question is open (see
[Direction](vision.md#direction-under-evaluation) and the
[iced migration's M5](iced-migration.md#m5-frozen)). Its design:
`EngineCommand` reuses serializable `roost-ipc` request DTOs.
`Engine::execute` returns an owned `CommandResult` or a typed `EngineError`
with a stable status key. `EngineSnapshot` is a versioned, owned, ordered
replacement projection. Every committed workspace transition has a monotonic
in-process revision; compound events share that revision and retain their
established order.

`EngineEventStream` subscribes before its initial snapshot, discards any stale
buffered deltas already represented by that snapshot, and turns broadcast lag
or a revision gap into `EngineEvent::Resync`. Event publication is synchronous
and non-blocking while the state lock establishes order. No UI callback runs
under that lock, and persistence I/O happens after it is released.

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
keeps split-sequence scan state and reply ordering inside the engine while the
adapter retains libghostty-vt access and executes native clipboard/pointer
ports. No UI callback or renderer type crosses the router boundary.

## Dependency checks

`make check-iced` enforces these with `cargo tree` greps; CI runs its
own equivalent in `iced-build-e2e`'s toolkit-boundary step:

```sh
# make check-iced (excerpt)
cargo tree -p roost-iced   | grep -E 'gtk4|libadwaita|pango|cairo-rs'         # must not match
cargo tree -p roost-engine | grep -E 'gtk4|libadwaita|iced|notify-rust|zbus|arboard'  # must not match
```

Neither shared crate may gain Iced, AppKit, or any other renderer, windowing,
or desktop-integration dependency — the Make target names `iced`,
`notify-rust`, `zbus`, and `arboard` explicitly for `roost-engine`; the CI
step covers `roost-engine` (gtk4/libadwaita/iced) and additionally
`roost-ui-model` (those plus pango/cairo-rs/wgpu). `roost-ui-model` uses the
renderer-independent `roost_vt::ColorRgb`; enabling libghostty-vt FFI remains a
UI-adapter choice.

`roost-iced` depends on `roost-engine`; neither shared crate depends back on
the adapter.
