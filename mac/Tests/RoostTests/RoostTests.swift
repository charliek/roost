// Smoke tests for the Mac executable. Kept tight on purpose — the real
// behavior coverage will live in Rust integration tests against
// `roost-core`. These exist mainly so `swift test` runs on macOS CI and
// catches gross packaging regressions, plus pin a few invariants that
// would silently break the daemon-discovery story if they regressed.

import Foundation
import Testing
@testable import Roost

@Test
func defaultSocketPathUsesHomeOnMac() {
    let socket = RoostApp.defaultSocketPath(environment: [
        "HOME": "/Users/tester",
    ])
    #expect(socket == "/Users/tester/Library/Caches/roost/roost.sock")
}

@Test
func defaultSocketPathIgnoresXdgRuntimeDirOnMac() {
    // The daemon's macOS branch is HOME-derived only, so the Mac
    // client must not chase XDG_RUNTIME_DIR even if a shell exports
    // it. Both sides agreeing matters more than mirroring Linux.
    let socket = RoostApp.defaultSocketPath(environment: [
        "XDG_RUNTIME_DIR": "/run/user/501",
        "HOME": "/Users/tester",
    ])
    #expect(socket == "/Users/tester/Library/Caches/roost/roost.sock")
}

@Test
func defaultSocketPathFallsBackToTmpWhenHomeMissing() {
    let socket = RoostApp.defaultSocketPath(environment: [:])
    #expect(socket == "/tmp/roost.sock")
}

@Test
func defaultSocketPathSkipsEmptyHome() {
    // Sandboxed launchd processes can inherit HOME="" (set but empty).
    // The function must fall through to /tmp, not yield
    // "/Library/Caches/roost/roost.sock".
    let socket = RoostApp.defaultSocketPath(environment: [
        "HOME": "",
    ])
    #expect(socket == "/tmp/roost.sock")
}

@Test
func defaultSocketPathSkipsRelativeHome() {
    // A relative HOME would yield an unusable socket path; fall
    // through to /tmp instead.
    let socket = RoostApp.defaultSocketPath(environment: [
        "HOME": "relative/path",
    ])
    #expect(socket == "/tmp/roost.sock")
}

@Test
func defaultSocketPathInvariants() {
    let socket = RoostApp.defaultSocketPath()
    #expect(!socket.isEmpty)
    #expect(socket.hasPrefix("/"))
    #expect(socket.contains("roost"))
}

// MARK: - Live-daemon regression guard
//
// Pinned because the gRPC layer between grpc-swift-nio-transport and
// tonic has subtle interop edges (eg the `:authority` pseudo-header
// must not contain `/` over UDS, or h2 RST_STREAMs every request).
// `swift test` alone can't catch that — only an actual round-trip
// can. This test runs against a live daemon when the calling
// environment sets `ROOST_LIVE_TEST_SOCKET` to a UDS path; locally
// without it the test is a no-op so `swift test` stays runnable
// off-CI. CI's swift-mac job starts `roost-core --socket <tmp>` in
// the background before invoking `swift test` with the env var set.

@Test(.enabled(if: ProcessInfo.processInfo.environment["ROOST_LIVE_TEST_SOCKET"]?.isEmpty == false))
func liveIdentifyAgainstDaemonSocket() async throws {
    let socket = ProcessInfo.processInfo.environment["ROOST_LIVE_TEST_SOCKET"] ?? ""
    let outcome = await runIdentify(socketPath: socket)
    switch outcome {
    case .ok(let identity):
        #expect(!identity.daemonVersion.isEmpty)
        #expect(identity.pid > 0)
        #expect(identity.protocolVersion >= 1)
    case .failed(let reason):
        Issue.record("live Identify against \(socket) failed: \(reason)")
    }
}
