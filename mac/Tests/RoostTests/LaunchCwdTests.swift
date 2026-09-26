// LaunchCwdTests — pure-helper tests for `RoostApp.launchCwd`, the
// seam `activeLaunchCwd` (⌘T / the launcher / providers) folds the
// client's `inheritedCwd(tabID:)` answer into (#556). The directory
// check itself lives in `LocalClient.inheritedCwd`, so this only pins
// the fallback: the client's answer wins when it has one, else the
// project's cwd.

import Testing

@testable import Roost

@Suite("launch cwd resolution")
struct LaunchCwdTests {
    @Test func clientAnswerPreferredWhenPresent() {
        #expect(RoostApp.launchCwd(inherited: "/n", project: "/p") == "/n")
    }

    @Test func projectIsTheFallback() {
        #expect(RoostApp.launchCwd(inherited: nil, project: "/p") == "/p")
    }
}
