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
    case .notificationFired: return "notificationFired"
    case .tabsReordered: return "tabsReordered"
    case .projectsReordered: return "projectsReordered"
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
        // Tabs are NOT restored.
        let tabsInProject = await ws2.tabs(in: p.id)
        #expect(tabsInProject.isEmpty)
        // Ids advance past the previously-issued tab id.
        let nextTab = try await ws2.openTab(projectID: projectID, cwd: "/", title: "")
        #expect(nextTab.id > firstTabID)
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
}
