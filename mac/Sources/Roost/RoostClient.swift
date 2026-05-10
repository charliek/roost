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
                target: .unixDomainSocket(path: socketPath),
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
