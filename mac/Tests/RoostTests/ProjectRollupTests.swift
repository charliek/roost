// Mirror of `crates/roost-linux/src/rollup.rs`'s test module — same
// case set so the two UIs stay byte-equivalent in their rollup picks.

import Foundation
import Testing
@testable import Roost

@Test
func projectRollup_emptyListIsNone() {
    #expect(projectRollup(tabs: []) == .none)
}

@Test
func projectRollup_allNoneIsNone() {
    let tabs: [(state: TabAgentState, hookActive: Bool)] = [
        (.none, false),
        (.none, false),
    ]
    #expect(projectRollup(tabs: tabs) == .none)
}

@Test
func projectRollup_singleRunning() {
    #expect(projectRollup(tabs: [(.running, false)]) == .running)
}

@Test
func projectRollup_singleNeedsInput() {
    #expect(projectRollup(tabs: [(.needsInput, false)]) == .needsInput)
}

@Test
func projectRollup_singleIdle() {
    #expect(projectRollup(tabs: [(.idle, false)]) == .idle)
}

@Test
func projectRollup_needsInputOutranksRunning() {
    let tabs: [(state: TabAgentState, hookActive: Bool)] = [
        (.running, false),
        (.needsInput, false),
    ]
    #expect(projectRollup(tabs: tabs) == .needsInput)
}

@Test
func projectRollup_runningOutranksIdle() {
    let tabs: [(state: TabAgentState, hookActive: Bool)] = [
        (.idle, false),
        (.running, false),
    ]
    #expect(projectRollup(tabs: tabs) == .running)
}

@Test
func projectRollup_idleOutranksNone() {
    let tabs: [(state: TabAgentState, hookActive: Bool)] = [
        (.none, false),
        (.idle, false),
    ]
    #expect(projectRollup(tabs: tabs) == .idle)
}

@Test
func projectRollup_hookActiveSuppressesNeedsInput() {
    // If the only NeedsInput tab has its hook active, the rollup falls
    // back to whatever the other tabs say.
    let tabs: [(state: TabAgentState, hookActive: Bool)] = [
        (.needsInput, true),  // hook-active → suppressed
        (.running, false),
    ]
    #expect(projectRollup(tabs: tabs) == .running)
}

@Test
func projectRollup_hookActiveSuppressesRunning() {
    let tabs: [(state: TabAgentState, hookActive: Bool)] = [
        (.running, true),  // hook-active → suppressed
        (.idle, false),
    ]
    #expect(projectRollup(tabs: tabs) == .idle)
}

@Test
func projectRollup_hookActiveOnAllFallsBackToNone() {
    let tabs: [(state: TabAgentState, hookActive: Bool)] = [
        (.running, true),
        (.needsInput, true),
    ]
    #expect(projectRollup(tabs: tabs) == .none)
}

@Test
func tabAgentState_fromProtoMapsCorrectly() {
    #expect(TabAgentState.fromProto(0) == .none)  // unspecified
    #expect(TabAgentState.fromProto(1) == .none)
    #expect(TabAgentState.fromProto(2) == .running)
    #expect(TabAgentState.fromProto(3) == .needsInput)
    #expect(TabAgentState.fromProto(4) == .idle)
    // Unknown defensively maps to none.
    #expect(TabAgentState.fromProto(99) == .none)
}

@Test
func rollupState_nsColorIsNilOnlyForNone() {
    #expect(RollupState.none.nsColor == nil)
    #expect(RollupState.running.nsColor != nil)
    #expect(RollupState.needsInput.nsColor != nil)
    #expect(RollupState.idle.nsColor != nil)
}
