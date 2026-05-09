// Smoke tests for the Mac executable. Kept tiny on purpose — the real
// behavior coverage will live in Rust integration tests against
// `roost-core`. These exist mainly so `swift test` runs on macOS CI and
// catches gross packaging regressions.

import Testing
@testable import Roost

@Test
func defaultSocketPathIsNonEmpty() {
    let socket = RoostApp.defaultSocketPath()
    #expect(!socket.isEmpty)
    #expect(socket.contains("roost"))
}

@Test
func defaultSocketPathIsAbsolute() {
    let socket = RoostApp.defaultSocketPath()
    #expect(socket.hasPrefix("/"))
}

