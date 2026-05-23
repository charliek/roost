// Workspace.swift — daemon-removal refactor M4a.
//
// In-process workspace state for the Mac UI. Holds the project +
// tab maps, the active selection, and a monotonically-increasing
// id counter; persists projects + next_id to `state.json` at the
// bundle profile's `stateDir`.
//
// Mirrors `crates/roost-linux/src/daemon/state.rs` semantically:
// in-memory `BTreeMap`-style storage, atomic write via tmp +
// rename, tabs do not survive UI quits (no-session-restore
// goal), corrupt state.json → start empty.
//
// Threading: the workspace is `@MainActor`. PTY callbacks that
// need to mutate state hop here via `Task { @MainActor in ... }`.
// State broadcasts go out via the `events` AsyncStream so the
// IPC server can push `events.subscribe` envelopes from
// background tasks.

import Foundation

@MainActor
final class Workspace {
    // MARK: Types

    enum TabState: String, Codable, Sendable {
        case none
        case running
        case needsInput = "needs_input"
        case idle
    }

    struct Project: Hashable, Sendable {
        var id: Int64
        var name: String
        var cwd: String
        var position: Int32
        var createdAt: Int64
    }

    struct Tab: Hashable, Sendable {
        var id: Int64
        var projectId: Int64
        var title: String
        var cwd: String
        var state: TabState
        var hasNotification: Bool
        var userTitled: Bool
        var position: Int32
        var createdAt: Int64
        var lastActive: Int64
        var hookActive: Bool
    }

    enum Event: Sendable {
        case tabOpened(Tab)
        case tabClosed(tabID: Int64)
        case tabStateChanged(tabID: Int64, state: TabState)
        case tabTitleChanged(tabID: Int64, title: String)
        case tabCwdChanged(tabID: Int64, cwd: String)
        case tabNotification(tabID: Int64, hasPending: Bool)
        case projectCreated(Project)
        case projectRenamed(projectID: Int64, name: String)
        case projectDeleted(projectID: Int64)
        case activeChanged(projectID: Int64, tabID: Int64)
        case hookActiveChanged(tabID: Int64, active: Bool)
        case notificationFired(tabID: Int64, title: String, body: String)
    }

    enum WorkspaceError: Error, CustomStringConvertible {
        case projectNotFound(Int64)
        case tabNotFound(Int64)
        case tabProjectMismatch(projectID: Int64, tabID: Int64)

        var description: String {
            switch self {
            case .projectNotFound(let id): return "project \(id) not found"
            case .tabNotFound(let id): return "tab \(id) not found"
            case .tabProjectMismatch(let p, let t):
                return "tab \(t) does not belong to project \(p)"
            }
        }
    }

    // MARK: State

    private var projects: [Int64: Project] = [:]
    private var tabs: [Int64: Tab] = [:]
    private var nextID: Int64 = 1
    private(set) var activeProjectID: Int64 = 0
    private(set) var activeTabID: Int64 = 0
    private let statePath: String?
    private var observers: [UUID: @Sendable (Event) -> Void] = [:]

    // MARK: Init

    /// In-memory workspace (used by tests).
    init() {
        self.statePath = nil
    }

    /// Open or create the workspace backed by `state.json` at
    /// `statePath`. Reads existing projects + next_id. Tabs are
    /// intentionally NOT restored (the no-session-restore goal).
    /// Corrupt or absent file → start empty (warn-log).
    init(statePath: String) {
        self.statePath = statePath
        if let snapshot = Self.readSnapshot(at: statePath) {
            self.nextID = max(1, snapshot.nextID)
            for p in snapshot.projects {
                self.projects[p.id] = Project(
                    id: p.id,
                    name: p.name,
                    cwd: p.cwd,
                    position: p.position,
                    createdAt: p.createdAt
                )
            }
        }
    }

    // MARK: Subscribe

    /// Register an observer for workspace events. Returns an
    /// opaque token; pass it to `unsubscribe(token:)` when done.
    @discardableResult
    func subscribe(_ handler: @escaping @Sendable (Event) -> Void) -> UUID {
        let token = UUID()
        observers[token] = handler
        return token
    }

    func unsubscribe(token: UUID) {
        observers.removeValue(forKey: token)
    }

    private func emit(_ event: Event) {
        for handler in observers.values {
            handler(event)
        }
    }

    // MARK: Snapshots

    /// Full snapshot in (sorted) display order.
    func snapshot() -> [Project] {
        let sortedProjects = projects.values
            .sorted { ($0.position, $0.id) < ($1.position, $1.id) }
        return Array(sortedProjects)
    }

    /// Tabs for a project in display order. Returns empty if the
    /// project is gone.
    func tabs(in projectID: Int64) -> [Tab] {
        tabs.values
            .filter { $0.projectId == projectID }
            .sorted { ($0.position, $0.id) < ($1.position, $1.id) }
    }

    func tab(_ tabID: Int64) -> Tab? { tabs[tabID] }
    func project(_ projectID: Int64) -> Project? { projects[projectID] }

    // MARK: Project mutators

    @discardableResult
    func createProject(name: String, cwd: String) -> Project {
        let id = allocID()
        let chosenName = name.isEmpty ? "Untitled \(projects.count + 1)" : name
        let position = Int32(projects.count)
        let project = Project(
            id: id,
            name: chosenName,
            cwd: cwd,
            position: position,
            createdAt: unixNow()
        )
        projects[id] = project
        persist()
        emit(.projectCreated(project))
        return project
    }

    func renameProject(_ projectID: Int64, name: String) throws {
        guard var p = projects[projectID] else {
            throw WorkspaceError.projectNotFound(projectID)
        }
        p.name = name
        projects[projectID] = p
        persist()
        emit(.projectRenamed(projectID: projectID, name: name))
    }

    /// Delete a project and all its tabs. Returns the cascaded tab
    /// ids so the caller can close their PTYs. The
    /// per-tab `tabClosed` events fire first, then
    /// `projectDeleted`, then `activeChanged` if the selection
    /// moved.
    @discardableResult
    func deleteProject(_ projectID: Int64) throws -> [Int64] {
        guard projects[projectID] != nil else {
            throw WorkspaceError.projectNotFound(projectID)
        }
        let cascaded = tabs.values
            .filter { $0.projectId == projectID }
            .map { $0.id }
        for tid in cascaded {
            tabs.removeValue(forKey: tid)
        }
        projects.removeValue(forKey: projectID)

        var activeChanged = false
        if activeProjectID == projectID || cascaded.contains(activeTabID) {
            // Pick the first project + its first tab IN DISPLAY ORDER,
            // not by id. Falling back by id ignores user-driven
            // sidebar reorders; CR-flagged on PR #78.
            let fallbackProject = projects.values
                .sorted { ($0.position, $0.id) < ($1.position, $1.id) }
                .first
                .map { $0.id } ?? 0
            let fallbackTab = tabs.values
                .filter { $0.projectId == fallbackProject }
                .sorted { ($0.position, $0.id) < ($1.position, $1.id) }
                .first
                .map { $0.id } ?? 0
            activeProjectID = fallbackProject
            activeTabID = fallbackTab
            activeChanged = true
        }
        persist()

        for tid in cascaded {
            emit(.tabClosed(tabID: tid))
        }
        emit(.projectDeleted(projectID: projectID))
        if activeChanged {
            emit(.activeChanged(projectID: activeProjectID, tabID: activeTabID))
        }
        return cascaded
    }

    /// Reorder tabs within a project. Ids not listed keep their
    /// relative order trailing the listed ones.
    func reorderTabs(projectID: Int64, tabIDs: [Int64]) throws {
        guard projects[projectID] != nil else {
            throw WorkspaceError.projectNotFound(projectID)
        }
        for tid in tabIDs {
            guard let t = tabs[tid] else {
                throw WorkspaceError.tabNotFound(tid)
            }
            if t.projectId != projectID {
                throw WorkspaceError.tabProjectMismatch(projectID: projectID, tabID: tid)
            }
        }
        var next: Int32 = 0
        for tid in tabIDs {
            tabs[tid]?.position = next
            next += 1
        }
        let unlisted = tabs.values
            .filter { $0.projectId == projectID && !tabIDs.contains($0.id) }
            .sorted { $0.position < $1.position }
            .map { $0.id }
        for tid in unlisted {
            tabs[tid]?.position = next
            next += 1
        }
        persist()
    }

    func reorderProjects(_ projectIDs: [Int64]) throws {
        for pid in projectIDs {
            guard projects[pid] != nil else {
                throw WorkspaceError.projectNotFound(pid)
            }
        }
        var next: Int32 = 0
        for pid in projectIDs {
            projects[pid]?.position = next
            next += 1
        }
        let unlisted = projects.values
            .filter { !projectIDs.contains($0.id) }
            .sorted { $0.position < $1.position }
            .map { $0.id }
        for pid in unlisted {
            projects[pid]?.position = next
            next += 1
        }
        persist()
    }

    // MARK: Tab mutators

    func openTab(projectID: Int64, cwd: String, title: String) throws -> Tab {
        guard projects[projectID] != nil else {
            throw WorkspaceError.projectNotFound(projectID)
        }
        let id = allocID()
        let position = Int32(tabs.values.lazy.filter { $0.projectId == projectID }.count)
        let derivedTitle = title.isEmpty ? deriveTitle(cwd: cwd) : title
        let now = unixNow()
        let tab = Tab(
            id: id,
            projectId: projectID,
            title: derivedTitle,
            cwd: cwd,
            state: .none,
            hasNotification: false,
            userTitled: !title.isEmpty,
            position: position,
            createdAt: now,
            lastActive: now,
            hookActive: false
        )
        tabs[id] = tab
        activeProjectID = projectID
        activeTabID = id
        persist()
        emit(.tabOpened(tab))
        emit(.activeChanged(projectID: projectID, tabID: id))
        return tab
    }

    func closeTab(_ tabID: Int64) throws {
        guard let row = tabs.removeValue(forKey: tabID) else {
            throw WorkspaceError.tabNotFound(tabID)
        }
        var activeChanged = false
        if activeTabID == tabID {
            // Pick the next tab in DISPLAY ORDER (position-sorted),
            // not dictionary order. CR-flagged on PR #78.
            let siblingsInProject = tabs.values
                .filter { $0.projectId == row.projectId }
                .sorted { ($0.position, $0.id) < ($1.position, $1.id) }
            let next = siblingsInProject.first
                ?? tabs.values.sorted { ($0.position, $0.id) < ($1.position, $1.id) }.first
            activeProjectID = next?.projectId ?? row.projectId
            activeTabID = next?.id ?? 0
            activeChanged = true
        }
        persist()
        emit(.tabClosed(tabID: tabID))
        if activeChanged {
            emit(.activeChanged(projectID: activeProjectID, tabID: activeTabID))
        }
    }

    func setTabTitle(_ tabID: Int64, title: String) throws {
        guard var t = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
        t.title = title
        t.userTitled = true
        tabs[tabID] = t
        emit(.tabTitleChanged(tabID: tabID, title: title))
    }

    /// OSC 0/1/2 title — respects a prior manual rename.
    func setTabTitleFromOSC(_ tabID: Int64, title: String) throws {
        guard var t = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
        if t.userTitled { return }
        t.title = title
        tabs[tabID] = t
        emit(.tabTitleChanged(tabID: tabID, title: title))
    }

    func setTabCwd(_ tabID: Int64, cwd: String) throws {
        guard var t = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
        t.cwd = cwd
        tabs[tabID] = t
        emit(.tabCwdChanged(tabID: tabID, cwd: cwd))
    }

    func setTabState(_ tabID: Int64, state: TabState) throws {
        guard var t = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
        t.state = state
        tabs[tabID] = t
        emit(.tabStateChanged(tabID: tabID, state: state))
    }

    func setTabHasNotification(_ tabID: Int64, hasPending: Bool) throws {
        guard var t = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
        t.hasNotification = hasPending
        tabs[tabID] = t
        emit(.tabNotification(tabID: tabID, hasPending: hasPending))
    }

    func setTabHookActive(_ tabID: Int64, active: Bool) throws {
        guard var t = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
        t.hookActive = active
        tabs[tabID] = t
        emit(.hookActiveChanged(tabID: tabID, active: active))
    }

    func focusTab(_ tabID: Int64) throws -> (previousProject: Int64, previousTab: Int64) {
        guard let row = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
        let prev = (activeProjectID, activeTabID)
        activeProjectID = row.projectId
        activeTabID = row.id
        emit(.activeChanged(projectID: row.projectId, tabID: row.id))
        return prev
    }

    func fireNotification(_ tabID: Int64, title: String, body: String) throws {
        guard tabs[tabID] != nil else { throw WorkspaceError.tabNotFound(tabID) }
        emit(.notificationFired(tabID: tabID, title: title, body: body))
    }

    /// Ensure a default project exists; return its id. Used by
    /// `tab.open` when the caller passes `project_id = 0` and the
    /// workspace is empty.
    @discardableResult
    func ensureDefaultProject(cwd: String) -> Int64 {
        if let first = projects.values.sorted(by: {
            ($0.position, $0.id) < ($1.position, $1.id)
        }).first {
            if activeProjectID == 0 {
                activeProjectID = first.id
                emit(.activeChanged(projectID: first.id, tabID: 0))
            }
            return first.id
        }
        let project = createProject(name: "Default", cwd: cwd)
        activeProjectID = project.id
        emit(.activeChanged(projectID: project.id, tabID: 0))
        return project.id
    }

    // MARK: Persistence

    private func allocID() -> Int64 {
        nextID = max(1, nextID) + 1
        return nextID
    }

    private func persist() {
        guard let statePath else { return }
        let snapshot = SnapshotFile(
            nextID: nextID,
            projects: projects.values
                .sorted { ($0.position, $0.id) < ($1.position, $1.id) }
                .map { p in
                    SnapshotFile.ProjectSnapshot(
                        id: p.id,
                        name: p.name,
                        cwd: p.cwd,
                        position: p.position,
                        createdAt: p.createdAt
                    )
                }
        )
        do {
            try Self.write(snapshot: snapshot, to: statePath)
        } catch {
            NSLog("workspace: failed to persist state.json: \(error)")
        }
    }

    private static func readSnapshot(at path: String) -> SnapshotFile? {
        let url = URL(fileURLWithPath: path)
        guard let data = try? Data(contentsOf: url), !data.isEmpty else {
            return nil
        }
        do {
            let decoder = JSONDecoder()
            return try decoder.decode(SnapshotFile.self, from: data)
        } catch {
            NSLog("workspace: state.json failed to load (\(error)); starting empty")
            return nil
        }
    }

    /// Atomic write: write to `<path>.tmp`, fsync, rename. Keeps
    /// `<path>.bak` of the prior version as a one-level rollback.
    static func write(snapshot: SnapshotFile, to path: String) throws {
        let url = URL(fileURLWithPath: path)
        let parent = url.deletingLastPathComponent()
        try FileManager.default.createDirectory(
            at: parent,
            withIntermediateDirectories: true
        )
        // Best-effort backup.
        if FileManager.default.fileExists(atPath: path) {
            let bak = url.appendingPathExtension("bak")
            try? FileManager.default.removeItem(at: bak)
            try? FileManager.default.copyItem(at: url, to: bak)
        }
        let tmp = url.appendingPathExtension("tmp")
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        let data = try encoder.encode(snapshot) + Data([0x0a])
        // Remove any stale tmp so a shorter new JSON can't leave
        // trailing bytes from a previous attempt — CR-flagged on
        // PR #78. The rename below is atomic, so the only window
        // where a half-written tmp could be promoted is between
        // writeFile and rename in the next write cycle. Truncating
        // first eliminates that window.
        try? FileManager.default.removeItem(at: tmp)
        FileManager.default.createFile(atPath: tmp.path, contents: nil)
        let handle = try FileHandle(forWritingTo: tmp)
        try handle.write(contentsOf: data)
        try handle.synchronize()
        try handle.close()
        // Atomic rename.
        if FileManager.default.fileExists(atPath: path) {
            try FileManager.default.removeItem(at: url)
        }
        try FileManager.default.moveItem(at: tmp, to: url)
    }

    struct SnapshotFile: Codable, Equatable, Sendable {
        let nextID: Int64
        let projects: [ProjectSnapshot]

        struct ProjectSnapshot: Codable, Equatable, Sendable {
            let id: Int64
            let name: String
            let cwd: String
            let position: Int32
            let createdAt: Int64

            enum CodingKeys: String, CodingKey {
                case id, name, cwd, position
                case createdAt = "created_at"
            }
        }

        enum CodingKeys: String, CodingKey {
            case nextID = "next_id"
            case projects
        }
    }
}

private func unixNow() -> Int64 {
    Int64(Date().timeIntervalSince1970)
}

private func deriveTitle(cwd: String) -> String {
    if cwd.isEmpty { return "shell" }
    return (cwd as NSString).lastPathComponent.isEmpty ? "shell" : (cwd as NSString).lastPathComponent
}
