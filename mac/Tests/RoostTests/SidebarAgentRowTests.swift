// Pure-logic tests for the Mac sidebar's agent rows (plan 007 §3.2–§3.4).
//
// No test in this suite drives `NSOutlineView`; the testable pieces are
// the rendered-row model (which rows exist, in what order, and which one
// wears the active highlight) and the row colours. The outline's own row
// numbering — what the row-index refactor depends on — is only reachable
// from the functional suite in `tools/roosttest/`.

import AppKit
import Testing

@testable import Roost

private let NOW: Int64 = 1_700_000_000

private func project(_ id: Int64) -> Workspace.Project {
    Workspace.Project(id: id, name: "p\(id)", cwd: "/tmp", position: 0, createdAt: 0)
}

private func agentTab(
    _ id: Int64, project: Int64, lifecycle: AgentLifecycle, at: Int64 = NOW - 5,
    title: String? = nil
) -> Workspace.Tab {
    Workspace.Tab(
        id: id,
        projectId: project,
        title: title ?? "tab-\(id)",
        cwd: "/tmp",
        agent: AgentTabState(
            shell: .atPrompt,
            lifecycle: lifecycle,
            ownership: Ownership(source: "claude", sessionID: "s\(id)", lastEventAt: at)),
        hasNotification: false,
        userTitled: false,
        position: 0,
        createdAt: 0,
        lastActive: 0
    )
}

private func shellTab(_ id: Int64, project: Int64) -> Workspace.Tab {
    Workspace.Tab(
        id: id,
        projectId: project,
        title: "shell",
        cwd: "/tmp",
        agent: AgentTabState(shell: .atPrompt, lifecycle: .inactive, ownership: nil),
        hasNotification: false,
        userTitled: false,
        position: 0,
        createdAt: 0,
        lastActive: 0
    )
}

// MARK: - renderedSidebarAgents

@MainActor
@Test func renderedAgentsKeepsEveryProjectInOrderIncludingEmptyOnes() {
    let rendered = renderedSidebarAgents(
        projects: [project(1), project(2), project(3)],
        tabs: [agentTab(10, project: 1, lifecycle: .working), shellTab(20, project: 2)],
        activeTabID: nil,
        now: NOW
    )
    #expect(rendered.map(\.projectID) == [1, 2, 3])
    #expect(rendered[0].agents.map(\.row.tabID) == [10])
    #expect(rendered[1].agents.isEmpty)
    #expect(rendered[2].agents.isEmpty)
}

@MainActor
@Test func renderedAgentsMarksOnlyTheActiveTabsRow() {
    let rendered = renderedSidebarAgents(
        projects: [project(1), project(2)],
        tabs: [
            agentTab(10, project: 1, lifecycle: .working),
            agentTab(11, project: 1, lifecycle: .working, at: NOW - 9),
            agentTab(20, project: 2, lifecycle: .waiting),
        ],
        activeTabID: 11,
        now: NOW
    )
    let flags = rendered.flatMap { entry in entry.agents.map { ($0.row.tabID, $0.isActive) } }
    #expect(flags.filter(\.1).map(\.0) == [11])
}

@MainActor
@Test func renderedAgentsMarksNothingActiveWithoutAnActiveTab() {
    let rendered = renderedSidebarAgents(
        projects: [project(1)],
        tabs: [agentTab(10, project: 1, lifecycle: .working)],
        activeTabID: nil,
        now: NOW
    )
    #expect(rendered[0].agents.allSatisfy { !$0.isActive })
}

@MainActor
@Test func renderedAgentsOrdersByUrgencyLikeThePalette() {
    let rendered = renderedSidebarAgents(
        projects: [project(1)],
        tabs: [
            agentTab(10, project: 1, lifecycle: .working, at: NOW),
            agentTab(11, project: 1, lifecycle: .waiting, at: NOW - 100),
            agentTab(12, project: 1, lifecycle: .finished, at: NOW),
        ],
        activeTabID: nil,
        now: NOW
    )
    #expect(rendered[0].agents.map(\.row.tabID) == [11, 10, 12])
}

// MARK: - equality guard

// The blanket post-event refresh reloads the outline only when the
// rendered rows differ from the ones on screen. These pin the two
// staleness cases that guard has to catch — a closed tab and a rename —
// and the identical-snapshot case that makes it cheap.

@MainActor
@Test func renderedAgentsCompareEqualForAnUnchangedSnapshot() {
    let projects = [project(1)]
    let tabs = [
        agentTab(10, project: 1, lifecycle: .working),
        agentTab(11, project: 1, lifecycle: .waiting),
    ]
    let first = renderedSidebarAgents(
        projects: projects, tabs: tabs, activeTabID: 10, now: NOW)
    let second = renderedSidebarAgents(
        projects: projects, tabs: tabs, activeTabID: 10, now: NOW)
    #expect(first[0].agents == second[0].agents)
}

@MainActor
@Test func renderedAgentsDifferWhenATabIsClosed() {
    let before = renderedSidebarAgents(
        projects: [project(1)],
        tabs: [
            agentTab(10, project: 1, lifecycle: .working),
            agentTab(11, project: 1, lifecycle: .working),
        ],
        activeTabID: nil,
        now: NOW
    )
    let after = renderedSidebarAgents(
        projects: [project(1)],
        tabs: [agentTab(10, project: 1, lifecycle: .working)],
        activeTabID: nil,
        now: NOW
    )
    #expect(before[0].agents != after[0].agents)
}

@MainActor
@Test func renderedAgentsDifferWhenATabIsRenamed() {
    let before = renderedSidebarAgents(
        projects: [project(1)],
        tabs: [agentTab(10, project: 1, lifecycle: .working, title: "old")],
        activeTabID: nil,
        now: NOW
    )
    let after = renderedSidebarAgents(
        projects: [project(1)],
        tabs: [agentTab(10, project: 1, lifecycle: .working, title: "new")],
        activeTabID: nil,
        now: NOW
    )
    #expect(before[0].agents.map(\.row.name) == ["old"])
    #expect(before[0].agents != after[0].agents)
}

// MARK: - colours

@MainActor
@Test func timeTextTakesTheLifecycleColourOnlyForWaitingAndFailed() {
    #expect(sidebarAgentTimeColor(for: .waiting) == rollupColor(for: .waiting))
    #expect(sidebarAgentTimeColor(for: .failed) == rollupColor(for: .failed))

    let muted = AgentPalette.metricsColor(.muted)
    #expect(sidebarAgentTimeColor(for: .working) == muted)
    #expect(sidebarAgentTimeColor(for: .finished) == muted)
    #expect(sidebarAgentTimeColor(for: .inactive) == muted)
}
