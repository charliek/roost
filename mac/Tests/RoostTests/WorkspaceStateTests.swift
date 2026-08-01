// WorkspaceStateTests — M4a of the daemon-removal refactor.
//
// Cover the Workspace's in-memory state machine + state.json
// persistence: project/tab CRUD, reorder, cascade-delete,
// atomic-write durability, corrupt-file fallback, id counter
// persistence.

import Foundation
import Testing

@testable import Roost

@Suite("Workspace state machine")
struct WorkspaceStateTests {
    @Test func createsAndListsProjects() async {
        let ws = await Workspace()
        let a = await ws.createProject(name: "alpha", cwd: "/a")
        let b = await ws.createProject(name: "beta", cwd: "/b")
        let snap = await ws.snapshot()
        #expect(snap.map(\.id) == [a.id, b.id])
        #expect(snap.map(\.name) == ["alpha", "beta"])
        #expect(b.id > a.id)
    }

    @Test func openTabFlipsActiveSelection() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/", title: "")
        let active = await (ws.activeProjectID, ws.activeTabID)
        #expect(active.0 == p.id)
        #expect(active.1 == t.id)
    }

    @Test func closeTabFallsBackToSibling() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let t1 = try await ws.openTab(projectID: p.id, cwd: "/", title: "one")
        let t2 = try await ws.openTab(projectID: p.id, cwd: "/", title: "two")
        // t2 is active because openTab sets it.
        let activeBefore = await ws.activeTabID
        #expect(activeBefore == t2.id)
        try await ws.closeTab(t2.id)
        let activeAfter = await ws.activeTabID
        #expect(activeAfter == t1.id)
    }

    @Test func deleteProjectCascadesTabs() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        _ = try await ws.openTab(projectID: p.id, cwd: "/", title: "one")
        _ = try await ws.openTab(projectID: p.id, cwd: "/", title: "two")
        let cascaded = try await ws.deleteProject(p.id)
        #expect(cascaded.count == 2)
        let snap = await ws.snapshot()
        #expect(snap.isEmpty)
    }

    @Test func closeLastTabDeletesProject() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/", title: "only")
        let captured = EventCapture()
        await ws.subscribe { event in captured.append(label(for: event)) }
        try await ws.closeTab(t.id)
        // The only project is gone with its last tab.
        let snap = await ws.snapshot()
        #expect(snap.isEmpty)
        // Event order: tabClosed → projectDeleted → activeChanged.
        #expect(captured.snapshot() == ["tabClosed", "projectDeleted", "activeChanged"])
        let active = await (ws.activeProjectID, ws.activeTabID)
        #expect(active.0 == 0)
        #expect(active.1 == 0)
    }

    @Test func closeLastTabOfInactiveProjectKeepsActive() async throws {
        // Closing a non-active project's last tab deletes that project
        // but must not steal the active selection from elsewhere.
        let ws = await Workspace()
        let a = await ws.createProject(name: "a", cwd: "/")
        let aTab = try await ws.openTab(projectID: a.id, cwd: "/", title: "a1")
        let b = await ws.createProject(name: "b", cwd: "/")
        let bTab = try await ws.openTab(projectID: b.id, cwd: "/", title: "b1")
        // Make project A active, then close project B's last tab.
        _ = try await ws.focusTab(aTab.id)
        try await ws.closeTab(bTab.id)
        let snap = await ws.snapshot()
        #expect(snap.count == 1)
        #expect(snap.first?.id == a.id)
        // Active stays on A; no spurious reassignment.
        let active = await (ws.activeProjectID, ws.activeTabID)
        #expect(active.0 == a.id)
        #expect(active.1 == aTab.id)
    }

    @Test func setTabTitleLocksAgainstOSC() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/", title: "")
        try await ws.setTabTitle(t.id, title: "manual")
        try await ws.setTabTitleFromOSC(t.id, title: "shell-says")
        let after = await ws.tab(t.id)
        #expect(after?.title == "manual")
        #expect(after?.userTitled == true)
    }

    /// Issue #196: `setTabCwd` re-derives the tab title from cwd
    /// when `!userTitled`, so the title follows cwd on any shell
    /// (Apple bash 3.2 / `--norc` bash / etc.), not just shells with
    /// the OSC 0 integration loaded. Events fire cwd-then-title.
    @Test func setTabCwdReDerivesTitleWhenNotUserTitled() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/tmp", title: "")
        #expect(await ws.tab(t.id)?.title == "tmp")
        let captured = EventCapture()
        await ws.subscribe { captured.append(label(for: $0)) }
        try await ws.setTabCwd(t.id, cwd: "/usr")
        let labels = captured.snapshot()
        // Cwd-then-title: cause-then-effect.
        #expect(labels == ["tabCwdChanged", "tabTitleChanged"])
        #expect(await ws.tab(t.id)?.title == "usr")
    }

    /// `setTabCwd` does NOT touch the title when the user has
    /// manually renamed (mirrors `setTabTitleFromOSC`'s gate).
    @Test func setTabCwdPreservesUserTitledTitle() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/tmp", title: "")
        try await ws.setTabTitle(t.id, title: "manual")
        let captured = EventCapture()
        await ws.subscribe { captured.append(label(for: $0)) }
        try await ws.setTabCwd(t.id, cwd: "/usr")
        // No tabTitleChanged — userTitled blocks the re-derivation.
        #expect(captured.snapshot() == ["tabCwdChanged"])
        #expect(await ws.tab(t.id)?.title == "manual")
    }

    /// `cd .` (cwd unchanged in basename) doesn't churn a redundant
    /// tabTitleChanged. Guards against per-prompt event spam from
    /// a shell that re-emits the same OSC 7 every prompt.
    @Test func setTabCwdSkipsTitleEventWhenBasenameUnchanged() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/tmp", title: "")
        let captured = EventCapture()
        await ws.subscribe { captured.append(label(for: $0)) }
        try await ws.setTabCwd(t.id, cwd: "/tmp")
        // Cwd event still fires (model writes through); title
        // suppressed since basename didn't change.
        #expect(captured.snapshot() == ["tabCwdChanged"])
    }

    /// CLI / IPC `tab.open` callers can pass an explicit placeholder
    /// title (`"roostctl"`, `"Tab 1"`, …). `openTab` leaves
    /// `userTitled=false` on those (the supplied title is treated
    /// as a placeholder per the openTab comment). The model fix
    /// overwrites the placeholder on the first cwd change.
    /// Guards: a future refactor that flips `openTab` to
    /// `userTitled = !title.isEmpty` silently inverts the model
    /// invariant — this test catches it.
    @Test func setTabCwdOverwritesPlaceholderTitle() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/tmp", title: "roostctl")
        #expect(await ws.tab(t.id)?.title == "roostctl")
        #expect(await ws.tab(t.id)?.userTitled == false)
        try await ws.setTabCwd(t.id, cwd: "/usr")
        #expect(await ws.tab(t.id)?.title == "usr")
    }

    /// Cross-platform parity: opening a tab at cwd `/` derives the
    /// title to `"/"`, matching the Rust twin's `derive_title("/")`
    /// (special-cased there because `Path::file_name()` returns
    /// `None`). Swift's `(cwd as NSString).lastPathComponent` already
    /// returns `"/"` for the root, so this is a regression lock
    /// rather than a code change. Since `deriveTitle` is private,
    /// route the assertion through `openTab`.
    @Test func openTabAtRootDerivesTitleSlash() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/", title: "")
        #expect(await ws.tab(t.id)?.title == "/")
    }

    @Test func reorderTabsPartialKeepsUnlisted() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let a = try await ws.openTab(projectID: p.id, cwd: "/", title: "a")
        let b = try await ws.openTab(projectID: p.id, cwd: "/", title: "b")
        let c = try await ws.openTab(projectID: p.id, cwd: "/", title: "c")
        try await ws.reorderTabs(projectID: p.id, tabIDs: [c.id, a.id])
        let order = await ws.tabs(in: p.id).map(\.id)
        #expect(order == [c.id, a.id, b.id])
    }

    /// Ported from the Rust twin `position_is_max_plus_one_after_delete`
    /// (`crates/roost-engine/src/workspace.rs`). Swift allocated from
    /// `count` until #262, which collides after a mid-list delete
    /// because positions are sparse, not dense (#80).
    @Test func positionIsMaxPlusOneAfterDelete() async throws {
        let ws = await Workspace()
        let a = await ws.createProject(name: "a", cwd: "")
        let b = await ws.createProject(name: "b", cwd: "")
        let c = await ws.createProject(name: "c", cwd: "")
        #expect([a.position, b.position, c.position] == [0, 1, 2])

        // Delete the middle project, then create a new one. The old
        // `count` rule would reuse position 2 (colliding with c); the
        // fix must hand out max(0, 2) + 1 = 3.
        _ = try await ws.deleteProject(b.id)
        let d = await ws.createProject(name: "d", cwd: "")
        #expect(d.position == 3, "new project position collided after delete")

        // Same invariant for tabs within a project.
        let t0 = try await ws.openTab(projectID: a.id, cwd: "/", title: "t0")
        let t1 = try await ws.openTab(projectID: a.id, cwd: "/", title: "t1")
        #expect([t0.position, t1.position] == [0, 1])
        try await ws.closeTab(t0.id)
        let t2 = try await ws.openTab(projectID: a.id, cwd: "/", title: "t2")
        #expect(t2.position == 2, "new tab position collided after close")
        #expect(await ws.tabs(in: a.id).map(\.id) == [t1.id, t2.id])
    }

    /// The user-visible symptom (#262): closing from the *front* drops
    /// `count` below every surviving position, so the pre-fix rule
    /// handed a brand-new tab a position the strip renders earlier — a
    /// new tab appearing to the left of an older one. No collision is
    /// even required; `count < max + 1` is enough.
    @Test func tabOpenedAfterFrontCloseSortsLast() async throws {
        let ws = await Workspace()
        let p = await ws.createProject(name: "p", cwd: "/")
        let a = try await ws.openTab(projectID: p.id, cwd: "/", title: "a")
        let b = try await ws.openTab(projectID: p.id, cwd: "/", title: "b")
        let c = try await ws.openTab(projectID: p.id, cwd: "/", title: "c")
        try await ws.closeTab(a.id)
        try await ws.closeTab(b.id)
        let d = try await ws.openTab(projectID: p.id, cwd: "/", title: "d")
        #expect(d.position > c.position)
        #expect(
            await ws.tabs(in: p.id).map(\.id) == [c.id, d.id],
            "a newly-opened tab must sort after every survivor"
        )
    }

    @Test func eventsFireOnMutation() async {
        let ws = await Workspace()
        let captured = EventCapture()
        await ws.subscribe { event in
            captured.append(label(for: event))
        }
        let p = await ws.createProject(name: "p", cwd: "/")
        _ = try? await ws.openTab(projectID: p.id, cwd: "/", title: "")
        let labels = captured.snapshot()
        #expect(labels.contains("projectCreated"))
        #expect(labels.contains("tabOpened"))
        #expect(labels.contains("activeChanged"))
    }
}

/// Concurrency-safe label sink for the events test. Swift 6 strict
/// sendable rejects `inout` captures into a `@Sendable` closure.
private final class EventCapture: @unchecked Sendable {
    private let lock = NSLock()
    private var labels: [String] = []
    func append(_ label: String) {
        lock.lock()
        labels.append(label)
        lock.unlock()
    }
    func snapshot() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return labels
    }
}

private func label(for event: Workspace.Event) -> String {
    switch event {
    case .projectCreated: return "projectCreated"
    case .tabOpened: return "tabOpened"
    case .activeChanged: return "activeChanged"
    case .projectRenamed: return "projectRenamed"
    case .projectDeleted: return "projectDeleted"
    case .tabClosed: return "tabClosed"
    case .tabStateChanged: return "tabStateChanged"
    case .tabTitleChanged: return "tabTitleChanged"
    case .tabCwdChanged: return "tabCwdChanged"
    case .tabNotification: return "tabNotification"
    case .hookActiveChanged: return "hookActiveChanged"
    case .agentChanged: return "agentChanged"
    case .notificationFired: return "notificationFired"
    case .tabsReordered: return "tabsReordered"
    case .projectsReordered: return "projectsReordered"
    }
}

// Agent state model (plan 002). Mirrors the Rust workspace suite in
// `crates/roost-engine/src/workspace.rs` case for case, so the two
// implementations of the same op set can't drift.
@MainActor
@Suite("Workspace agent state model")
struct WorkspaceAgentStateTests {
    /// A one-tab workspace with the window reported **unfocused**, so
    /// these tests exercise the agent axis without policy §3.5's focus
    /// suppression in the way. `attentionPolicy*` covers the matrix.
    private func agentWorkspace() throws -> (Workspace, Int64) {
        let ws = Workspace()
        let p = ws.createProject(name: "p", cwd: "")
        let t = try ws.openTab(projectID: p.id, cwd: "/", title: "")
        ws.setWindowFocused(false)
        return (ws, t.id)
    }

    /// A workspace whose one tab is already claimed by `claude`/`s1`.
    private func ownedWorkspace(_ lifecycle: AgentLifecycle) throws -> (Workspace, Int64) {
        let (ws, tid) = try agentWorkspace()
        try ws.agentReport(
            report(tid, "claude", "s1", .claim, lifecycle)
        )
        return (ws, tid)
    }

    private func report(
        _ tabID: Int64,
        _ source: String,
        _ session: String,
        _ action: OwnershipAction,
        _ lifecycle: AgentLifecycle? = nil
    ) -> AgentReport {
        AgentReport(
            tabID: tabID,
            source: source,
            sessionID: session,
            ownershipAction: action,
            lifecycle: lifecycle
        )
    }

    /// A report that sets attention.
    private func attentionReport(_ tabID: Int64) -> AgentReport {
        var r = report(tabID, "claude", "s1", .claim)
        r.attention = .set
        r.title = "Claude Code"
        r.body = "Turn complete"
        return r
    }

    /// Replaces `setTabStateFromOSCRespectsHookActive`: the gate it
    /// pinned is gone. OSC 133 now writes the shell axis unconditionally,
    /// and derivation — not a suppression rule — decides which axis the
    /// tab shows.
    @Test func shellMarksWriteTheShellAxisUnderAgentOwnership() throws {
        let (ws, tid) = try ownedWorkspace(.waiting)
        try ws.applyShellMark(tid, body: "C")
        let tab = try #require(ws.tab(tid))
        #expect(tab.agent.shell == .foregroundProcess)
        #expect(tab.agent.lifecycle == .waiting)
        #expect(tab.state == .needsInput)
    }

    @Test func shellMarksDriveTheStateWithoutAnOwner() throws {
        let (ws, tid) = try agentWorkspace()
        try ws.applyShellMark(tid, body: "C")
        #expect(ws.tab(tid)?.state == .running)
        try ws.applyShellMark(tid, body: "D;0")
        #expect(ws.tab(tid)?.state == Workspace.TabState.none)
        // Undefined mark bodies are no-change.
        try ws.applyShellMark(tid, body: "Z")
        #expect(ws.tab(tid)?.state == Workspace.TabState.none)
    }

    /// The `D`/`A` failsafe end to end: a killed agent's ownership
    /// survives as a label but stops driving derivation, and raw OSC
    /// re-opens.
    @Test func promptMarkReopensRawOscForADeadAgent() throws {
        let (ws, tid) = try ownedWorkspace(.working)
        #expect(try !ws.raiseAttention(tid, title: "Wrapper", body: "noise", source: .rawOsc))
        #expect(ws.tab(tid)?.hasNotification == false)

        try ws.applyShellMark(tid, body: "D;0")
        let tab = try #require(ws.tab(tid))
        #expect(tab.agent.lifecycle == .inactive)
        #expect(tab.hookActive, "ownership survives as a label")
        #expect(tab.state == Workspace.TabState.none, "falls through to shell")
        #expect(try ws.raiseAttention(tid, title: "Build", body: "done", source: .rawOsc))
        #expect(ws.tab(tid)?.hasNotification == true)
    }

    /// Session scoping is enforced in the workspace, alongside the
    /// mutation (plan §3.3). A report from a different session is
    /// dropped whole — lifecycle *and* attention.
    @Test func reportFromAForeignSessionIsDropped() throws {
        let (ws, tid) = try ownedWorkspace(.working)

        var stale = report(tid, "claude", "s2", .preserve, .finished)
        stale.attention = .set
        stale.title = "Claude Code"
        stale.body = "Turn complete"
        let (accepted, tab) = try ws.agentReport(stale)
        #expect(!accepted)
        #expect(tab.agent.lifecycle == .working)
        #expect(!tab.hasNotification, "a dropped report fires nothing")

        // Same session id, different source: still a mismatch.
        let (codexAccepted, _) = try ws.agentReport(
            report(tid, "codex", "s1", .preserve, .finished)
        )
        #expect(!codexAccepted)
    }

    @Test func claimSupersedesALiveOwnerAndReleaseIsScoped() throws {
        let (ws, tid) = try ownedWorkspace(.working)

        let (claimed, superseded) = try ws.agentReport(
            report(tid, "codex", "s9", .claim, .waiting)
        )
        #expect(claimed, "claim is the supersede path")
        #expect(superseded.agent.ownership?.source == "codex")

        // The displaced owner can no longer release.
        let (staleRelease, stillOwned) = try ws.agentReport(
            report(tid, "claude", "s1", .release)
        )
        #expect(!staleRelease)
        #expect(stillOwned.hookActive)

        let (released, free) = try ws.agentReport(report(tid, "codex", "s9", .release))
        #expect(released)
        #expect(!free.hookActive)
        #expect(free.agent.lifecycle == .inactive)
    }

    /// Attention effects are the workspace's to apply: `set` raises the
    /// pending flag and fires, `clear` drops it.
    @Test func attentionSetAndClearDriveTheNotificationFlag() throws {
        let (ws, tid) = try agentWorkspace()
        var claim = attentionReport(tid)
        claim.body = "Needs your permission"
        let (_, notified) = try ws.agentReport(claim)
        #expect(notified.hasNotification)

        var clear = report(tid, "claude", "s1", .preserve)
        clear.attention = .clear
        let (_, cleared) = try ws.agentReport(clear)
        #expect(!cleared.hasNotification)
    }

    // Attention policy B (plan §3.5) — one predicate, applied once.

    /// The tab you are looking at is the tab you have seen: a structured
    /// notification for it is dropped whole — no pending bit, no events.
    /// Emitting nothing is what makes "switch away afterwards and the
    /// badge does not appear" true by construction.
    @Test func attentionPolicyDropsANotificationForTheFocusedActiveTab() throws {
        let (ws, tid) = try agentWorkspace()
        ws.setWindowFocused(true)
        let captured = EventCapture()
        ws.subscribe { captured.append(label(for: $0)) }

        let (accepted, tab) = try ws.agentReport(attentionReport(tid))
        #expect(accepted, "the report itself still applies")
        #expect(!tab.hasNotification)
        #expect(!captured.snapshot().contains("notificationFired"))
        #expect(!captured.snapshot().contains("tabNotification"))

        // Switching away must not resurrect it.
        let projectID = try #require(ws.tab(tid)?.projectId)
        let other = try ws.openTab(projectID: projectID, cwd: "/", title: "")
        #expect(other.id != tid)
        #expect(ws.tab(tid)?.hasNotification == false)
    }

    @Test func attentionPolicyDeliversWhenTheWindowIsUnfocused() throws {
        let (ws, tid) = try agentWorkspace()
        ws.setWindowFocused(false)
        let captured = EventCapture()
        ws.subscribe { captured.append(label(for: $0)) }

        let (_, tab) = try ws.agentReport(attentionReport(tid))
        #expect(tab.hasNotification)
        #expect(captured.snapshot().contains("notificationFired"))
    }

    @Test func attentionPolicyDeliversWhenAnotherTabIsActive() throws {
        let (ws, tid) = try agentWorkspace()
        let projectID = try #require(ws.tab(tid)?.projectId)
        let other = try ws.openTab(projectID: projectID, cwd: "/", title: "")
        _ = try ws.focusTab(other.id)
        ws.setWindowFocused(true)
        let captured = EventCapture()
        ws.subscribe { captured.append(label(for: $0)) }

        let (_, tab) = try ws.agentReport(attentionReport(tid))
        #expect(tab.hasNotification)
        #expect(captured.snapshot().contains("notificationFired"))
    }

    /// Structured attention is NEVER gated on agent ownership (plan
    /// §3.4) — `notification.create` / `roostctl notify` must get through
    /// even mid-turn, since that is how the agent itself speaks.
    @Test func structuredAttentionIsNeverGatedByALiveAgent() throws {
        let (ws, tid) = try ownedWorkspace(.working)

        #expect(try ws.raiseAttention(tid, title: "Roost", body: "explicit", source: .structured))
        #expect(ws.tab(tid)?.hasNotification == true)

        // …and the same is true through the report path.
        try ws.setTabHasNotification(tid, hasPending: false)
        var r = attentionReport(tid)
        r.ownershipAction = .preserve
        let (accepted, tab) = try ws.agentReport(r)
        #expect(accepted)
        #expect(tab.hasNotification)
    }

    @Test func raiseAttentionOnAMissingTabIsAnError() {
        let ws = Workspace()
        #expect(throws: Workspace.WorkspaceError.self) {
            try ws.raiseAttention(999, title: "t", body: "b", source: .structured)
        }
    }

    /// Focus defaults to *focused* before any UI reports it, so a
    /// headless / IPC-only workspace routes exactly as a real window
    /// would. The active tab is therefore suppressed, and — the half
    /// that keeps the default safe — an inactive tab still delivers.
    @Test func windowFocusDefaultsToFocused() throws {
        let ws = Workspace()
        let p = ws.createProject(name: "p", cwd: "")
        let active = try ws.openTab(projectID: p.id, cwd: "/", title: "a")
        let background = try ws.openTab(projectID: p.id, cwd: "/", title: "b")
        _ = try ws.focusTab(active.id)

        #expect(try !ws.raiseAttention(active.id, title: "t", body: "b", source: .structured))
        #expect(try ws.raiseAttention(background.id, title: "t", body: "b", source: .structured))
        #expect(ws.tab(background.id)?.hasNotification == true)
    }

    /// Plan §3.7's transition table. `running`/`needs_input`/`idle`
    /// produce their pre-change `tab.state`; `none` is the one genuine
    /// behavior change — it releases, so the shell axis shows through.
    @Test func legacySetStateFollowsTheTransitionTable() throws {
        let (ws, tid) = try agentWorkspace()
        for (legacy, lifecycle) in [
            (Workspace.TabState.running, AgentLifecycle.working),
            (Workspace.TabState.needsInput, AgentLifecycle.waiting),
            (Workspace.TabState.idle, AgentLifecycle.finished),
        ] {
            try ws.setTabState(tid, state: legacy)
            let tab = try #require(ws.tab(tid))
            #expect(tab.state == legacy, "legacy projection must not move")
            #expect(tab.agent.lifecycle == lifecycle)
            #expect(tab.agent.ownership?.source == Workspace.manualSource)
            #expect(tab.agent.ownership?.sessionID == "")
        }

        // `none` releases; with a live foreground process the shell axis
        // now shows `running` rather than `none`.
        try ws.applyShellMark(tid, body: "C")
        try ws.setTabState(tid, state: Workspace.TabState.none)
        let tab = try #require(ws.tab(tid))
        #expect(!tab.hookActive, "set-state none releases ownership")
        #expect(tab.agent.ownership == nil)
        #expect(tab.agent.lifecycle == .inactive)
        #expect(tab.state == .running, "shell-derived")
    }

    /// A manual override takes the tab from a live agent — the user has
    /// the wheel — and `none` releases even though the release itself is
    /// scoped to `manual`.
    @Test func manualOverrideSupersedesALiveAgent() throws {
        let (ws, tid) = try ownedWorkspace(.working)

        try ws.setTabState(tid, state: .needsInput)
        #expect(ws.tab(tid)?.agent.ownership?.source == Workspace.manualSource)

        // Claude's next in-session event is now out of scope.
        let (accepted, _) = try ws.agentReport(report(tid, "claude", "s1", .preserve, .finished))
        #expect(!accepted)

        // And `none` still releases, despite `claude` having held it.
        try ws.setTabState(tid, state: Workspace.TabState.none)
        #expect(ws.tab(tid)?.agent.ownership == nil)
    }

    @Test func setHookActiveClaimsAndReleasesAsLegacy() throws {
        let (ws, tid) = try agentWorkspace()
        try ws.setTabHookActive(tid, active: true)
        let claimed = try #require(ws.tab(tid))
        #expect(claimed.hookActive)
        #expect(claimed.agent.ownership?.source == Workspace.legacySource)
        #expect(
            claimed.state == Workspace.TabState.none,
            "ownership alone doesn't move the legacy state"
        )

        try ws.setTabHookActive(tid, active: false)
        #expect(ws.tab(tid)?.hookActive == false)
    }

    @Test func ptyReplacementClearsOwnership() throws {
        let (ws, tid) = try ownedWorkspace(.working)
        try ws.applyShellMark(tid, body: "C")

        try ws.ptyReplaced(tid)
        let tab = try #require(ws.tab(tid))
        #expect(tab.agent.ownership == nil)
        #expect(tab.agent.lifecycle == .inactive)
        #expect(tab.agent.shell == .unknown)
        #expect(tab.state == Workspace.TabState.none)
    }

    /// The derived slices ride along with the full record, so the UI and
    /// any external subscriber see one consistent story.
    @Test func acceptedReportEmitsStateHookAndAgentEvents() throws {
        let (ws, tid) = try agentWorkspace()
        let captured = EventCapture()
        ws.subscribe { captured.append(label(for: $0)) }
        try ws.agentReport(report(tid, "claude", "s1", .claim, .waiting))
        #expect(captured.snapshot() == ["tabStateChanged", "hookActiveChanged", "agentChanged"])

        // A report that changes nothing emits nothing — repeated prompt
        // marks are the common case and must not churn the UI.
        try ws.applyShellMark(tid, body: "Z")
        #expect(captured.snapshot().count == 3)
    }

    /// The converse of a dropped report (which succeeds with
    /// `accepted: false`): a tab that doesn't exist is an error.
    @Test func agentReportOnAMissingTabIsAnError() {
        let ws = Workspace()
        #expect(throws: Workspace.WorkspaceError.self) {
            try ws.agentReport(self.report(999, "claude", "s1", .claim))
        }
    }
}

@Suite("Workspace state.json persistence")
struct WorkspaceStatePersistenceTests {
    private func tempPath() -> String {
        let dir = NSTemporaryDirectory()
        let name = "roost-test-\(UUID().uuidString).json"
        return (dir as NSString).appendingPathComponent(name)
    }

    @Test func projectsAndNextIDSurviveReopen() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }

        let (projectID, firstTabID): (Int64, Int64) = try await {
            let ws = await Workspace(statePath: path)
            let p = await ws.createProject(name: "Roost", cwd: "/tmp")
            let t = try await ws.openTab(projectID: p.id, cwd: "/tmp", title: "shell")
            return (p.id, t.id)
        }()

        let ws2 = await Workspace(statePath: path)
        let projects = await ws2.snapshot()
        #expect(projects.count == 1)
        let p = try #require(projects.first)
        #expect(p.id == projectID)
        #expect(p.name == "Roost")
        #expect(p.cwd == "/tmp")
        // Tabs come back as restore *descriptors*, not live tabs —
        // the live `tabs` map is empty until the UI re-opens them.
        let tabsInProject = await ws2.tabs(in: p.id)
        #expect(tabsInProject.isEmpty)
        // Ids advance past the previously-issued tab id.
        let nextTab = try await ws2.openTab(projectID: projectID, cwd: "/", title: "")
        #expect(nextTab.id > firstTabID)
    }

    @Test func persistRestoreRoundTripsTabLayout() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }

        let projectID: Int64 = try await {
            let ws = await Workspace(statePath: path)
            let p = await ws.createProject(name: "p", cwd: "/proj")
            _ = try await ws.openTab(projectID: p.id, cwd: "/a", title: "atab")
            let b = try await ws.openTab(projectID: p.id, cwd: "/b", title: "btab")
            _ = try await ws.openTab(projectID: p.id, cwd: "/c", title: "ctab")
            // Select the middle tab so restore picks it by position.
            _ = try await ws.focusTab(b.id)
            return p.id
        }()

        let ws2 = await Workspace(statePath: path)
        // Restored tabs are descriptors, not live tabs.
        let live = await ws2.tabs(in: projectID)
        #expect(live.isEmpty)
        let restore = try #require(await ws2.takeRestoreLayout())
        #expect(restore.activeProjectID == projectID)
        #expect(restore.activeTabPosition == 1, "tab 'b' is at position 1")
        let rp = try #require(restore.projects.first { $0.projectID == projectID })
        #expect(rp.tabs.map(\.cwd) == ["/a", "/b", "/c"])
        #expect(rp.tabs[1].title == "btab")
        // `takeRestoreLayout` is one-shot.
        let again = await ws2.takeRestoreLayout()
        #expect(again == nil)
    }

    /// Tied persisted tab positions are reachable — the `state.json`
    /// captured for #262 holds `[2, 3, 3]` — and the restore layout is
    /// the order the bootstrap re-opens tabs in, so the tie decides the
    /// restored tab strip. `sorted(by:)` is documented as **not** stable
    /// while Rust's `sort_by_key` is stable by contract, so without the
    /// index tiebreak the same file could restore in a different order on
    /// Mac than on Linux. The twin is
    /// `restore_layout_breaks_tied_tab_positions_by_file_order`; the
    /// fixture's file order is neither ascending nor descending in
    /// `position`, so a sort that ignored the index could not pass by luck.
    @Test func restoreLayoutBreaksTiedTabPositionsByFileOrder() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try """
        { "next_id": 200, "active_project_id": 100,
          "projects": [
            { "id": 100, "name": "p", "cwd": "/tmp", "position": 0, "created_at": 100,
              "tabs": [
                { "title": "a", "cwd": "/a", "position": 3, "user_titled": false },
                { "title": "b", "cwd": "/b", "position": 1, "user_titled": false },
                { "title": "c", "cwd": "/c", "position": 3, "user_titled": false }
              ] }
          ] }
        """.write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        let restore = try #require(await ws.takeRestoreLayout())
        #expect(restore.projects.first?.tabs.map(\.title) == ["b", "a", "c"])
    }

    /// Issue #196 follow-up: `userTitled` is persisted across
    /// relaunch so a manually-renamed tab keeps its rename — and so
    /// the model's `setTabCwd` re-derivation (also #196) doesn't
    /// silently clobber it on the first post-relaunch `cd`.
    @Test func userTitledPersistsAcrossRelaunch() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }

        try await {
            let ws = await Workspace(statePath: path)
            let p = await ws.createProject(name: "p", cwd: "/")
            let manual = try await ws.openTab(projectID: p.id, cwd: "/tmp", title: "")
            let placeholder = try await ws.openTab(projectID: p.id, cwd: "/tmp", title: "roostctl")
            try await ws.setTabTitle(manual.id, title: "docs")
            #expect(await ws.tab(manual.id)?.userTitled == true)
            #expect(await ws.tab(placeholder.id)?.userTitled == false)
        }()

        let ws2 = await Workspace(statePath: path)
        let restore = try #require(await ws2.takeRestoreLayout())
        let rp = try #require(restore.projects.first)
        #expect(rp.tabs.count == 2)
        #expect(rp.tabs[0].title == "docs")
        #expect(rp.tabs[0].userTitled, "manual rename keeps userTitled")
        #expect(rp.tabs[1].title == "roostctl")
        #expect(!rp.tabs[1].userTitled, "placeholder title is not userTitled")
    }

    /// A state.json written by a build predating `user_titled`
    /// persistence has no `user_titled` key per tab. Must load with
    /// the field defaulted to `false` (matches the prior implicit
    /// "always not user-titled" behavior).
    @Test func legacyTabWithoutUserTitledDefaultsToFalse() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        let legacy = """
        {
            "next_id": 5,
            "projects": [{
                "id": 1, "name": "Old", "cwd": "/tmp",
                "position": 0, "created_at": 1,
                "tabs": [{ "title": "docs", "cwd": "/usr", "position": 0 }]
            }]
        }
        """
        try legacy.write(toFile: path, atomically: true, encoding: .utf8)
        let ws = await Workspace(statePath: path)
        let restore = try #require(await ws.takeRestoreLayout())
        let tab = try #require(restore.projects.first?.tabs.first)
        #expect(tab.title == "docs")
        #expect(tab.cwd == "/usr")
        #expect(!tab.userTitled, "missing user_titled key must default to false")
    }

    @Test func legacyStateWithoutTabsLoadsWithDefaults() async {
        // A state.json written by a build predating tab persistence
        // (no `tabs` / `active_*` keys) must still load — those fields
        // default to empty / 0 rather than failing to decode.
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        let legacy = """
        {"next_id":5,"projects":[{"id":1,"name":"Old","cwd":"/tmp","position":0,"created_at":1}]}
        """
        try? legacy.write(toFile: path, atomically: true, encoding: .utf8)
        let ws = await Workspace(statePath: path)
        let projects = await ws.snapshot()
        #expect(projects.count == 1)
        let restore = await ws.takeRestoreLayout()
        #expect(restore?.activeProjectID == 0)
        #expect(restore?.projects.first?.tabs.isEmpty == true)
    }

    @Test func corruptedStateStartsEmpty() async {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try? "not json".write(toFile: path, atomically: true, encoding: .utf8)
        let ws = await Workspace(statePath: path)
        let snap = await ws.snapshot()
        #expect(snap.isEmpty, "corrupt state must start empty")
    }

    @Test func atomicWriteLeavesBackup() async throws {
        let path = tempPath()
        defer {
            try? FileManager.default.removeItem(atPath: path)
            try? FileManager.default.removeItem(atPath: path + ".bak")
        }
        let ws = await Workspace(statePath: path)
        _ = await ws.createProject(name: "first", cwd: "/")
        _ = await ws.createProject(name: "second", cwd: "/")
        // A .bak should exist now with the first-write state.
        #expect(FileManager.default.fileExists(atPath: path + ".bak"))
        let bakData = try Data(contentsOf: URL(fileURLWithPath: path + ".bak"))
        #expect(String(data: bakData, encoding: .utf8)?.contains("first") == true)
    }

    @Test func cwdChangesWriteThrough() async throws {
        // No throttle: every setTabCwd writes through, so a reopen sees
        // the LATEST cwd (last write wins), not a coalesced earlier one.
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }

        let ws = await Workspace(statePath: path)
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/start", title: "")
        try await ws.setTabCwd(t.id, cwd: "/first")
        try await ws.setTabCwd(t.id, cwd: "/second")

        let ws2 = await Workspace(statePath: path)
        let restore = try #require(await ws2.takeRestoreLayout())
        #expect(
            restore.projects.first?.tabs.first?.cwd == "/second",
            "the latest cwd must reach disk (write-through, no throttle)"
        )
    }

    @Test func flushFreezesFurtherPersistence() async throws {
        // flush() writes the current layout (with fsync) and then
        // freezes: a subsequent mutation must NOT reach disk, so a
        // teardown cascade can't clobber the flushed layout.
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }

        let ws = await Workspace(statePath: path)
        let p = await ws.createProject(name: "p", cwd: "/")
        let t = try await ws.openTab(projectID: p.id, cwd: "/flushed", title: "")
        await ws.flush()
        // Frozen — this write must be a no-op.
        try await ws.setTabCwd(t.id, cwd: "/after-flush")

        let ws2 = await Workspace(statePath: path)
        let restore = try #require(await ws2.takeRestoreLayout())
        #expect(
            restore.projects.first?.tabs.first?.cwd == "/flushed",
            "a post-flush mutation must not have reached disk"
        )
    }

    /// A `state.json` that still restores with tied project positions.
    ///
    /// `createProject` hands out `max + 1` and the load path now
    /// repairs colliding positions, so the **clamp is the last seam**
    /// that can produce a project tie: both `afterMax` and the load
    /// normalizer saturate at `Int32.max` rather than trapping, and a
    /// saturated `previous + 1` cannot outrun a sibling already
    /// sitting there. p1 alone at 0, p2…p5 tied at `Int32.max` — the
    /// shape both callers assert.
    private func tiedProjectsState() -> String {
        projectsState(
            (1...5).map { (id: Int64($0), name: "p\($0)", position: $0 == 1 ? 0 : Int32.max) }
        )
    }

    /// A `state.json` holding `rows` as tabless projects in `/tmp`, in
    /// the given order. The mirror of the Rust tests' `project_json`.
    private func projectsState(
        _ rows: [(id: Int64, name: String, position: Int32)],
        activeProjectID: Int64 = 0
    ) -> String {
        let projects = rows.map { row in
            """
            { "id": \(row.id), "name": "\(row.name)", "cwd": "/tmp",
              "position": \(row.position), "created_at": \(row.id), "tabs": [] }
            """
        }
        return """
        { "next_id": 200, "active_project_id": \(activeProjectID),
          "projects": [\(projects.joined(separator: ","))] }
        """
    }

    private func decodeSnapshot(at path: String) throws -> Workspace.SnapshotFile {
        let data = try Data(contentsOf: URL(fileURLWithPath: path))
        return try JSONDecoder().decode(Workspace.SnapshotFile.self, from: data)
    }

    /// `reorderProjects` sorts the *unlisted* rows out of
    /// `Dictionary.values` — unspecified order, seeded per process —
    /// and then **assigns** `position = next++` from that order. Without
    /// the id tiebreak a tie scrambles the sidebar permanently, and
    /// differently on every launch.
    @Test func reorderProjectsBreaksPositionTiesById() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try tiedProjectsState().write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        #expect(
            await ws.snapshot().map(\.position) == [0, .max, .max, .max, .max],
            "the tie must survive the load — otherwise this tests nothing"
        )
        // Partial reorder: only project 1 is listed, so 2…5 trail it in
        // `(position, id)` order and are renumbered from that order.
        try await ws.reorderProjects([1])
        #expect(await ws.snapshot().map(\.id) == [1, 2, 3, 4, 5])
        #expect(await ws.snapshot().map(\.position) == [0, 1, 2, 3, 4])
    }

    /// `persist()` must write each project in the order the sidebar
    /// shows it, ties included — a nondeterministic file order means a
    /// relaunch can swap two rows relative to what was on screen.
    ///
    /// Asserted against literals rather than against `ws.snapshot()`:
    /// the snapshot is produced by the very `(position, id)` comparator
    /// this test exists to pin, so comparing the two would only prove
    /// that persist and snapshot agree — including on being wrong
    /// together.
    @Test func persistWritesTiedProjectsInDisplayOrder() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try tiedProjectsState().write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        // Any mutation persists; renaming leaves every position alone.
        try await ws.renameProject(1, name: "renamed")
        let written = try decodeSnapshot(at: path)
        #expect(written.projects.map(\.position) == [0, .max, .max, .max, .max], "tie preserved")
        #expect(written.projects.map(\.id) == [1, 2, 3, 4, 5], "ties break by ascending id")
        #expect(written.projects.map(\.name) == ["renamed", "p2", "p3", "p4", "p5"])
    }

    /// Regression test for the allocator's sparse-position output
    /// (`max + 1` after a mid-project close), not for the `(position,
    /// id)` id tiebreak. It fails against pre-fix code: the old
    /// allocator sized a new position off the project's *live* tab
    /// count, which shrinks on close, so the tab opened after the two
    /// closes below got a position lower than the still-open `c` and
    /// sorted ahead of it — this test's positions/cwd-order assertions
    /// catch that. It is NOT a tiebreak test: no seam can create a live
    /// tab-position tie in Swift post-fix (`openTab` is the sole
    /// allocator; restored tabs come back as descriptors, never live
    /// rows), so there is nothing here to tie-break. The tiebreak rule
    /// itself is stated by the Rust twin,
    /// `persist_sorts_tabs_by_position_then_id` — see its own comment:
    /// that one can't fail either, for a different, provable reason.
    @Test func persistWritesTabsInDisplayOrder() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }

        let ws = await Workspace(statePath: path)
        let p = await ws.createProject(name: "p", cwd: "/")
        let a = try await ws.openTab(projectID: p.id, cwd: "/a", title: "a")
        let b = try await ws.openTab(projectID: p.id, cwd: "/b", title: "b")
        _ = try await ws.openTab(projectID: p.id, cwd: "/c", title: "c")
        // Close from the middle and the front so positions go sparse.
        try await ws.closeTab(b.id)
        try await ws.closeTab(a.id)
        _ = try await ws.openTab(projectID: p.id, cwd: "/d", title: "d")

        let displayed = await ws.tabs(in: p.id)
        #expect(displayed.map(\.position) == [2, 3], "positions stay sparse")
        let written = try decodeSnapshot(at: path)
        let writtenTabs = try #require(written.projects.first?.tabs)
        #expect(writtenTabs.map(\.cwd) == displayed.map(\.cwd))
        #expect(writtenTabs.map(\.cwd) == ["/c", "/d"])
    }

    /// `Int32.max + 1` traps in Swift, and positions decode straight
    /// from a user-editable `state.json`. A workspace at `Int32.max` is
    /// corrupt input, not a supported state: the allocator degrades it
    /// to a tie rather than crashing the app at launch.
    @Test func projectPositionAtInt32MaxClampsInsteadOfTrapping() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        let maxed = """
        {
            "next_id": 10,
            "projects": [{
                "id": 1, "name": "maxed", "cwd": "/tmp",
                "position": 2147483647, "created_at": 1, "tabs": []
            }]
        }
        """
        try maxed.write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        let next = await ws.createProject(name: "next", cwd: "/tmp")
        #expect(next.position == Int32.max, "clamped to a tie, not trapped")
        // The tab allocator shares the clamp; a tab at `Int32.max` is
        // unreachable in Swift (tabs never restore as live rows), so
        // `next_position_saturates_at_i32_max` covers that half in Rust.
        let tab = try await ws.openTab(projectID: 1, cwd: "/tmp", title: "t")
        #expect(tab.position == 0)
    }

    /// The clamp above deliberately produces a *tie* rather than
    /// trapping — this is the downstream half: once two projects sit
    /// at `Int32.max`, nothing may lose data, crash, or reorder
    /// nondeterministically. Loads the tie directly (rather than via
    /// `createProject`, which `projectPositionAtInt32MaxClampsInsteadOfTrapping`
    /// already covers) so this test is about survival after the tie
    /// exists, not about how it got there. It is also the load-path
    /// normalizer's boundary case: `[Int32.max, Int32.max]` must clamp
    /// to a surviving tie rather than trap on `previous + 1`.
    @Test func projectsTiedAtInt32MaxSurviveLoadAndRoundTrip() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        let tied = """
        {
            "next_id": 10,
            "projects": [
                { "id": 1, "name": "p1", "cwd": "/tmp",
                  "position": 2147483647, "created_at": 1, "tabs": [] },
                { "id": 2, "name": "p2", "cwd": "/tmp",
                  "position": 2147483647, "created_at": 2, "tabs": [] }
            ]
        }
        """
        try tied.write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        #expect(
            await ws.snapshot().map(\.id) == [1, 2],
            "both projects survive the load — the tie must not lose one"
        )
        #expect(await ws.snapshot().map(\.position) == [Int32.max, Int32.max])

        // Any persisting mutator round-trips the tie through disk.
        try await ws.renameProject(1, name: "renamed")
        let ws2 = await Workspace(statePath: path)
        #expect(
            await ws2.snapshot().map(\.id) == [1, 2],
            "the tie survives a reopen in the same relative order"
        )
        #expect(await ws2.snapshot().map(\.name) == ["renamed", "p2"])
    }

    /// `afterMax` isn't just a max-boundary clamp — plain `highest + 1`
    /// must still hold below it. Positions decode straight from
    /// `state.json` with no validation, so a negative value (e.g. from
    /// hand-editing, or a future allocator bug) is representable input.
    @Test func negativeProjectPositionAllocatesNextUp() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        let negative = """
        {
            "next_id": 10,
            "projects": [{
                "id": 1, "name": "negative", "cwd": "/tmp",
                "position": -5, "created_at": 1, "tabs": []
            }]
        }
        """
        try negative.write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        let next = await ws.createProject(name: "next", cwd: "/tmp")
        #expect(next.position == -4, "max + 1 must hold below -1, not just near Int32.max")
    }

    // MARK: Load-path repair of colliding project positions
    //
    // Persisted *tabs* re-open through `openTab` (App.swift's
    // bootstrap), so their positions are freshly allocated and a
    // persisted tab collision self-heals. Persisted *projects* load
    // their position verbatim, so a collision written by a pre-fix
    // build survives every relaunch until the load-path normalizer
    // breaks the tie.

    /// A `state.json` whose projects share a position must restore
    /// with unique positions, in the same relative order.
    @Test func collidingProjectPositionsAreRepairedOnLoad() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([(100, "AAA", 0), (101, "BBB", 2), (102, "CCC", 2)])
            .write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        let snap = await ws.snapshot()
        #expect(snap.map(\.name) == ["AAA", "BBB", "CCC"], "relative order is preserved")
        #expect(snap.map(\.position) == [0, 2, 3], "the tie is broken by pushing the loser up")
    }

    /// Positions are sparse by design — the invariant is uniqueness
    /// within a parent, never density. Repairing `[0, 5, 5]` must
    /// yield `[0, 5, 6]`, not `[0, 1, 2]`: densifying would rewrite
    /// the two rows that were never wrong.
    @Test func projectPositionRepairPreservesGaps() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([(100, "AAA", 0), (101, "BBB", 5), (102, "CCC", 5)])
            .write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        #expect(await ws.snapshot().map(\.position) == [0, 5, 6])
    }

    /// Repairing an already-repaired file changes nothing: load the
    /// collision, persist the repair, reload. The second load must see
    /// unique positions and leave them exactly where the first put
    /// them.
    @Test func projectPositionRepairIsIdempotentAcrossReopen() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([(100, "AAA", 0), (101, "BBB", 2), (102, "CCC", 2)])
            .write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        #expect(await ws.snapshot().map(\.position) == [0, 2, 3])
        // Any persisting mutator writes the repair back to disk.
        try await ws.renameProject(100, name: "AAA")

        let ws2 = await Workspace(statePath: path)
        let reopened = await ws2.snapshot()
        #expect(
            reopened.map(\.position) == [0, 2, 3],
            "normalizing twice equals normalizing once"
        )
        #expect(reopened.map(\.name) == ["AAA", "BBB", "CCC"])
    }

    /// The overwhelmingly common case: nobody's positions collide.
    /// The repair must not touch them, gaps included.
    @Test func uniqueProjectPositionsAreLeftAlone() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([(100, "AAA", 0), (101, "BBB", 5), (102, "CCC", 9)])
            .write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        #expect(await ws.snapshot().map(\.position) == [0, 5, 9])
    }

    /// The reason the walk seeds `previous` with `nil` rather than
    /// `-1`. `position` decodes as a plain `Int32` with no
    /// non-negativity check, so a hand-edited file can hold negatives;
    /// a `-1` seed would rewrite a unique, correctly-ordered
    /// `[-5, -3]` to `[0, 1]`, breaking the no-op guarantee on input
    /// that was never wrong.
    @Test func uniqueNegativeProjectPositionsAreLeftAlone() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([(100, "AAA", -5), (101, "BBB", -3)])
            .write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        #expect(await ws.snapshot().map(\.position) == [-5, -3])
    }

    /// A file with no projects must load as an empty workspace rather
    /// than tripping the normalizer's walk (`previous` never leaves
    /// `nil`) or the restore-layout build.
    @Test func emptyProjectListLoadsClean() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([]).write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        #expect(await ws.snapshot().isEmpty)
        let restore = try #require(await ws.takeRestoreLayout())
        #expect(restore.projects.isEmpty)
    }

    /// The accepted degradation at the ceiling, end to end: repairing
    /// `[Int32.max - 1, Int32.max - 1]` pushes the loser to `Int32.max`,
    /// and the next `createProject` clamps onto that same value. Two
    /// projects then share `Int32.max`.
    ///
    /// **This is the pinned behavior, not a defect** (plan 004 §3.1): a
    /// workspace at the integer ceiling is corrupt input, not a
    /// supported state, so the arithmetic degrades to a tie rather than
    /// trapping at launch. The repair is a *uniqueness* pass below the
    /// ceiling and a *survival* pass at it — it is explicitly **not** a
    /// uniqueness guarantee. The Rust
    /// `ceiling_repair_degrades_to_a_tie_on_the_next_allocation`
    /// mirrors this.
    @Test func ceilingRepairDegradesToATieOnTheNextAllocation() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([(100, "AAA", .max - 1), (101, "BBB", .max - 1)])
            .write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        #expect(
            await ws.snapshot().map(\.position) == [.max - 1, .max],
            "the repair spends the last position in the range"
        )

        let next = await ws.createProject(name: "CCC", cwd: "/tmp")
        #expect(
            next.position == .max,
            "max + 1 saturates onto the row the repair just placed"
        )
        #expect(
            await ws.snapshot().map(\.position) == [.max - 1, .max, .max],
            "accepted tie at the ceiling"
        )
    }

    /// A file whose row order differs from display order, with a
    /// collision inside it. The repair must be computed over the
    /// *display* walk while everything keyed off file order — the
    /// restore layout, the active-project resolution — is untouched,
    /// and `snapshot()` must still come back by `(position, id)`.
    ///
    /// File order `C@5, A@0, B@5`; display order `A@0, C@5, B@5`. The
    /// tie is between C and B, and only the walk order reveals which
    /// of them moves. The Rust
    /// `file_order_differing_from_display_order_repairs_and_restores`
    /// mirrors it.
    @Test func fileOrderDifferingFromDisplayOrderRepairsAndRestores() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([(1, "C", 5), (2, "A", 0), (3, "B", 5)], activeProjectID: 3)
            .write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        let snap = await ws.snapshot()
        #expect(snap.map(\.name) == ["A", "C", "B"], "snapshot is by (position, id)")
        #expect(snap.map(\.position) == [0, 5, 6], "B loses the tie to the lower id")

        let restore = try #require(await ws.takeRestoreLayout())
        #expect(
            restore.projects.map(\.projectID) == [1, 2, 3],
            "the restore layout keeps the file's row order"
        )
        #expect(restore.activeProjectID == 3)
        #expect(
            await ws.project(3) != nil,
            "the active project still resolves against the repaired rows"
        )
    }

    /// Two rows sharing an `id` — corrupt input, but input the loader
    /// accepts. The id-keyed `projects` dictionary collapses them
    /// last-write-wins, so **insertion order picks the survivor**, and
    /// insertion order is the file's.
    ///
    /// The collapse itself is pre-existing and deliberate on both
    /// sides; what this pins is that the load-path repair does not
    /// change *which* row wins, and that Rust agrees — its
    /// `duplicate_project_ids_keep_the_file_order_survivor` asserts the
    /// same literal fixture. Were the two to disagree, one `state.json`
    /// would yield a different project identity, metadata, position and
    /// tab layout on macOS than on Linux.
    ///
    /// Fails against a normalizer that hands its pairs back in display
    /// order: that walk inserts `low@0` first, leaving `high@10` as the
    /// survivor while Rust keeps `low@0`.
    @Test func duplicateProjectIDsKeepTheFileOrderSurvivor() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([(7, "high", 10), (7, "low", 0)])
            .write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        let snap = await ws.snapshot()
        #expect(snap.count == 1, "the duplicate id collapses to one row")
        #expect(
            snap.map(\.name) == ["low"] && snap.map(\.position) == [0],
            "the last row in the file wins, not the last in display order"
        )
    }

    /// Two rows sharing an `id` **and** a `position` make `(position, id)`
    /// a non-total order, and `sorted(by:)` on a predicate that is not a
    /// strict weak ordering has no defined result — so the repair walk
    /// needs the file index as a final key to be defined at all. Rust
    /// reaches the same order for free (`sort_by_key` is stable). The
    /// walk order decides the *positions*; which row survives insertion
    /// is the file's order either way, which
    /// `duplicateProjectIDsKeepTheFileOrderSurvivor` pins.
    @Test func duplicateProjectRowsRepairInFileOrder() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try projectsState([(7, "first", 5), (7, "second", 5)])
            .write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        let snap = await ws.snapshot()
        #expect(snap.map(\.name) == ["second"], "the last row in the file wins")
        #expect(
            snap.map(\.position) == [6],
            "the second row is the one pushed up, because it walks second"
        )
    }

    /// Persist is stable across a reload: load a collided file, replay
    /// the bootstrap, persist; do it all again; the two files' *project*
    /// rows must be identical. A repair that shifted a row a little
    /// further on each launch would pass every single-load test above
    /// and still walk a user's sidebar apart over a week of restarts.
    ///
    /// Whole-file byte equality is deliberately not asserted: the
    /// bootstrap re-opens each persisted tab through `openTab`, which
    /// allocates fresh ids, so `next_id` legitimately advances every
    /// launch. Project rows are the unit that must not drift.
    @Test func secondPersistOfACollidedFileIsStable() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try Self.collidedProjectsWithTabsJSON
            .write(toFile: path, atomically: true, encoding: .utf8)

        // Replay App.swift's bootstrap: every persisted tab re-opens as
        // a fresh shell through the normal allocation path, then the
        // layout is flushed.
        func loadRestoreAndPersist() async throws -> [Workspace.SnapshotFile.ProjectSnapshot] {
            let ws = await Workspace(statePath: path)
            let restore = try #require(await ws.takeRestoreLayout())
            for rp in restore.projects {
                for spec in rp.tabs {
                    _ = try await ws.openTab(
                        projectID: rp.projectID,
                        cwd: spec.cwd,
                        title: spec.title
                    )
                }
            }
            await ws.flush()
            return try decodeSnapshot(at: path).projects
        }

        let first = try await loadRestoreAndPersist()
        #expect(first.map(\.position) == [2, 3], "the collision is repaired on the first load")
        let second = try await loadRestoreAndPersist()
        #expect(first == second, "a second load + persist must not move anything")
        #expect(
            try decodeSnapshot(at: path).nextID > 200,
            "ids did advance — the equality above is not a trivially unchanged file"
        )
    }

    /// Colliding project positions *and* tabs, so the round trip in
    /// `secondPersistOfACollidedFileIsStable` allocates ids and can
    /// tell a stable project row from an unchanged file.
    private static let collidedProjectsWithTabsJSON = """
    {
      "next_id": 200,
      "active_project_id": 101,
      "active_tab_position": 0,
      "projects": [
        { "id": 100, "name": "AAA", "cwd": "/tmp", "position": 2, "created_at": 100,
          "tabs": [
            { "title": "a", "cwd": "/tmp/a", "position": 3, "user_titled": false },
            { "title": "b", "cwd": "/tmp/b", "position": 3, "user_titled": false }
          ] },
        { "id": 101, "name": "BBB", "cwd": "/tmp", "position": 2, "created_at": 101,
          "tabs": [
            { "title": "c", "cwd": "/tmp/c", "position": 0, "user_titled": false }
          ] }
      ]
    }
    """

    /// The real collided `state.json` captured from a user's machine
    /// (issue #262): its `roost` project holds tab positions
    /// `[2, 3, 3]`.
    ///
    /// **Be clear about what this does and does not prove.** Tab
    /// positions self-heal by construction — the restore layout is a
    /// set of *descriptors*, and the UI bootstrap re-opens each one
    /// through `openTab`, which allocates a fresh position. This test
    /// replays that bootstrap and is therefore a **regression guard
    /// against a future change that would let persisted tab positions
    /// survive**, not a test of the bug the load-path repair fixes.
    /// The project-position tests above are the ones that fail without
    /// the repair; this file's project positions (`0…4`) are already
    /// unique, so it also pins the no-op case on real input.
    @Test func realCollidedStateFileRestoresWithUniquePositions() async throws {
        let path = tempPath()
        defer { try? FileManager.default.removeItem(atPath: path) }
        try Self.collidedStateJSON.write(toFile: path, atomically: true, encoding: .utf8)

        let ws = await Workspace(statePath: path)
        let restore = try #require(await ws.takeRestoreLayout())
        // Replay App.swift's bootstrap: every persisted tab re-opens
        // as a fresh shell through the normal allocation path.
        for rp in restore.projects {
            for spec in rp.tabs {
                _ = try await ws.openTab(
                    projectID: rp.projectID,
                    cwd: spec.cwd,
                    title: spec.title
                )
            }
        }

        let projects = await ws.snapshot()
        #expect(projects.map(\.name) == ["roost", "shed", "cadence", "cc-plugins", "slaudio"])
        #expect(
            projects.map(\.position) == [0, 1, 2, 3, 4],
            "already-unique project positions survive untouched"
        )
        for p in projects {
            let positions = await ws.tabs(in: p.id).map(\.position)
            #expect(
                Set(positions).count == positions.count,
                "project '\(p.name)' restored with duplicate tab positions: \(positions)"
            )
        }
        #expect(await ws.tabs(in: 2260).count == 3, "the collided project keeps all three tabs")
    }

    /// Verbatim copy of `~/Library/Application Support/Roost/state.json`
    /// as captured from the reporting machine, inlined rather than
    /// added as a bundle resource: every other `state.json` test in
    /// this suite writes a literal to a temp path, and a resource would
    /// cost a `resources:` declaration on the test target plus
    /// `Bundle.module` plumbing for one file. Raw string so JSON's
    /// `\/` escapes survive — Swift rejects `\/` in a normal literal.
    private static let collidedStateJSON = #"""
    {
      "active_project_id" : 2260,
      "active_tab_position" : 0,
      "next_id" : 2280,
      "projects" : [
        {
          "created_at" : 1785203391,
          "cwd" : "",
          "id" : 2260,
          "name" : "roost",
          "position" : 0,
          "tabs" : [
            {
              "cwd" : "\/Users\/charliek\/projects\/roost",
              "position" : 2,
              "title" : "✳ Claude Code",
              "user_titled" : false
            },
            {
              "cwd" : "\/Users\/charliek\/projects\/roost",
              "position" : 3,
              "title" : "✳ Test waiting for input view color update",
              "user_titled" : false
            },
            {
              "cwd" : "\/Users\/charliek\/projects\/roost",
              "position" : 3,
              "title" : "🟢 \/Users\/charliek\/projects\/roost (feature\/plan-003-roostctl-doctor)",
              "user_titled" : false
            }
          ]
        },
        {
          "created_at" : 1785204289,
          "cwd" : "",
          "id" : 2266,
          "name" : "shed",
          "position" : 1,
          "tabs" : [
            {
              "cwd" : "\/Users\/charliek",
              "position" : 0,
              "title" : "👻 \/Users\/charliek",
              "user_titled" : false
            }
          ]
        },
        {
          "created_at" : 1785206061,
          "cwd" : "",
          "id" : 2269,
          "name" : "cadence",
          "position" : 2,
          "tabs" : [
            {
              "cwd" : "\/Users\/charliek",
              "position" : 0,
              "title" : "👻 \/Users\/charliek",
              "user_titled" : false
            }
          ]
        },
        {
          "created_at" : 1785206066,
          "cwd" : "",
          "id" : 2271,
          "name" : "cc-plugins",
          "position" : 3,
          "tabs" : [
            {
              "cwd" : "\/Users\/charliek",
              "position" : 0,
              "title" : "👻 \/Users\/charliek",
              "user_titled" : false
            }
          ]
        },
        {
          "created_at" : 1785206076,
          "cwd" : "",
          "id" : 2273,
          "name" : "slaudio",
          "position" : 4,
          "tabs" : [
            {
              "cwd" : "\/Users\/charliek",
              "position" : 0,
              "title" : "👻 \/Users\/charliek",
              "user_titled" : false
            }
          ]
        }
      ]
    }
    """#
}
