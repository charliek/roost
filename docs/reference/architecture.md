# Architecture

Roost ships two platform products — Swift + AppKit on macOS (`Roost.app`) and Rust + iced on Linux (`roost`, packaged as the `.deb`) — that each embed their runtime in-process. A third UI implementation, Rust + gtk4-rs (`crates/roost-linux`), lives in the repo as the Linux development/parity implementation; both Linux UIs share their authoritative state via the toolkit-neutral `roost-engine` crate. External tooling (the `roostctl` CLI, Claude Code hooks) talks to a running UI via newline-delimited JSON over a Unix-domain socket; the wire format is documented in [`docs/reference/ipc.md`](ipc.md). `libghostty-vt` is vendored once and linked directly into all three UIs for in-process VT parsing and rendering.

iced (`crates/roost-iced`) has shipped as the Linux package's production UI since the `poc/iced` branch merged to `main`; the production Swift + AppKit implementation on macOS is unaffected by that work — see [the Iced POC plan](../development/iced-poc-plan.md) for the design record that led here.

For the durable design rationale (why two languages, why in-process, why local UDS) see [Vision](../development/vision.md).

## Stack

Linux splits into two columns because the two Linux UIs share almost
everything below the window/renderer layer — both adapt the same
toolkit-neutral `roost-engine` — but differ in windowing toolkit and
renderer, and only one of them ships.

| Layer | macOS | Linux — GTK (`crates/roost-linux`, dev/parity) | Linux — iced (`crates/roost-iced`, shipped) |
|---|---|---|---|
| Window + chrome | Swift + AppKit | Rust + gtk4-rs + libadwaita (`crates/roost-linux/src/app.rs`) | Rust + iced (`crates/roost-iced/src/app.rs`) |
| Renderer | Core Graphics over libghostty-vt cell grid | Cairo + Pango over libghostty-vt cell grid (`crates/roost-linux/src/terminal_view.rs`) | iced + wgpu over libghostty-vt cell grid (`crates/roost-iced/src/terminal_widget.rs`) |
| Terminal engine | `libghostty-vt` (vendored, shared archive) | `libghostty-vt` (vendored, shared archive) | `libghostty-vt` (vendored, shared archive) |
| Workspace | `mac/Sources/Roost/Workspace.swift` (`@MainActor`) | `crates/roost-engine/src/workspace.rs` (shared) | `crates/roost-engine/src/workspace.rs` (shared) |
| PTY supervisor | `mac/Sources/Roost/PtySupervisor.swift` (forkpty + DispatchSourceRead) | `crates/roost-engine/src/pty.rs` (`portable-pty` + tokio tasks, shared) | `crates/roost-engine/src/pty.rs` (shared) |
| Persistence | `state.json` via tmp + fsync + `replaceItemAt` | `state.json` via tmp + fsync + rename + parent-dir fsync (`crates/roost-engine/src/persistence.rs`, shared) | `crates/roost-engine/src/persistence.rs` (shared) |
| IPC server | `mac/Sources/Roost/IPCServer.swift` (Darwin sockets) | `crates/roost-ipc/src/server.rs` (tokio `UnixListener`, shared) | `crates/roost-ipc/src/server.rs` (shared) |
| IPC wire types | `mac/Sources/Roost/IPCMessages.swift` (Codable) | `crates/roost-ipc/src/messages.rs` (serde, shared) | `crates/roost-ipc/src/messages.rs` (shared) |
| OSC scanning | `mac/Sources/Roost/OscScanner.swift` per `TerminalView` | `roost-osc` crate + `crates/roost-engine/src/osc.rs` (`OscRouter`, shared) per per-tab drain task | same (shared) |
| Agent state model (shell/lifecycle/attention/ownership axes, `tab.state` derivation) | `mac/Sources/Roost/AgentState.swift` | `crates/roost-ipc/src/agent.rs` (shared) | `crates/roost-ipc/src/agent.rs` (shared) |
| Agent adapters (Claude Code today) | `roostctl claude-hook` (binary from `crates/roost-cli`, links `crates/roost-agent`) — same binary on all three UIs | (same) | (same) |
| Single-instance (two flocks: socket/bind + state — see [Paths](paths.md#two-single-instance-locks)) | `mac/Sources/Roost/SingleInstance.swift` (flock via `@_silgen_name`) | `crates/roost-engine/src/single_instance.rs` (`File::try_lock`, shared) | `crates/roost-engine/src/single_instance.rs` (shared) |
| Shell-integration CLI | `roostctl` (binary from `crates/roost-cli`) — same binary on all three UIs | (same) | (same) |

The UIs are written separately and idiomatic to their platform; the JSON IPC wire format is shared across all three (via the `roost-ipc` crate on the Rust side + its hand-mirrored Swift counterpart in `IPCMessages.swift`), and the two Linux UIs additionally share `roost-engine` (workspace, PTY, persistence, events, single-instance) and `roost-ui-model` (config, theme, keybinds, palette, providers, agent/notification projections) — see [Shared Rust engine](../development/shared-rust-engine.md).

## Repository layout

```text
crates/
  roost-ipc/              # JSON wire format, framing, client, server, paths, target picker
  roost-agent/            # Pure agent adapters (Claude Code today) — hook event JSON in,
                          # tab.agent_report params out; no I/O, no socket, no clap
  roost-engine/           # Toolkit-neutral workspace, PTY, persistence, events, IPC dispatch — shared by both Linux UIs
  roost-ui-model/         # Toolkit-neutral config, theme, keybind, palette, provider, agent/notification projections
  roost-vt/               # libghostty-vt FFI wrapper (--features ffi)
  roost-osc/              # OSC scanner + state machine
  roost-cli/              # roostctl binary
  roost-linux/            # Linux UI (gtk4-rs) — in-repo development/parity implementation
  roost-iced/             # Linux UI (iced) — what the packaged .deb ships as /usr/bin/roost
mac/
  Sources/Roost/          # Swift Mac UI — embeds Workspace + PtySupervisor + IPC server
  Resources/              # themes, Info.plist.template, Roost.entitlements
  Tests/RoostTests/       # swift-testing test suite
  scripts/bundle.sh       # SwiftPM → .app bundle + embedded roostctl + ad-hoc codesign
docs/
  reference/ipc.md        # JSON IPC wire spec — canonical
  archive/roost.proto     # Historical reference (the pre-M7 gRPC schema)
third_party/ghostty/      # Vendored libghostty-vt build
```

## Hot path

PTY bytes flow `kernel → master fd → in-process drain task → libghostty-vt vt_write → renderer`. Everything is in the same process; the IPC socket carries only control messages (`tab.open`, `tab.write`, `events.subscribe`, etc.) and event broadcasts. The renderer never sees the wire.

```mermaid
flowchart LR
    CLI["roostctl notify"]
    Hook["roostctl claude-hook"]
    OSC["printf '\\033]9;…'"]
    PTY["PTY supervisor<br/>(in-process)"]
    Workspace["Workspace<br/>(in-process)"]
    Scanner["OSC scanner<br/>(per TerminalView)"]
    UI["UI event handler<br/>(main thread)"]
    Indicator["per-tab indicator"]
    Stripe["sidebar rollup stripe"]
    Banner["desktop notification"]
    IPC["JSON IPC server"]

    CLI --> IPC
    Hook --> IPC
    IPC --> Workspace
    OSC --> PTY --> Scanner --> Workspace
    Workspace --> UI
    UI --> Indicator
    UI --> Stripe
    UI --> Banner
```

The wire surface is small enough to inspect by hand:

```bash
echo '{"id":"1","op":"identify","params":{}}' | nc -U ~/Library/Caches/Roost/roost.sock
```

## Threading

Both UI toolkits (AppKit, GTK4) are single-threaded. Widget operations and `libghostty-vt` calls must run on the main thread.

| Layer | Thread |
|---|---|
| UI widgets, draw, input | Main thread only |
| `libghostty-vt` terminal handle + `vt_write` | Main thread only |
| PTY read (master fd) | `DispatchSourceRead` background queue (Mac) / dedicated tokio task (Linux) |
| PTY write | Main thread (`LocalClient.writeTab`) |
| OSC dispatch | Main thread (hopped from the read queue) |
| IPC accept loop | Detached `Task` (Mac) / tokio task (Linux) — never blocks main |
| IPC handler dispatch | Per-connection `Task` (Mac) / tokio task (Linux); mutations hop to main |
| `state.json` writes | Main thread (small; atomic via tmp + rename) |

The Mac PTY read path uses a dedicated pattern: the `DispatchSourceRead` closure is installed via a `nonisolated static` helper so Swift 6 doesn't infer `@MainActor` isolation on the closure body (which would trip `dispatch_assert_queue(main)` from the dispatch worker thread). Bytes bridge to the main actor through a `Sendable AsyncStream<InternalEvent>` that a drain `Task { @MainActor in ... }` consumes — see `mac/Sources/Roost/PtySupervisor.swift` for the comment block that walks through this.

## Boundaries

- Each UI process owns its workspace, PTY supervisor, and IPC server. There is no separate daemon. State is in memory + the bundle-profile `state.json` file.
- `libghostty-vt` lives inside each UI for VT parsing + rendering.
- OSC scanning lives in the UI (`OscScanner.swift` on macOS, `roost-osc` crate on Linux) because OSC parsing walks the same byte stream the VT parser does. OSC events apply directly to the local workspace via `LocalClient.applyOSC`.
- Terminal *query* replies (the program asking the terminal for its colors, device attributes, etc.) split across two channels — embedder-synthesized OSC color replies vs. libghostty-answered device replies. See [Terminal query replies](terminal-queries.md) for which is which and why.
- The IPC server is per-UI: external tooling (`roostctl`, Claude hooks) talks to the bundle profile's socket. Dev builds of iced use isolated `Roost-iced`/`roost-iced` paths alongside the existing Mac and GTK paths; the packaged `.deb` build adopts the production `roost`/`Roost` profile instead (see [Paths & Environment](paths.md)). `roostctl --target {mac,gtk,iced}` routes explicitly; with no selector it probes every distinct candidate concurrently and requires a choice if multiple UIs answer.
- Single-instance enforcement uses `flock(LOCK_EX | LOCK_NB)` on a pidfile next to the socket. Second launches read the holder PID and exit 0. `ROOST_ALLOW_MULTI=1` bypasses for dev/test workflows.

See [Vision → Decision log](../development/vision.md#decision-log) for the rationale behind each major choice.
