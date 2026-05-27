// New-tab cwd precedence (P3). The native read itself needs a live
// PTY, so we unit-test the pure precedence helper here; the end-to-end
// native read is exercised by tools/roosttest/test_shell_integration.py.

import Testing

@testable import Roost

@Suite("launch cwd resolution")
struct LaunchCwdTests {
    @Test func nativePreferredWhenPresent() {
        #expect(RoostApp.resolveLaunchCwd(native: "/n", live: "/l", project: "/p") == "/n")
    }

    @Test func liveWhenNativeMissingOrEmpty() {
        #expect(RoostApp.resolveLaunchCwd(native: nil, live: "/l", project: "/p") == "/l")
        #expect(RoostApp.resolveLaunchCwd(native: "", live: "/l", project: "/p") == "/l")
    }

    @Test func projectIsLastResort() {
        #expect(RoostApp.resolveLaunchCwd(native: nil, live: "", project: "/p") == "/p")
        #expect(RoostApp.resolveLaunchCwd(native: "", live: "", project: "/p") == "/p")
    }
}
