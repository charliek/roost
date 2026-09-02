// The two seams where the Mac UI has to stay in step with the core's
// agent model (plan 002): the active-tab sync a UI-initiated selection
// performs, and the `.resync` reconcile's refresh of a session the UI
// already holds. Both are `RoostApp` decisions extracted as pure
// helpers so they can be exercised without an AppKit window — same
// pattern as `RoostApp.activeTabIndex` in `TabPillStateTests`.

import Foundation
import Testing

@testable import Roost

@MainActor
@Suite("UI ↔ core sync")
struct AppCoreSyncTests {
    // MARK: Active-tab sync

    @Test func uiInitiatedSelectionPushesToTheCore() {
        #expect(
            RoostApp.shouldSyncCoreActiveTab(
                selected: 7, coreActive: 3, hasNotification: false, applyingCoreEvent: false
            )
        )
    }

    @Test func syncIsSkippedWhenTheCoreAlreadyAgrees() {
        #expect(
            !RoostApp.shouldSyncCoreActiveTab(
                selected: 7, coreActive: 7, hasNotification: false, applyingCoreEvent: false
            )
        )
    }

    /// #369: `focusTab` also acknowledges, so "the core already agrees"
    /// is no longer the whole test. A notification raised while the
    /// window was unfocused lands on the *active* tab, and clicking the
    /// pill you are already on must still clear it — the only route to
    /// the core is this push.
    @Test func anAlreadyActiveBadgedTabStillPushesToAcknowledge() {
        #expect(
            RoostApp.shouldSyncCoreActiveTab(
                selected: 7, coreActive: 7, hasNotification: true, applyingCoreEvent: false
            )
        )
    }

    /// A selection made while *reacting* to `active.changed` must not
    /// echo back. The UI's `focusTab` passes through the project's
    /// remembered tab before landing on the requested one, so a push
    /// there would emit a second `active.changed` the UI reacts to in
    /// turn — the two selections ping-pong without converging.
    @Test func aSelectionMadeWhileReactingToTheCoreNeverEchoes() {
        #expect(
            !RoostApp.shouldSyncCoreActiveTab(
                selected: 7, coreActive: 3, hasNotification: false, applyingCoreEvent: true
            )
        )
        #expect(
            !RoostApp.shouldSyncCoreActiveTab(
                selected: 7, coreActive: 7, hasNotification: false, applyingCoreEvent: true
            )
        )
    }

    /// The echo guard dominates the acknowledge: the UI is reacting to
    /// a core event that already carried the false-edge, so pushing here
    /// would answer the core's own broadcast with a second one.
    @Test func aBadgedTabReactingToTheCoreStillNeverEchoes() {
        #expect(
            !RoostApp.shouldSyncCoreActiveTab(
                selected: 7, coreActive: 7, hasNotification: true, applyingCoreEvent: true
            )
        )
        #expect(
            !RoostApp.shouldSyncCoreActiveTab(
                selected: 7, coreActive: 3, hasNotification: true, applyingCoreEvent: true
            )
        )
    }

    /// Why the sync exists. Suppression (plan §3.5) is decided against
    /// the *core's* active tab, so a UI selection the core never hears
    /// about routes both directions wrong: the visible tab's attention
    /// is recorded — and plan AC 10 forbids it surfacing on the next
    /// switch — while the background tab's is dropped outright.
    @Test func theCoresActiveTabDecidesSuppression() throws {
        let ws = Workspace()
        let p = ws.createProject(name: "p", cwd: "")
        let a = try ws.openTab(projectID: p.id, cwd: "/", title: "a")
        let b = try ws.openTab(projectID: p.id, cwd: "/", title: "b")
        ws.setWindowFocused(true)
        _ = try ws.focusTab(a.id)

        // UI on B, core still on A — exactly backwards.
        #expect(try ws.raiseAttention(b.id, title: "t", body: "y", source: .structured))
        #expect(try !ws.raiseAttention(a.id, title: "t", body: "y", source: .structured))

        // …and right once the selection is pushed through, as
        // `selectTab(at:)` now does.
        try ws.setTabHasNotification(b.id, hasPending: false)
        _ = try ws.focusTab(b.id)
        #expect(try !ws.raiseAttention(b.id, title: "t", body: "y", source: .structured))
        #expect(try ws.raiseAttention(a.id, title: "t", body: "y", source: .structured))
    }

    // MARK: `.resync` agent reconcile

    private var failedAgent: AgentTabState {
        AgentTabState(
            shell: .foregroundProcess,
            lifecycle: .failed,
            ownership: Ownership(source: "claude", sessionID: "s1", lastEventAt: 100)
        )
    }

    /// The boot gap swallows `agent_report.changed` as readily as
    /// `tab.opened`: a session the UI already holds can be sitting on a
    /// stale record, and the attach loop never looks at it — the pill
    /// and rollup would stay inactive while `tab.list` reports `failed`.
    @Test func resyncFlagsAnExistingSessionsStaleAgentRecord() {
        let stale = RoostApp.staleAgentTabIDs(
            snapshot: [(id: 1, agent: failedAgent), (id: 2, agent: AgentTabState())],
            sessions: [(id: 1, agent: AgentTabState()), (id: 2, agent: AgentTabState())]
        )
        #expect(stale == [1], "only the drifted tab, so an unchanged one doesn't churn the UI")
    }

    /// A snapshot tab with no session is the attach loop's job, and a
    /// session still mid-open carries no id to match on.
    @Test func resyncIgnoresUnmatchedTabsAndSessions() {
        let sessions: [(id: Int64?, agent: AgentTabState)] = [(id: nil, agent: AgentTabState())]
        #expect(
            RoostApp.staleAgentTabIDs(
                snapshot: [(id: 2, agent: failedAgent)],
                sessions: sessions
            ).isEmpty
        )
        #expect(
            RoostApp.staleAgentTabIDs(snapshot: [], sessions: sessions).isEmpty
        )
    }
}
