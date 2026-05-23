// IPCHandlerImpl.swift — daemon-removal refactor M4b.
//
// Bridges `IPCServer`'s `IPCHandler` protocol to a shared
// `LocalClient`. Each request hops to `@MainActor` (where the
// workspace + PTY supervisor live) before mutating state; the
// response value goes back over the connection via the
// non-isolated `IPCServer.writeAll` path.
//
// Mirrors the Rust handler in `crates/roost-linux/src/ipc.rs`'s
// `dispatch()` — same op names, same error code mapping, same
// envelope semantics.

import Foundation

actor IPCHandlerImpl: IPCHandler {
    private let client: LocalClient
    private let socketPath: String
    private let appLabel: String
    private let appID: String

    @MainActor
    init(client: LocalClient, socketPath: String, appLabel: String, appID: String) {
        self.client = client
        self.socketPath = socketPath
        self.appLabel = appLabel
        self.appID = appID
    }

    func handle(op: String, params: AnyCodable?) async throws -> AnyCodable? {
        switch op {
        case "identify":
            return try await encodeResult(self.identify())
        case "tab.open":
            return try await encodeResult(self.tabOpen(params: params))
        case "tab.close":
            try await self.tabClose(params: params)
            return AnyCodable([:] as [String: Any])
        case "tab.list":
            return try await encodeResult(self.tabList())
        case "tab.write":
            try await self.tabWrite(params: params)
            return AnyCodable([:] as [String: Any])
        case "tab.resize":
            try await self.tabResize(params: params)
            return AnyCodable([:] as [String: Any])
        case "project.create":
            return try await encodeResult(self.projectCreate(params: params))
        case "project.rename":
            try await self.projectRename(params: params)
            return AnyCodable([:] as [String: Any])
        case "project.delete":
            try await self.projectDelete(params: params)
            return AnyCodable([:] as [String: Any])
        case "tab.reorder":
            try await self.tabReorder(params: params)
            return AnyCodable([:] as [String: Any])
        case "project.reorder":
            try await self.projectReorder(params: params)
            return AnyCodable([:] as [String: Any])
        case "tab.focus":
            return try await encodeResult(self.tabFocus(params: params))
        case "tab.set_title":
            try await self.tabSetTitle(params: params)
            return AnyCodable([:] as [String: Any])
        case "tab.set_state":
            try await self.tabSetState(params: params)
            return AnyCodable([:] as [String: Any])
        case "tab.clear_notification":
            try await self.tabClearNotification(params: params)
            return AnyCodable([:] as [String: Any])
        case "tab.set_hook_active":
            try await self.tabSetHookActive(params: params)
            return AnyCodable([:] as [String: Any])
        case "notification.create":
            try await self.notificationCreate(params: params)
            return AnyCodable([:] as [String: Any])
        case "events.subscribe":
            // Stubbed per spec — no event push from M0/M3a/M4
            // unless a consumer needs it. Replies OK.
            return AnyCodable([:] as [String: Any])
        default:
            throw IPCHandlerError.unknownOp(op)
        }
    }

    // MARK: identify

    @MainActor
    private func identify() async throws -> IPCIdentifyResult {
        return IPCIdentifyResult(
            socketPath: socketPath,
            pid: Int32(ProcessInfo.processInfo.processIdentifier),
            activeProjectID: client.workspace.activeProjectID,
            activeTabID: client.workspace.activeTabID,
            appLabel: appLabel,
            appID: appID,
            uiVersion: Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0.0.0",
            protocolVersion: ipcProtocolVersion
        )
    }

    // MARK: tabs

    @MainActor
    private func tabOpen(params: AnyCodable?) async throws -> IPCTabOpenResult {
        let p = try decodeParams(params, as: IPCTabOpenParams.self)
        var projectID = p.projectID
        if projectID == 0 {
            projectID = client.workspace.ensureDefaultProject(cwd: p.cwd)
        }
        do {
            let tab = try client.openTab(
                projectID: projectID,
                cwd: p.cwd,
                argv: p.argv,
                cols: try ipcDim(p.cols, defaultValue: 80, field: "cols"),
                rows: try ipcDim(p.rows, defaultValue: 24, field: "rows"),
                title: p.title
            )
            return IPCTabOpenResult(tab: tab.toIPC(isActive: true))
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        } catch let err as PtySupervisor.PtyError {
            throw mapPty(err)
        }
    }

    @MainActor
    private func tabClose(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCTabCloseParams.self)
        do {
            try client.closeTab(p.tabID)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabList() async -> IPCTabListResult {
        let projects = client.workspace.snapshot()
        return IPCTabListResult(
            projects: projects.map { p in
                let tabs = client.workspace.tabs(in: p.id)
                return IPCProject(
                    id: p.id,
                    name: p.name,
                    cwd: p.cwd,
                    position: p.position,
                    createdAt: p.createdAt,
                    tabs: tabs.map { t in
                        t.toIPC(isActive: t.id == client.workspace.activeTabID)
                    }
                )
            }
        )
    }

    @MainActor
    private func tabWrite(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCTabWriteParams.self)
        do {
            _ = try client.writeTab(p.tabID, data: p.data)
        } catch let err as PtySupervisor.PtyError {
            throw mapPty(err)
        }
    }

    @MainActor
    private func tabResize(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCTabResizeParams.self)
        let cols = try ipcDim(p.cols, defaultValue: 0, field: "cols")
        let rows = try ipcDim(p.rows, defaultValue: 0, field: "rows")
        do {
            try client.resizeTab(p.tabID, cols: cols, rows: rows)
        } catch let err as PtySupervisor.PtyError {
            throw mapPty(err)
        }
    }

    // MARK: projects

    @MainActor
    private func projectCreate(params: AnyCodable?) async throws -> IPCProjectCreateResult {
        let p = try decodeParams(params, as: IPCProjectCreateParams.self)
        let project = client.createProject(name: p.name, cwd: p.cwd)
        return IPCProjectCreateResult(
            project: IPCProject(
                id: project.id,
                name: project.name,
                cwd: project.cwd,
                position: project.position,
                createdAt: project.createdAt,
                tabs: []
            )
        )
    }

    @MainActor
    private func projectRename(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCProjectRenameParams.self)
        do {
            try client.renameProject(p.projectID, name: p.name)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func projectDelete(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCProjectDeleteParams.self)
        do {
            _ = try client.deleteProject(p.projectID)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabReorder(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCTabReorderParams.self)
        do {
            try client.reorderTabs(projectID: p.projectID, tabIDs: p.tabIDs)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func projectReorder(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCProjectReorderParams.self)
        do {
            try client.reorderProjects(p.projectIDs)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabFocus(params: AnyCodable?) async throws -> IPCTabFocusResult {
        let p = try decodeParams(params, as: IPCTabFocusParams.self)
        do {
            let prev = try client.focusTab(p.tabID)
            return IPCTabFocusResult(
                previousProjectID: prev.previousProject,
                previousTabID: prev.previousTab
            )
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabSetTitle(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCTabSetTitleParams.self)
        do {
            try client.setTabTitle(p.tabID, title: p.title)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabSetState(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCTabSetStateParams.self)
        let state: Workspace.TabState
        switch p.state {
        case .none: state = .none
        case .running: state = .running
        case .needsInput: state = .needsInput
        case .idle: state = .idle
        }
        do {
            try client.setTabState(p.tabID, state: state)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabClearNotification(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCTabClearNotificationParams.self)
        do {
            try client.clearTabNotification(p.tabID)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabSetHookActive(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCTabSetHookActiveParams.self)
        do {
            try client.setTabHookActive(p.tabID, active: p.active)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func notificationCreate(params: AnyCodable?) async throws {
        let p = try decodeParams(params, as: IPCNotificationCreateParams.self)
        do {
            try client.fireNotification(p.tabID, title: p.title, body: p.body)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }
}

// MARK: - Param decoding helpers

private func decodeParams<T: Decodable>(_ params: AnyCodable?, as: T.Type) throws -> T {
    let raw = params?.value ?? [String: Any]()
    do {
        let data = try JSONSerialization.data(withJSONObject: raw, options: [])
        return try JSONDecoder().decode(T.self, from: data)
    } catch {
        throw IPCHandlerError.invalidParam("\(error)")
    }
}

private func encodeResult<T: Encodable>(_ value: T) throws -> AnyCodable? {
    let data = try JSONEncoder().encode(value)
    let any = try JSONSerialization.jsonObject(with: data, options: [])
    return AnyCodable(any)
}

/// Defensive u16 conversion. Zero → `defaultValue`; values
/// exceeding UInt16 throw `invalid-param`. Mirrors the Rust
/// `u16::try_from` validation in `crates/roost-linux/src/ipc.rs`.
private func ipcDim(_ value: UInt32, defaultValue: UInt16, field: String) throws -> UInt16 {
    if value == 0 { return defaultValue }
    if value > UInt32(UInt16.max) {
        throw IPCHandlerError.invalidParam("\(field) out of u16 range: \(value)")
    }
    return UInt16(value)
}

private func mapWorkspace(_ err: Workspace.WorkspaceError) -> IPCHandlerError {
    switch err {
    case .projectNotFound, .tabNotFound:
        return IPCHandlerError.notFound(err.description)
    case .tabProjectMismatch:
        return IPCHandlerError.invalidParam(err.description)
    }
}

private func mapPty(_ err: PtySupervisor.PtyError) -> IPCHandlerError {
    switch err {
    case .notFound, .writeFailed:
        return IPCHandlerError.notFound(err.description)
    case .forkpty, .ttySize:
        return IPCHandlerError.internalError(err.description)
    case .duplicateTab:
        return IPCHandlerError.invalidParam(err.description)
    }
}

// MARK: - Per-op param + result structs (snake_case CodingKeys to
// match `crates/roost-ipc/src/messages.rs`)

private struct IPCTabOpenParams: Codable, Sendable {
    var projectID: Int64 = 0
    var cwd: String = ""
    var argv: [String] = []
    var cols: UInt32 = 0
    var rows: UInt32 = 0
    var title: String = ""

    enum CodingKeys: String, CodingKey {
        case projectID = "project_id"
        case cwd, argv, cols, rows, title
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        if let pid = try c.decodeIfPresent(String.self, forKey: .projectID) {
            guard let v = Int64(pid) else {
                throw DecodingError.dataCorruptedError(
                    forKey: .projectID, in: c,
                    debugDescription: "project_id must be a string-wrapped int64"
                )
            }
            self.projectID = v
        }
        self.cwd = try c.decodeIfPresent(String.self, forKey: .cwd) ?? ""
        self.argv = try c.decodeIfPresent([String].self, forKey: .argv) ?? []
        self.cols = try c.decodeIfPresent(UInt32.self, forKey: .cols) ?? 0
        self.rows = try c.decodeIfPresent(UInt32.self, forKey: .rows) ?? 0
        self.title = try c.decodeIfPresent(String.self, forKey: .title) ?? ""
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(projectID), forKey: .projectID)
        try c.encode(cwd, forKey: .cwd)
        try c.encode(argv, forKey: .argv)
        try c.encode(cols, forKey: .cols)
        try c.encode(rows, forKey: .rows)
        try c.encode(title, forKey: .title)
    }
}

private struct IPCTabOpenResult: Codable {
    let tab: IPCTab
}

private struct IPCTabCloseParams: Codable {
    let tabID: Int64
    enum CodingKeys: String, CodingKey { case tabID = "tab_id" }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c, debugDescription: "tab_id must be string int64"
            )
        }
        self.tabID = v
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
    }
}

private struct IPCTabListResult: Codable {
    let projects: [IPCProject]
}

private struct IPCTabWriteParams: Codable {
    let tabID: Int64
    let data: Data
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case data
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c, debugDescription: "tab_id must be string int64"
            )
        }
        self.tabID = v
        let b64 = try c.decode(String.self, forKey: .data)
        guard let d = Data(base64Encoded: b64) else {
            throw DecodingError.dataCorruptedError(
                forKey: .data, in: c, debugDescription: "data must be base64"
            )
        }
        self.data = d
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
        try c.encode(data.base64EncodedString(), forKey: .data)
    }
}

private struct IPCTabResizeParams: Codable {
    let tabID: Int64
    let cols: UInt32
    let rows: UInt32
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case cols, rows
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c, debugDescription: "tab_id must be string int64"
            )
        }
        self.tabID = v
        self.cols = try c.decode(UInt32.self, forKey: .cols)
        self.rows = try c.decode(UInt32.self, forKey: .rows)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
        try c.encode(cols, forKey: .cols)
        try c.encode(rows, forKey: .rows)
    }
}

private struct IPCProjectCreateParams: Codable {
    var name: String = ""
    var cwd: String = ""
}

private struct IPCProjectCreateResult: Codable {
    let project: IPCProject
}

private struct IPCProjectRenameParams: Codable {
    let projectID: Int64
    let name: String
    enum CodingKeys: String, CodingKey {
        case projectID = "project_id"
        case name
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .projectID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .projectID, in: c, debugDescription: "project_id must be string int64"
            )
        }
        self.projectID = v
        self.name = try c.decode(String.self, forKey: .name)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(projectID), forKey: .projectID)
        try c.encode(name, forKey: .name)
    }
}

private struct IPCProjectDeleteParams: Codable {
    let projectID: Int64
    enum CodingKeys: String, CodingKey { case projectID = "project_id" }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .projectID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .projectID, in: c, debugDescription: "project_id must be string int64"
            )
        }
        self.projectID = v
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(projectID), forKey: .projectID)
    }
}

private struct IPCTabReorderParams: Codable {
    let projectID: Int64
    let tabIDs: [Int64]
    enum CodingKeys: String, CodingKey {
        case projectID = "project_id"
        case tabIDs = "tab_ids"
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .projectID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .projectID, in: c, debugDescription: "project_id must be string int64"
            )
        }
        self.projectID = v
        let rawIDs = try c.decode([String].self, forKey: .tabIDs)
        self.tabIDs = try rawIDs.map { s in
            guard let v = Int64(s) else {
                throw DecodingError.dataCorruptedError(
                    forKey: .tabIDs, in: c, debugDescription: "tab_ids must be string int64s"
                )
            }
            return v
        }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(projectID), forKey: .projectID)
        try c.encode(tabIDs.map { String($0) }, forKey: .tabIDs)
    }
}

private struct IPCProjectReorderParams: Codable {
    let projectIDs: [Int64]
    enum CodingKeys: String, CodingKey { case projectIDs = "project_ids" }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let rawIDs = try c.decode([String].self, forKey: .projectIDs)
        self.projectIDs = try rawIDs.map { s in
            guard let v = Int64(s) else {
                throw DecodingError.dataCorruptedError(
                    forKey: .projectIDs, in: c, debugDescription: "project_ids must be string int64s"
                )
            }
            return v
        }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(projectIDs.map { String($0) }, forKey: .projectIDs)
    }
}

private struct IPCTabFocusParams: Codable {
    let tabID: Int64
    enum CodingKeys: String, CodingKey { case tabID = "tab_id" }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c, debugDescription: "tab_id must be string int64"
            )
        }
        self.tabID = v
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
    }
}

private struct IPCTabFocusResult: Codable {
    let previousProjectID: Int64
    let previousTabID: Int64
    enum CodingKeys: String, CodingKey {
        case previousProjectID = "previous_project_id"
        case previousTabID = "previous_tab_id"
    }
    init(previousProjectID: Int64, previousTabID: Int64) {
        self.previousProjectID = previousProjectID
        self.previousTabID = previousTabID
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.previousProjectID = Int64(try c.decode(String.self, forKey: .previousProjectID)) ?? 0
        self.previousTabID = Int64(try c.decode(String.self, forKey: .previousTabID)) ?? 0
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(previousProjectID), forKey: .previousProjectID)
        try c.encode(String(previousTabID), forKey: .previousTabID)
    }
}

private struct IPCTabSetTitleParams: Codable {
    let tabID: Int64
    let title: String
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case title
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c, debugDescription: "tab_id must be string int64"
            )
        }
        self.tabID = v
        self.title = try c.decode(String.self, forKey: .title)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
        try c.encode(title, forKey: .title)
    }
}

private struct IPCTabSetStateParams: Codable {
    let tabID: Int64
    let state: IPCTabState
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case state
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c, debugDescription: "tab_id must be string int64"
            )
        }
        self.tabID = v
        self.state = try c.decode(IPCTabState.self, forKey: .state)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
        try c.encode(state, forKey: .state)
    }
}

private struct IPCTabClearNotificationParams: Codable {
    let tabID: Int64
    enum CodingKeys: String, CodingKey { case tabID = "tab_id" }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c, debugDescription: "tab_id must be string int64"
            )
        }
        self.tabID = v
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
    }
}

private struct IPCTabSetHookActiveParams: Codable {
    let tabID: Int64
    let active: Bool
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case active
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c, debugDescription: "tab_id must be string int64"
            )
        }
        self.tabID = v
        self.active = try c.decode(Bool.self, forKey: .active)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
        try c.encode(active, forKey: .active)
    }
}

private struct IPCNotificationCreateParams: Codable {
    let tabID: Int64
    let title: String
    let body: String
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case title, body
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c, debugDescription: "tab_id must be string int64"
            )
        }
        self.tabID = v
        self.title = try c.decode(String.self, forKey: .title)
        self.body = try c.decodeIfPresent(String.self, forKey: .body) ?? ""
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
        try c.encode(title, forKey: .title)
        try c.encode(body, forKey: .body)
    }
}

// MARK: - Workspace → IPC conversions

extension Workspace.Tab {
    func toIPC(isActive: Bool) -> IPCTab {
        IPCTab(
            id: id,
            projectID: projectId,
            title: title,
            cwd: cwd,
            state: state.toIPC(),
            hasNotification: hasNotification,
            isActive: isActive,
            userTitled: userTitled,
            position: position,
            createdAt: createdAt,
            lastActive: lastActive,
            hookActive: hookActive
        )
    }
}

extension Workspace.TabState {
    func toIPC() -> IPCTabState {
        switch self {
        case .none: return .none
        case .running: return .running
        case .needsInput: return .needsInput
        case .idle: return .idle
        }
    }
}
