// Mirror of `crates/roost-linux/src/rollup.rs`'s test module — same
// case set so the two UIs stay byte-equivalent in their rollup picks.

import Foundation
import Testing
@testable import Roost

/// A tab an agent owns, mid-turn.
private func owned(_ lifecycle: AgentLifecycle) -> AgentTabState {
    AgentTabState(
        shell: .atPrompt,
        lifecycle: lifecycle,
        ownership: Ownership(source: "claude", sessionID: "s1")
    )
}

/// A plain shell with no agent.
private func shell(_ shell: ShellState) -> AgentTabState {
    AgentTabState(shell: shell)
}

/// The production call shape: tabs in, presented lifecycles out.
private func rollup(_ tabs: [AgentTabState]) -> AgentLifecycle {
    projectRollup(tabs.map(Agent.effectiveLifecycle))
}

@Test
func projectRollup_emptyListIsNone() {
    #expect(rollup([]) == .inactive)
}

@Test
func projectRollup_allIdleShellsIsNone() {
    #expect(rollup([shell(.atPrompt), shell(.unknown)]) == .inactive)
}

@Test
func projectRollup_foregroundProcessIsRunning() {
    #expect(rollup([shell(.foregroundProcess)]) == .working)
}

@Test
func projectRollup_needsInputOutranksRunning() {
    #expect(rollup([owned(.working), owned(.waiting)]) == .waiting)
}

@Test
func projectRollup_runningOutranksIdle() {
    #expect(rollup([owned(.finished), shell(.foregroundProcess)]) == .working)
}

@Test
func projectRollup_idleOutranksNone() {
    #expect(rollup([shell(.atPrompt), owned(.finished)]) == .finished)
}

/// `failed` sits above `waiting` — the whole reason the rollup ranks on
/// the agent axis instead of the legacy `TabState`, which collapses the
/// two.
@Test
func projectRollup_failedOutranksNeedsInput() {
    #expect(rollup([owned(.waiting), owned(.failed)]) == .failed)
}

// The three tests below replace `hookActiveSuppressesNeedsInput` /
// `hookActiveSuppressesRunning` / `hookActiveOnAllFallsBackToNone`,
// which pinned the behavior plan 002 §2.2(a) reverses: an agent-owned
// tab used to be dropped from the rollup entirely.

@Test
func projectRollup_agentOwnedNeedsInputParticipates() {
    #expect(rollup([owned(.waiting), shell(.foregroundProcess)]) == .waiting)
}

@Test
func projectRollup_agentOwnedRunningParticipates() {
    #expect(rollup([owned(.working), owned(.finished)]) == .working)
}

@Test
func projectRollup_allTabsAgentOwnedStillRanks() {
    #expect(rollup([owned(.working), owned(.waiting)]) == .waiting)
}

/// Ownership without a live lifecycle falls through to the shell axis —
/// the `D`/`A` failsafe, seen from the sidebar.
@Test
func projectRollup_inactiveOwnerFallsThroughToShell() {
    var tab = owned(.inactive)
    tab.shell = .foregroundProcess
    #expect(rollup([tab]) == .working)
}

/// A `failed` lifecycle left behind on a tab whose owner is gone must
/// not outrank live tabs.
@Test
func projectRollup_failedWithoutAnOwnerFallsThroughToShell() {
    let tab = AgentTabState(shell: .atPrompt, lifecycle: .failed, ownership: nil)
    #expect(rollup([tab]) == .inactive)
}

@Test
func rollupColor_isNilOnlyForInactive() {
    #expect(rollupColor(for: .inactive) == nil)
    #expect(rollupColor(for: .working) != nil)
    #expect(rollupColor(for: .waiting) != nil)
    #expect(rollupColor(for: .finished) != nil)
    #expect(rollupColor(for: .failed) != nil)
}

/// `failed` must not reuse `waiting`'s colour: distinguishing "the agent
/// wants you" from "the agent died" is the point of the fifth state.
@Test
func rollupColor_failedIsDistinctFromWaiting() {
    #expect(rollupColor(for: .failed) != rollupColor(for: .waiting))
}
