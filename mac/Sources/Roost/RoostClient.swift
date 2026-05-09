// gRPC client wrapper for talking to roost-core over a Unix domain socket.
//
// Uses grpc-swift v2 + the Posix HTTP/2 transport. UDS (not TCP) is the
// only supported transport: roost-core is a strictly local daemon, never
// remote. See docs/development/vision.md (DL-3, DL-4) for rationale.
//
// Phase 5 step 2: only `Identify()` is wired. `StreamPty` and
// `WatchEvents` follow once the AppKit window has the cell renderer +
// libghostty-vt FFI in place.

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

/// Result of attempting to handshake with `roost-core`. Error path
/// carries a human-readable summary, not a typed error — UI surfaces it
/// directly in the status panel today.
enum IdentifyOutcome: Sendable {
    case ok(RoostIdentity)
    case failed(String)
}

/// One-shot Identify against the daemon. Opens a transient gRPC client,
/// performs the handshake, returns the result. The transport is closed
/// before this function returns; subsequent calls open fresh clients.
///
/// Long-lived clients (for `StreamPty` and `WatchEvents`) come in
/// follow-up commits and will keep a single transport open for the
/// lifetime of the window.
func runIdentify(socketPath: String) async -> IdentifyOutcome {
    do {
        let transport = try HTTP2ClientTransport.Posix(
            target: .unixDomainSocket(path: socketPath),
            transportSecurity: .plaintext
        )
        return try await withGRPCClient(transport: transport) { client in
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
