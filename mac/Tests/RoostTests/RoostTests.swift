// Phase 2 placeholder test. Confirms `swift test` runs on macOS CI before
// any real test logic exists.

import Testing
@testable import Roost

@Test
func defaultSocketPathIsNonEmpty() {
    let socket = RoostMain.defaultSocketPath()
    #expect(!socket.isEmpty)
    #expect(socket.contains("roost"))
}
