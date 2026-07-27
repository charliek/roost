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
// `crates/roost-linux/src/daemon/state.rs` case for case, so the two
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
}
