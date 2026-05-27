// LocalClient.swift — daemon-removal refactor M4b.
//
// In-process adapter that the App will consume in M4b3 instead of
// `RoostClient` (gRPC). Wraps a shared `Workspace` + `PtySupervisor`
// + the IPC socket path so the same handles drive both the UI's
// state mutations and the IPC server's dispatch.
//
// Methods mirror `RoostClient`'s shape so the M4b3 rewire is a
// thin call-site rename per closure. Throws Swift-native errors
// (`Workspace.WorkspaceError`, `PtySupervisor.PtyError`) instead
// of gRPC `RPCError`s.

import Foundation

@MainActor
final class LocalClient {
    let workspace: Workspace
    let supervisor: PtySupervisor
    let socketPath: String

    init(workspace: Workspace, supervisor: PtySupervisor, socketPath: String) {
        self.workspace = workspace
        self.supervisor = supervisor
        self.socketPath = socketPath
    }

    // MARK: Projects

    func listProjects() -> [Workspace.Project] {
        workspace.snapshot()
    }

    @discardableResult
    func createProject(name: String, cwd: String) -> Workspace.Project {
        workspace.createProject(name: name, cwd: cwd)
    }

    func renameProject(_ projectID: Int64, name: String) throws {
        try workspace.renameProject(projectID, name: name)
    }

    /// Deletes the project and reaps every cascaded PTY. Returns
    /// the cascaded tab ids.
    @discardableResult
    func deleteProject(_ projectID: Int64) throws -> [Int64] {
        let cascaded = try workspace.deleteProject(projectID)
        for tabID in cascaded {
            supervisor.close(tabID: tabID)
        }
        return cascaded
    }

    func reorderProjects(_ projectIDs: [Int64]) throws {
        try workspace.reorderProjects(projectIDs)
    }

    func reorderTabs(projectID: Int64, tabIDs: [Int64]) throws {
        try workspace.reorderTabs(projectID: projectID, tabIDs: tabIDs)
    }

    // MARK: Tabs

    /// Open a tab and spawn the shell. The workspace records the
    /// tab first (which fires `tabOpened`); the supervisor then
    /// allocates the PTY. On supervisor failure the workspace tab
    /// is rolled back (fires `tabClosed`) and the error is
    /// rethrown.
    @discardableResult
    func openTab(
        projectID: Int64,
        cwd: String,
        argv: [String] = [],
        cols: UInt16 = 80,
        rows: UInt16 = 24,
        title: String = ""
    ) throws -> Workspace.Tab {
        // Resolve the starting cwd: caller-supplied → project's cwd
        // → $HOME. Ensures `roostctl tab open --project-id N`
        // (which omits --cwd) lands in the project's directory or
        // at minimum the user's home, not Finder's `/`. The Mac UI
        // already does the same fallback in `openNewTab`; this
        // covers the IPC entry point (which `roostctl` uses).
        let resolvedCwd: String
        if !cwd.isEmpty {
            resolvedCwd = cwd
        } else if let project = workspace.snapshot().first(where: { $0.id == projectID }),
                  !project.cwd.isEmpty {
            resolvedCwd = project.cwd
        } else {
            resolvedCwd = ProcessInfo.processInfo.environment["HOME"] ?? ""
        }

        let tab = try workspace.openTab(
            projectID: projectID,
            cwd: resolvedCwd,
            title: title
        )
        do {
            try supervisor.spawn(
                tabID: tab.id,
                cwd: resolvedCwd,
                argv: argv,
                cols: cols,
                rows: rows,
                socketPath: socketPath
            )
        } catch {
            try? workspace.closeTab(tab.id)
            throw error
        }
        return tab
    }

    func closeTab(_ tabID: Int64) throws {
        supervisor.close(tabID: tabID)
        try workspace.closeTab(tabID)
    }

    func setTabTitle(_ tabID: Int64, title: String) throws {
        try workspace.setTabTitle(tabID, title: title)
    }

    func setTabState(_ tabID: Int64, state: Workspace.TabState) throws {
        try workspace.setTabState(tabID, state: state)
    }

    func setTabHookActive(_ tabID: Int64, active: Bool) throws {
        try workspace.setTabHookActive(tabID, active: active)
    }

    func clearTabNotification(_ tabID: Int64) throws {
        try workspace.setTabHasNotification(tabID, hasPending: false)
    }

    func focusTab(_ tabID: Int64) throws -> (previousProject: Int64, previousTab: Int64) {
        try workspace.focusTab(tabID)
    }

    func fireNotification(_ tabID: Int64, title: String, body: String) throws {
        try workspace.setTabHasNotification(tabID, hasPending: true)
        try workspace.fireNotification(tabID, title: title, body: body)
    }

    // MARK: PTY I/O

    @discardableResult
    func writeTab(_ tabID: Int64, data: Data) throws -> Int {
        try supervisor.write(tabID: tabID, data: data)
    }

    func resizeTab(_ tabID: Int64, cols: UInt16, rows: UInt16) throws {
        try supervisor.resize(tabID: tabID, cols: cols, rows: rows)
    }

    // MARK: OSC routing

    /// Apply an OSC sequence directly to the workspace. Replaces
    /// the daemon-era `ReportOsc` round-trip. Called by the
    /// renderer's OSC callback in M4b3.
    func applyOSC(tabID: Int64, command: UInt32, payload: String) {
        switch command {
        case 0, 1, 2:
            // Shell-set title; respects a prior manual rename.
            try? workspace.setTabTitleFromOSC(tabID, title: payload)
        case 7:
            // OSC 7: cwd. The OSC scanner has already decoded
            // `file://host/path` → `/path` (see `OscEvent.asReport`'s
            // `.pwd` branch in `OscScanner.swift`), so `payload`
            // here is already a plain path. The earlier `if let
            // path = parseOSC7Path(payload)` re-parse expected
            // `file://...` and silently dropped the event because
            // the path no longer had that prefix. Pass through
            // verbatim — but defensively re-run `parseOSC7Path`
            // for the (theoretical) case where some external IPC
            // caller sends an unparsed URI through `tab.set_cwd`
            // or similar; the helper is idempotent on already-
            // parsed paths via the nil-on-no-scheme guard.
            let path = parseOSC7Path(payload) ?? payload
            if !path.isEmpty {
                try? workspace.setTabCwd(tabID, cwd: path)
            }
        case 9, 99, 777:
            let (title, body) = parseNotificationPayload(command: command, payload: payload)
            try? workspace.setTabHasNotification(tabID, hasPending: true)
            try? workspace.fireNotification(tabID, title: title, body: body)
        case 133:
            // OSC 133 prompt/command mark → run state. Suppressed when a
            // Claude hook owns the tab (setTabStateFromOSC gates on
            // hookActive).
            if let state = commandMarkState(payload) {
                try? workspace.setTabStateFromOSC(tabID, state: state)
            }
        default:
            // Other OSC commands are ignored — the spec doesn't
            // route them to workspace state.
            break
        }
    }
}

/// Map an OSC 133 mark body to a run state: `C` (command start) →
/// running; `A`/`B`/`D` (prompt / command end) → none (clear the dot);
/// other bodies → nil (no change). Only the first char matters, so
/// `D;<exit>` keeps the exit code we ignore.
func commandMarkState(_ body: String) -> Workspace.TabState? {
    guard let mark = body.first else { return nil }
    switch mark {
    case "C": return .running
    case "A", "B", "D": return Workspace.TabState.none
    default: return nil
    }
}

/// Strip the `file://` scheme + host segment from an OSC 7
/// payload. `file://host/path` → `/path`; `file:///abs` →
/// `/abs`; `file://hostonly` → nil (no path component, so we
/// don't overwrite the workspace cwd with a host token).
func parseOSC7Path(_ payload: String) -> String? {
    guard payload.hasPrefix("file://") else { return nil }
    let afterScheme = String(payload.dropFirst("file://".count))
    guard let slashIndex = afterScheme.firstIndex(of: "/") else { return nil }
    return String(afterScheme[slashIndex...])
}

/// Split an OSC 9 / 99 / 777 notification payload into
/// (title, body). OSC 777 strips a leading `notify;` then splits
/// title/body on the next `;`. OSC 9 / 99 use the entire payload
/// as the title.
func parseNotificationPayload(command: UInt32, payload: String) -> (String, String) {
    if command == 777 {
        let trimmed: String
        if payload.hasPrefix("notify;") {
            trimmed = String(payload.dropFirst("notify;".count))
        } else {
            trimmed = payload
        }
        if let sep = trimmed.firstIndex(of: ";") {
            return (
                String(trimmed[..<sep]),
                String(trimmed[trimmed.index(after: sep)...])
            )
        }
        return (trimmed, "")
    }
    return (payload, "")
}
