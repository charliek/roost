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
        title: String = "",
        activate: Bool = true
    ) throws -> Workspace.Tab {
        let startCwd = Self.spawnCwd(
            requested: cwd,
            projectCwd: workspace.project(projectID)?.cwd ?? "",
            home: homeDirectory(),
            isDirectory: isDirectory
        )
        let tab = try workspace.openTab(
            projectID: projectID,
            cwd: startCwd,
            title: title,
            activate: activate
        )
        do {
            try supervisor.spawn(
                tabID: tab.id,
                cwd: startCwd,
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

    /// Where a shell asked to start in `requested` really starts: there
    /// if it is a directory, else in the project's cwd if that is one,
    /// else `home` — Rust's `application::usable_cwd`. `openTab` resolves
    /// it once and both records it in the row and hands it to the spawn,
    /// because a cwd that is not a directory would otherwise leave the
    /// child in Roost.app's own launch directory while the row names
    /// another (#541). An existing directory the shell cannot enter still
    /// counts, and fails the spawn.
    nonisolated static func spawnCwd(
        requested: String,
        projectCwd: String,
        home: String,
        isDirectory: (String) -> Bool
    ) -> String {
        [requested, projectCwd].first(where: isDirectory) ?? home
    }

    /// `tab.open`'s `project_id: 0`. A new project records the requested
    /// cwd only if it is a directory, and otherwise none, as Rust's does;
    /// its tabs then take the `$HOME` fallback when they spawn.
    @discardableResult
    func ensureDefaultProject(cwd: String, activate: Bool) -> Int64 {
        workspace.ensureDefaultProject(
            cwd: Self.spawnCwd(requested: cwd, projectCwd: "", home: "", isDirectory: isDirectory),
            activate: activate
        )
    }

    /// Where a tab opened from `tabID` starts — `tab.open`'s
    /// `cwd_from_tab`, mirroring Rust's `application::inherited_cwd`.
    func inheritedCwd(tabID: Int64) -> String? {
        guard let row = workspace.tab(tabID) else { return nil }
        return [supervisor.foregroundCwd(tabID: tabID), row.cwd]
            .compactMap { $0 }
            .first(where: isDirectory)
    }

    func closeTab(_ tabID: Int64) throws {
        // Row first, then the PTY, on both arms — the order
        // `roost_engine::application::close_tab` pins (#416). The main
        // actor already serializes this against the exit auto-close, so
        // on this side it is parity, not a fix.
        let removed = Result { try workspace.closeTab(tabID) }
        supervisor.close(tabID: tabID)
        try removed.get()
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

    @discardableResult
    func agentReport(_ report: AgentReport) throws -> (accepted: Bool, tab: Workspace.Tab) {
        try workspace.agentReport(report)
    }

    func clearTabNotification(_ tabID: Int64) throws {
        try workspace.setTabHasNotification(tabID, hasPending: false)
    }

    func focusTab(_ tabID: Int64) throws -> (previousProject: Int64, previousTab: Int64) {
        try workspace.focusTab(tabID)
    }

    /// A `false` return is suppression (plan §3.4 / §3.5), not failure —
    /// only a missing tab throws.
    @discardableResult
    func raiseAttention(
        _ tabID: Int64,
        title: String,
        body: String,
        source: Workspace.AttentionSource
    ) throws -> Bool {
        try workspace.raiseAttention(tabID, title: title, body: body, source: source)
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
            // `rawOsc` is dropped while a live agent session is mid-turn:
            // the agent already reports its own attention through
            // `tab.agent_report`, and a wrapper shell echoing OSC 9 on
            // top of that double-notifies. The gate lives inside
            // `raiseAttention` alongside the mutation (plan §3.4).
            let (title, body) = parseNotificationPayload(command: command, payload: payload)
            _ = try? raiseAttention(tabID, title: title, body: body, source: .rawOsc)
        case 133:
            // OSC 133 prompt/command mark → the shell axis. Never gated:
            // the shell and agent axes are independent, and derivation
            // decides which one the tab shows.
            try? workspace.applyShellMark(tabID, body: payload)
        default:
            // Other OSC commands are ignored — the spec doesn't
            // route them to workspace state.
            break
        }
    }
}

private func isDirectory(_ path: String) -> Bool {
    var isDir: ObjCBool = false
    return FileManager.default.fileExists(atPath: path, isDirectory: &isDir) && isDir.boolValue
}

/// `$HOME` if it is an absolute path, else `/` — Rust's `home_dir` — so a
/// spawn cwd is never empty.
private func homeDirectory() -> String {
    let home = ProcessInfo.processInfo.environment["HOME"] ?? ""
    return home.hasPrefix("/") ? home : "/"
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
