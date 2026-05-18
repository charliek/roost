// gRPC client wrapper for talking to roost-core over a Unix domain socket.
//
// Uses grpc-swift v2's `withGRPCClient(transport:)` pattern matching the
// canonical hello-world example at
// github.com/grpc/grpc-swift-2/blob/main/Examples/hello-world/Sources/Subcommands/Greet.swift.
// UDS (not TCP) is the only transport: roost-core is a strictly local
// daemon, never remote. See docs/development/vision.md (DL-3, DL-4).
//
// Phase 5 step 2: only `Identify()` is wired. `StreamPty` and
// `WatchEvents` come once the AppKit window has the cell renderer +
// libghostty-vt FFI.

import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2Posix

/// HTTP/2 `:authority` pseudo-header used for every UDS connection.
///
/// grpc-swift-nio-transport's UDS resolver defaults `:authority` to
/// the raw socket path when no override is supplied. tonic's
/// underlying `h2` crate then rejects it as malformed (the path's
/// `/` characters fail RFC 3986 authority validation), terminating
/// the stream with RST_STREAM(0x1) before our RPC ever runs. The
/// gRPC-ecosystem convention over UDS — used by Go, grpcurl,
/// Kubernetes CSI — is the literal `"localhost"`. tonic and the
/// Rust client side both accept it.
///
/// See:
///   * grpc-swift-nio-transport NameResolver+UDS.swift `authority:`
///     parameter
///   * hyperium/tonic#243 (canonical issue)
private let udsAuthority = "localhost"

/// A snapshot of `roost-core`'s identity, mirroring the proto
/// `IdentifyResponse` with idiomatic Swift names.
struct RoostIdentity: Sendable {
    let socketPath: String
    let pid: Int32
    let activeProjectID: Int64
    let activeTabID: Int64
    let daemonVersion: String
    let protocolVersion: UInt32
}

/// Result of attempting to handshake with `roost-core`. The error path
/// carries a human-readable summary for the UI to surface.
enum IdentifyOutcome: Sendable {
    case ok(RoostIdentity)
    case failed(String)
}

/// Default deadline for the handshake. A reachable-but-stalled daemon
/// shouldn't keep the UI in "connecting…" forever — 5s is plenty for a
/// local UDS round-trip on the loopback path, and short enough that a
/// real failure surfaces quickly.
private let identifyTimeout: Duration = .seconds(5)

/// One-shot Identify against the daemon, with a hard timeout.
///
/// We race the gRPC call against a `Task.sleep`-backed deadline rather
/// than passing `CallOptions(timeout:)` so the deadline shape is
/// independent of grpc-swift's evolving public surface — `CallOptions`
/// has an internal initializer in v2 and only exposes a `.defaults`
/// static factory you'd then mutate. Doing the timeout in user code
/// avoids guessing at the right call-options overload entirely.
func runIdentify(socketPath: String) async -> IdentifyOutcome {
    await withTaskGroup(of: IdentifyOutcome.self) { group in
        group.addTask { await runIdentifyUnbounded(socketPath: socketPath) }
        group.addTask {
            try? await Task.sleep(for: identifyTimeout)
            return .failed("identify timed out after \(identifyTimeout)")
        }
        // First task to finish wins; cancel the other so we don't leak it.
        let first = await group.next() ?? .failed("identify task group returned no result")
        group.cancelAll()
        return first
    }
}

/// The actual gRPC call without any deadline. Wrapped by `runIdentify`
/// for callers; a future StreamPty client will hold a long-lived gRPC
/// client over the same transport.
private func runIdentifyUnbounded(socketPath: String) async -> IdentifyOutcome {
    do {
        return try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            let response = try await roost.identify(
                .with {
                    $0.clientName = "roost-mac"
                    $0.clientVersion = clientVersion()
                }
            )
            return .ok(
                RoostIdentity(
                    socketPath: response.socketPath,
                    pid: response.pid,
                    activeProjectID: response.activeProjectID,
                    activeTabID: response.activeTabID,
                    daemonVersion: response.daemonVersion,
                    protocolVersion: response.protocolVersion
                )
            )
        }
    } catch {
        return .failed("\(error)")
    }
}

private func clientVersion() -> String {
    Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0.1.0"
}

// =============================================================================
// Phase 5.5a: long-lived shell session over StreamPty
// =============================================================================

/// Open a fresh tab on `roost-core` and attach a bidirectional
/// `StreamPty` to it. Returns when the stream ends — daemon shutdown,
/// shell exit, or the wrapping Task being cancelled.
///
/// Output bytes are delivered to `onOutput` as they arrive. The
/// callback runs on the gRPC background task; the caller is
/// responsible for hopping to the main actor before touching
/// AppKit views.
///
/// `onTabOpened` fires once with the daemon-assigned tab id as soon
/// as `OpenTab` returns, before the StreamPty stream attaches. The
/// caller uses this to label the tab in the UI and to drive an
/// explicit `CloseTab` later. Like `onOutput`, it runs off the main
/// actor; consumers must hop before touching AppKit state.
///
/// `keystrokes` carries inbound input bytes the renderer captured
/// via `keyDown` events; each emitted chunk gets forwarded to the
/// daemon as a `PtyInput`. Closing the keystroke stream
/// (`continuation.finish()`) closes the writer and ends the
/// session.
/// Outbound events the UI can send on the StreamPty bidi stream.
/// Phase 6a M3 lifts the previous "stream is just keystroke bytes"
/// shape so window-resize can ride the same channel as input.
enum PtyClientEvent: Sendable {
    case input(Data)
    case resize(cols: UInt16, rows: UInt16)
}

func runShellSession(
    socketPath: String,
    projectID: Int64 = 0,
    cols: UInt16 = 80,
    rows: UInt16 = 24,
    title: String = "roost-mac",
    keystrokes: AsyncStream<PtyClientEvent>,
    onTabOpened: @escaping @Sendable (Int64) -> Void,
    onOutput: @escaping @Sendable (Data) -> Void
) async {
    do {
        try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)

            // Spawn a tab on the daemon. Empty argv = the daemon
            // resolves $SHELL on its end. cwd is left empty so the
            // daemon picks its own (typically the user's home).
            // projectID = 0 lets the daemon's `ensure_default_project`
            // kick in (legacy single-project behavior); non-zero
            // pins the tab to the sidebar-selected project.
            let opened = try await roost.openTab(
                .with {
                    $0.projectID = projectID
                    $0.argv = []
                    $0.cwd = ""
                    $0.cols = UInt32(cols)
                    $0.rows = UInt32(rows)
                    $0.title = title
                }
            )
            let tabID = opened.tab.id
            onTabOpened(tabID)

            try await roost.streamPty { writer in
                // First message MUST be PtyAttach per the proto.
                try await writer.write(
                    .with {
                        $0.attach = Roost_V1_PtyAttach.with {
                            $0.tabID = tabID
                            $0.cols = UInt32(cols)
                            $0.rows = UInt32(rows)
                        }
                    }
                )
                // Pump events -> PtyInput / PtyResize. Loop ends
                // naturally when the keystroke stream's continuation
                // finishes (eg when the window closes).
                for await event in keystrokes {
                    switch event {
                    case .input(let chunk):
                        try await writer.write(
                            .with {
                                $0.input = Roost_V1_PtyInput.with { $0.data = chunk }
                            }
                        )
                    case .resize(let cols, let rows):
                        try await writer.write(
                            .with {
                                $0.resize = Roost_V1_PtyResize.with {
                                    $0.cols = UInt32(cols)
                                    $0.rows = UInt32(rows)
                                }
                            }
                        )
                    }
                }
            } onResponse: { response in
                for try await message in response.messages {
                    if case .output(let out) = message.kind {
                        onOutput(out.data)
                    }
                    // PtyExit ends the stream from the server side;
                    // the for-loop will then exit naturally.
                }
            }
        }
    } catch {
        // Logged to stderr so the user sees session failures even
        // if the UI doesn't surface them yet. Phase 5.5c adds an
        // error path through the status panel.
        FileHandle.standardError.write(
            Data("[Roost.mac] shell session ended: \(error)\n".utf8)
        )
    }
}

/// Best-effort `CloseTab` on the daemon. Used when the UI closes a
/// tab so the daemon's PTY supervisor reaps the child immediately
/// rather than waiting for the StreamPty stream to drain.
///
/// Failures are logged to stderr only. The caller has already torn
/// down its keystroke stream by the time this runs, so the daemon
/// will eventually clean up even if this RPC never lands.
func closeShellTab(socketPath: String, tabID: Int64) async {
    do {
        try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            _ = try await roost.closeTab(
                .with { $0.tabID = tabID }
            )
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] closeTab(\(tabID)) failed: \(error)\n".utf8)
        )
    }
}

// =============================================================================
// Phase 6a step 2: project lifecycle
// =============================================================================

/// Plain-Swift mirror of the proto `Project` so the UI can hold a
/// list without leaning on generated grpc-swift types in its view
/// model. Tabs are intentionally not modeled here — the UI tracks
/// its own `TabSession` instances in `RoostApp`.
struct ProjectSnapshot: Sendable, Hashable {
    let id: Int64
    let name: String
    let cwd: String
}

/// Fetch the full project list (without tabs — see comment on
/// `ProjectSnapshot`). One round-trip; returns `[]` on any error so
/// the caller can decide whether to surface a UI state.
func listProjects(socketPath: String) async -> [ProjectSnapshot] {
    do {
        return try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            let response = try await roost.listTabs(.with { _ in })
            return response.projects.map {
                ProjectSnapshot(id: $0.id, name: $0.name, cwd: $0.cwd)
            }
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] listProjects failed: \(error)\n".utf8)
        )
        return []
    }
}

/// Best-effort `CreateProject`. `name = ""` → daemon picks
/// `"Untitled <n>"`.
func createProject(socketPath: String, name: String, cwd: String) async -> ProjectSnapshot? {
    do {
        return try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            let response = try await roost.createProject(
                .with {
                    $0.name = name
                    $0.cwd = cwd
                }
            )
            let p = response.project
            return ProjectSnapshot(id: p.id, name: p.name, cwd: p.cwd)
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] createProject failed: \(error)\n".utf8)
        )
        return nil
    }
}

/// Best-effort `RenameProject`. Errors logged, not surfaced.
/// Rename a tab via the daemon. The daemon-side handler sets the
/// per-tab `user_titled` lock so subsequent OSC 1/2 emissions from
/// the shell stop overwriting (`crates/roost-core/src/service.rs:416`).
/// Same semantics as Go's `ws.RenameTab`.
func setTabTitle(socketPath: String, tabID: Int64, title: String) async {
    do {
        try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            _ = try await roost.setTabTitle(
                .with {
                    $0.tabID = tabID
                    $0.title = title
                }
            )
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] setTabTitle(\(tabID)) failed: \(error)\n".utf8)
        )
    }
}

func renameProject(socketPath: String, projectID: Int64, name: String) async {
    do {
        try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            _ = try await roost.renameProject(
                .with {
                    $0.projectID = projectID
                    $0.name = name
                }
            )
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] renameProject(\(projectID)) failed: \(error)\n".utf8)
        )
    }
}

/// M2 of `goal-mac-parity-2026-05-18.md`: persist a new tab-order
/// sequence within a project. The daemon validates that every id in
/// `tabIDs` belongs to `projectID`; missing tabs keep their existing
/// position, unknown ids fail with INVALID_ARGUMENT.
func reorderTabs(socketPath: String, projectID: Int64, tabIDs: [Int64]) async {
    do {
        try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            _ = try await roost.reorderTabs(
                .with {
                    $0.projectID = projectID
                    $0.tabIds = tabIDs
                }
            )
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] reorderTabs(\(projectID)) failed: \(error)\n".utf8)
        )
    }
}

/// M3 of `goal-mac-parity-2026-05-18.md`: persist a new sidebar
/// project-order sequence. Same shape as `reorderTabs` but workspace-
/// scoped — every id is rejected if it doesn't exist in the project
/// table.
func reorderProjects(socketPath: String, projectIDs: [Int64]) async {
    do {
        try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            _ = try await roost.reorderProjects(
                .with {
                    $0.projectIds = projectIDs
                }
            )
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] reorderProjects failed: \(error)\n".utf8)
        )
    }
}

// =============================================================================
// Phase 6a step 2c — WatchEvents subscription
// =============================================================================
//
// The Mac UI bootstraps its workspace by calling `listProjects` once on
// launch and otherwise relies on its own RPC replies to update local state.
// That's fine until a *second* client (the CLI, a future Linux UI, or just
// a `roost-cli-rs project create` from another shell) mutates the daemon —
// the Mac UI doesn't see the change until restart. `watchEvents` is the
// daemon's server-stream of every workspace mutation; subscribing to it
// keeps the sidebar + tab list converged with daemon state without
// polling. See the goal doc M1 slice (b) for the full handler matrix.
//
// gRPC's server-stream is backed by `tokio::sync::broadcast` (capacity
// 256) daemon-side. A slow UI that falls behind gets `Lagged` and should
// re-`listProjects` to resync — handled in `RoostApp.subscribeToEvents`.

/// A long-lived server-stream of workspace mutation events. Yields
/// `Roost_V1_Event` values until the stream ends (e.g. daemon shutdown);
/// callers should bridge to a `@MainActor` consumer and re-subscribe on
/// stream end if they need reconnect-on-disconnect semantics.
///
/// On any underlying gRPC error the stream finishes; the error is logged
/// to stderr so a failed connect is debuggable without a UI surface.
func watchEvents(socketPath: String) -> AsyncStream<Roost_V1_Event> {
    AsyncStream { continuation in
        let task = Task {
            do {
                try await withGRPCClient(
                    transport: .http2NIOPosix(
                        target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                        transportSecurity: .plaintext
                    )
                ) { client in
                    let roost = Roost_V1_Roost.Client(wrapping: client)
                    try await roost.watchEvents(.with { _ in }) { response in
                        for try await event in response.messages {
                            if Task.isCancelled { return }
                            continuation.yield(event)
                        }
                    }
                }
            } catch {
                FileHandle.standardError.write(
                    Data("[Roost.mac] watchEvents stream ended: \(error)\n".utf8)
                )
            }
            continuation.finish()
        }
        continuation.onTermination = { _ in task.cancel() }
    }
}

/// Best-effort `ClearTabNotification` — fires `TabNotificationEvent
/// { has_pending: false }` for the tab, clearing the badge on
/// every watching client. Phase 6a P7 calls this from
/// `selectTab(at:)` so focusing a notified tab clears its badge.
func clearTabNotification(socketPath: String, tabID: Int64) async {
    do {
        try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            _ = try await roost.clearTabNotification(
                .with { $0.tabID = tabID }
            )
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] clearTabNotification(\(tabID)) failed: \(error)\n".utf8)
        )
    }
}

/// Best-effort `ReportOsc` — sends a pre-parsed (osc_command,
/// payload) tuple to the daemon's OSC routing layer (Phase 6a
/// P5). One-shot RPC; the UI's OscScanner produces these from
/// the PTY byte stream and dispatches them via this fn. Errors
/// log to stderr but don't fail loudly — a missed OSC update
/// (transient daemon hiccup, etc.) shouldn't crash the UI.
func reportOsc(
    socketPath: String,
    tabID: Int64,
    oscCommand: UInt32,
    payload: String
) async {
    do {
        try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            _ = try await roost.reportOsc(
                .with {
                    $0.tabID = tabID
                    $0.oscCommand = oscCommand
                    $0.payload = payload
                }
            )
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] reportOsc(tab=\(tabID), cmd=\(oscCommand)) failed: \(error)\n".utf8)
        )
    }
}

/// Best-effort `DeleteProject`. Cascade-deletes tabs daemon-side.
func deleteProject(socketPath: String, projectID: Int64) async {
    do {
        try await withGRPCClient(
            transport: .http2NIOPosix(
                target: .unixDomainSocket(path: socketPath, authority: udsAuthority),
                transportSecurity: .plaintext
            )
        ) { client in
            let roost = Roost_V1_Roost.Client(wrapping: client)
            _ = try await roost.deleteProject(
                .with { $0.projectID = projectID }
            )
        }
    } catch {
        FileHandle.standardError.write(
            Data("[Roost.mac] deleteProject(\(projectID)) failed: \(error)\n".utf8)
        )
    }
}
