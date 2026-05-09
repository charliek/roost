// Smoke tests for the Mac executable. Kept tight on purpose — the real
// behavior coverage will live in Rust integration tests against
// `roost-core`. These exist mainly so `swift test` runs on macOS CI and
// catches gross packaging regressions, plus pin a few invariants that
// would silently break the daemon-discovery story if they regressed.

import Testing
@testable import Roost

@Test
func defaultSocketPathPrefersXdgRuntimeDir() {
    let socket = RoostApp.defaultSocketPath(environment: [
        "XDG_RUNTIME_DIR": "/run/user/501",
        "HOME": "/Users/tester",
    ])
    #expect(socket == "/run/user/501/roost/roost.sock")
}

@Test
func defaultSocketPathFallsBackToHomeOnMac() {
    let socket = RoostApp.defaultSocketPath(environment: [
        "HOME": "/Users/tester",
    ])
    #expect(socket == "/Users/tester/Library/Caches/roost/roost.sock")
}

@Test
func defaultSocketPathFallsBackToTmpWhenEnvIsEmpty() {
    let socket = RoostApp.defaultSocketPath(environment: [:])
    #expect(socket == "/tmp/roost.sock")
}

@Test
func defaultSocketPathSkipsEmptyXdgRuntimeDir() {
    // launchd-spawned processes sometimes inherit XDG_RUNTIME_DIR=""
    // (set but empty). The function must fall through, not yield
    // "/roost/roost.sock".
    let socket = RoostApp.defaultSocketPath(environment: [
        "XDG_RUNTIME_DIR": "",
        "HOME": "/Users/tester",
    ])
    #expect(socket == "/Users/tester/Library/Caches/roost/roost.sock")
}

@Test
func defaultSocketPathSkipsRelativeHome() {
    // A relative HOME (or XDG_RUNTIME_DIR) would yield an unusable
    // socket path; we fall through to /tmp instead.
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
