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
        /// Fired after `reorderTabs`. The `tabIDs` payload is the
        /// post-reorder display order (the `tabIDs` argument
        /// followed by any unlisted siblings in their prior order).
        case tabsReordered(projectID: Int64, tabIDs: [Int64])
        /// Fired after `reorderProjects`. `projectIDs` is the
        /// post-reorder sidebar order.
        case projectsReordered(projectIDs: [Int64])
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
    /// One-shot tab layout loaded from `state.json` at init, drained
    /// by the app bootstrap via `takeRestoreLayout()`. Kept out of
    /// the live `tabs` map — the live tabs are the fresh shells the
    /// UI re-opens from these descriptors.
    private var restoreLayout: RestoreLayout?
    /// Last time a cwd/OSC-title change persisted. These fire per
    /// shell prompt / `cd`, so they're throttled (`persistThrottled`)
    /// to avoid an fsync per change; `nil` means "never". Mirrors the
    /// Rust `last_meta_persist`.
    private var lastMetaPersist: Date?
    /// Leading-edge throttle window for chatty shell-driven persists.
    /// The in-memory value stays current; the next layout mutation
    /// flushes it, so the worst case is this much staleness on disk.
    private static let metaPersistMinInterval: TimeInterval = 1.0

    // MARK: Init

    /// In-memory workspace (used by tests).
    init() {
        self.statePath = nil
    }

    /// Open or create the workspace backed by `state.json` at
    /// `statePath`. Reads existing projects + next_id, and loads each
    /// project's persisted tab layout into a one-shot `restoreLayout`
    /// (drained by the app bootstrap via `takeRestoreLayout` to
    /// re-open fresh shells in their saved dirs). The layout is NOT
    /// inserted as live tabs. Corrupt or absent file → start empty
    /// (warn-log).
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
            self.restoreLayout = RestoreLayout(
                projects: snapshot.projects.map { p in
                    RestoreProject(
                        projectID: p.id,
                        tabs: p.tabs
                            .sorted { $0.position < $1.position }
                            .map { RestoreTab(cwd: $0.cwd, title: $0.title) }
                    )
                },
                activeProjectID: snapshot.activeProjectID,
                activeTabPosition: snapshot.activeTabPosition
            )
        }
    }

    /// Take the one-shot tab layout loaded from `state.json` at init.
    /// Returns `nil` for the in-memory variant and after the first
    /// call. The app bootstrap calls this once to re-open each
    /// project's saved tabs as fresh shells.
    func takeRestoreLayout() -> RestoreLayout? {
        defer { restoreLayout = nil }
        return restoreLayout
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
        // The post-reorder display order is the supplied prefix
        // followed by any unlisted tabs (in their prior order).
        // App.swift's `.tabsReordered` handler applies the same
        // order via `applyTabsReorder`.
        emit(.tabsReordered(projectID: projectID, tabIDs: tabIDs + unlisted))
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
        emit(.projectsReordered(projectIDs: projectIDs + unlisted))
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
        // Always start with userTitled=false. The caller-supplied
        // `title` is a *placeholder* (e.g. "roost-mac 1", or the
        // CLI's "roostctl" default) that the shell's OSC 0/1/2
        // emissions should be allowed to overwrite. Only an
        // explicit user rename via Cmd+R / `setTabTitle` sets
        // userTitled=true, which then locks against further OSC
        // overwrites. The pre-fix `!title.isEmpty` policy locked
        // every newly-opened tab to its placeholder, preventing
        // shell prompts like `👻 /tmp` from ever appearing in the
        // tab bar.
        let tab = Tab(
            id: id,
            projectId: projectID,
            title: derivedTitle,
            cwd: cwd,
            state: .none,
            hasNotification: false,
            userTitled: false,
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

    /// Close a tab. When it was the project's **last** tab, the
    /// project is closed too (mirrors `deleteProject`'s cascade) so a
    /// project can never linger with zero live tabs. The event order
    /// in that case is `tabClosed → projectDeleted → activeChanged`,
    /// matching `deleteProject`; App.swift's `.projectDeleted` arm
    /// then falls back to another project or closes the window when
    /// the workspace is empty.
    func closeTab(_ tabID: Int64) throws {
        guard let row = tabs.removeValue(forKey: tabID) else {
            throw WorkspaceError.tabNotFound(tabID)
        }
        let projectID = row.projectId

        // Last tab in the project? Cascade-close the project. Inlined
        // rather than delegating to `deleteProject` so the already-
        // removed tab isn't re-emitted.
        let projectEmptied = projects[projectID] != nil
            && !tabs.values.contains { $0.projectId == projectID }
        if projectEmptied {
            projects.removeValue(forKey: projectID)
        }

        // Reassign the active selection if it pointed at the closed
        // tab (or, when the project went away, at that project).
        var activeChanged = false
        if activeTabID == tabID || (projectEmptied && activeProjectID == projectID) {
            if projectEmptied {
                // Project gone: fall back to another project's first
                // tab, both in DISPLAY ORDER (not dictionary order).
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
            } else {
                // Project survives: fall back to a sibling tab in
                // display order, else any tab anywhere. CR-flagged on
                // PR #78 (display order, not dictionary order).
                let siblingsInProject = tabs.values
                    .filter { $0.projectId == projectID }
                    .sorted { ($0.position, $0.id) < ($1.position, $1.id) }
                let next = siblingsInProject.first
                    ?? tabs.values.sorted { ($0.position, $0.id) < ($1.position, $1.id) }.first
                activeProjectID = next?.projectId ?? projectID
                activeTabID = next?.id ?? 0
            }
            activeChanged = true
        }
        persist()
        emit(.tabClosed(tabID: tabID))
        if projectEmptied {
            emit(.projectDeleted(projectID: projectID))
        }
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
        // Manual rename is deliberate + infrequent — persist now.
        persist()
    }

    /// OSC 0/1/2 title — respects a prior manual rename.
    func setTabTitleFromOSC(_ tabID: Int64, title: String) throws {
        guard var t = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
        if t.userTitled { return }
        t.title = title
        tabs[tabID] = t
        emit(.tabTitleChanged(tabID: tabID, title: title))
        // Shell-driven (OSC titles can fire per prompt) — throttle.
        persistThrottled()
    }

    func setTabCwd(_ tabID: Int64, cwd: String) throws {
        guard var t = tabs[tabID] else { throw WorkspaceError.tabNotFound(tabID) }
        t.cwd = cwd
        tabs[tabID] = t
        emit(.tabCwdChanged(tabID: tabID, cwd: cwd))
        // Shell-driven (OSC 7 fires per `cd`) — throttle so a `cd`
        // loop doesn't fsync per change; the new cwd is in memory and
        // the next layout mutation flushes it.
        persistThrottled()
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
        // Persist the active selection so it survives a relaunch
        // (restored by position). Skip when unchanged.
        if prev != (row.projectId, row.id) {
            persist()
        }
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
        // Active tab restored by position, not id (ids aren't stable
        // across a fresh-shell restore).
        let activeTabPosition = tabs[activeTabID]?.position ?? 0
        let snapshot = SnapshotFile(
            nextID: nextID,
            projects: projects.values
                .sorted { ($0.position, $0.id) < ($1.position, $1.id) }
                .map { p in
                    let projectTabs = tabs.values
                        .filter { $0.projectId == p.id }
                        .sorted { $0.position < $1.position }
                        .map {
                            SnapshotFile.TabSnapshot(
                                title: $0.title,
                                cwd: $0.cwd,
                                position: $0.position
                            )
                        }
                    return SnapshotFile.ProjectSnapshot(
                        id: p.id,
                        name: p.name,
                        cwd: p.cwd,
                        position: p.position,
                        createdAt: p.createdAt,
                        tabs: projectTabs
                    )
                },
            activeProjectID: activeProjectID,
            activeTabPosition: activeTabPosition
        )
        do {
            try Self.write(snapshot: snapshot, to: statePath)
        } catch {
            NSLog("workspace: failed to persist state.json: \(error)")
        }
    }

    /// Persist for chatty shell-driven changes (cwd / OSC title):
    /// at most once per `metaPersistMinInterval` (leading edge). The
    /// in-memory value is always current; the next layout mutation
    /// flushes any throttled change. Mirrors the Rust
    /// `take_throttled_persist`.
    private func persistThrottled() {
        let now = Date()
        if let last = lastMetaPersist, now.timeIntervalSince(last) < Self.metaPersistMinInterval {
            return
        }
        lastMetaPersist = now
        persist()
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
        // Atomic swap via `replaceItemAt`. Avoids the
        // remove-then-move window in which a crash would leave
        // `state.json` missing entirely. CR-flagged on PR #78.
        if FileManager.default.fileExists(atPath: path) {
            _ = try FileManager.default.replaceItemAt(
                url,
                withItemAt: tmp,
                backupItemName: nil,
                options: []
            )
        } else {
            try FileManager.default.moveItem(at: tmp, to: url)
        }
    }

    struct SnapshotFile: Codable, Equatable, Sendable {
        let nextID: Int64
        let projects: [ProjectSnapshot]
        /// Project to re-select on relaunch (`0` = no preference →
        /// first project). Mirrors the Rust `SnapshotFile`.
        let activeProjectID: Int64
        /// Position of the active tab within `activeProjectID`. Tab
        /// ids aren't stable across a fresh-shell restore, so the
        /// selection is restored by position.
        let activeTabPosition: Int32

        init(
            nextID: Int64,
            projects: [ProjectSnapshot],
            activeProjectID: Int64 = 0,
            activeTabPosition: Int32 = 0
        ) {
            self.nextID = nextID
            self.projects = projects
            self.activeProjectID = activeProjectID
            self.activeTabPosition = activeTabPosition
        }

        struct ProjectSnapshot: Codable, Equatable, Sendable {
            let id: Int64
            let name: String
            let cwd: String
            let position: Int32
            let createdAt: Int64
            /// This project's tab layout, in display order. Defaulted
            /// so a file from an older build (no `tabs` key) loads.
            let tabs: [TabSnapshot]

            init(
                id: Int64,
                name: String,
                cwd: String,
                position: Int32,
                createdAt: Int64,
                tabs: [TabSnapshot] = []
            ) {
                self.id = id
                self.name = name
                self.cwd = cwd
                self.position = position
                self.createdAt = createdAt
                self.tabs = tabs
            }

            enum CodingKeys: String, CodingKey {
                case id, name, cwd, position, tabs
                case createdAt = "created_at"
            }

            // Custom decode so a missing `tabs` key (legacy file or
            // the other UI predating tab persistence) defaults to []
            // rather than throwing — matches Rust's `#[serde(default)]`.
            init(from decoder: Decoder) throws {
                let c = try decoder.container(keyedBy: CodingKeys.self)
                id = try c.decode(Int64.self, forKey: .id)
                name = try c.decode(String.self, forKey: .name)
                cwd = try c.decode(String.self, forKey: .cwd)
                position = try c.decode(Int32.self, forKey: .position)
                createdAt = try c.decode(Int64.self, forKey: .createdAt)
                tabs = try c.decodeIfPresent([TabSnapshot].self, forKey: .tabs) ?? []
            }
        }

        /// A persisted tab's layout: enough to re-open a fresh shell
        /// in the right place, but no live state.
        struct TabSnapshot: Codable, Equatable, Sendable {
            let title: String
            let cwd: String
            let position: Int32
        }

        enum CodingKeys: String, CodingKey {
            case nextID = "next_id"
            case projects
            case activeProjectID = "active_project_id"
            case activeTabPosition = "active_tab_position"
        }

        // Custom decode so missing `tabs` / `active_*` keys default
        // instead of throwing (cross-version + legacy compatibility).
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            nextID = try c.decode(Int64.self, forKey: .nextID)
            projects = try c.decodeIfPresent([ProjectSnapshot].self, forKey: .projects) ?? []
            activeProjectID = try c.decodeIfPresent(Int64.self, forKey: .activeProjectID) ?? 0
            activeTabPosition = try c.decodeIfPresent(Int32.self, forKey: .activeTabPosition) ?? 0
        }
    }

    // MARK: Restore layout

    /// A persisted tab's layout, surfaced to the app bootstrap. A
    /// descriptor (cwd + title), not a live tab — the UI re-opens it
    /// as a fresh shell via the normal open path. Mirrors the Rust
    /// `RestoreTab`.
    struct RestoreTab: Sendable, Equatable {
        let cwd: String
        let title: String
    }

    struct RestoreProject: Sendable, Equatable {
        let projectID: Int64
        /// Tabs in display (position) order.
        let tabs: [RestoreTab]
    }

    struct RestoreLayout: Sendable, Equatable {
        let projects: [RestoreProject]
        /// Project to re-select (`0` = no preference → first project).
        let activeProjectID: Int64
        /// Position of the active tab within the active project.
        let activeTabPosition: Int32
    }
}

private func unixNow() -> Int64 {
    Int64(Date().timeIntervalSince1970)
}

private func deriveTitle(cwd: String) -> String {
    if cwd.isEmpty { return "shell" }
    return (cwd as NSString).lastPathComponent.isEmpty ? "shell" : (cwd as NSString).lastPathComponent
}
