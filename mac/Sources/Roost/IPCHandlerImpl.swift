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

import AppKit
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
            // `identify` params are optional — just validate that
            // any provided keys are part of the documented set.
            _ = try decodeParams(
                params,
                as: IPCIdentifyParams.self,
                expected: ["client_name", "client_version"]
            )
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
        case "tab.dump":
            return try await encodeResult(self.tabDump(params: params))
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
        case "app.screenshot":
            return try await encodeResult(self.screenshotCapture(params: params))
        case "palette.open":
            return try await encodeResult(self.paletteOpen(params: params))
        case "palette.state":
            return try await encodeResult(self.paletteState(params: params))
        case "palette.query":
            return try await encodeResult(self.paletteQuery(params: params))
        case "palette.activate":
            return try await encodeResult(self.paletteActivate(params: params))
        case "palette.dismiss":
            return try await encodeResult(self.paletteDismiss(params: params))
        case "selection.set":
            try await self.selectionSet(params: params)
            return AnyCodable([:] as [String: Any])
        case "selection.clear":
            try await self.selectionClear(params: params)
            return AnyCodable([:] as [String: Any])
        case "selection.dump":
            return try await encodeResult(self.selectionDump(params: params))
        case "clipboard.dump":
            return try await encodeResult(self.clipboardDump(params: params))
        case "clipboard.write":
            try await self.clipboardWrite(params: params)
            return AnyCodable([:] as [String: Any])
        case "tab.feed_pty_bytes":
            try await self.tabFeedPtyBytes(params: params)
            return AnyCodable([:] as [String: Any])
        case "tab.capture_pty_input":
            return try await encodeResult(self.tabCapturePtyInput(params: params))
        case "tab.dump_resolved":
            return try await encodeResult(self.tabDumpResolved(params: params))
        case "events.subscribe":
            // Honest failure rather than a false ACK: the server never
            // pushes events on the connection yet, so a client that
            // "subscribed" would wait forever. Surface not-implemented
            // so it can fall back (e.g. poll tab.list). Mirrors the
            // Rust handler; real streaming lands with its first
            // consumer. (#9)
            throw IPCHandlerError(
                code: "not-implemented",
                message: "events.subscribe is not yet implemented"
            )
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
        let p = try decodeParams(
            params, as: IPCTabOpenParams.self,
            expected: ["project_id", "cwd", "argv", "cols", "rows", "title"]
        )
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
        let p = try decodeParams(
            params, as: IPCTabCloseParams.self, expected: ["tab_id"]
        )
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
        let p = try decodeParams(
            params, as: IPCTabWriteParams.self, expected: ["tab_id", "data"]
        )
        do {
            _ = try client.writeTab(p.tabID, data: p.data)
        } catch let err as PtySupervisor.PtyError {
            throw mapPty(err)
        }
    }

    @MainActor
    private func tabResize(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCTabResizeParams.self,
            expected: ["tab_id", "cols", "rows"]
        )
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
        let p = try decodeParams(
            params, as: IPCProjectCreateParams.self, expected: ["name", "cwd"]
        )
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
        let p = try decodeParams(
            params, as: IPCProjectRenameParams.self,
            expected: ["project_id", "name"]
        )
        do {
            try client.renameProject(p.projectID, name: p.name)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func projectDelete(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCProjectDeleteParams.self, expected: ["project_id"]
        )
        do {
            _ = try client.deleteProject(p.projectID)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabReorder(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCTabReorderParams.self,
            expected: ["project_id", "tab_ids"]
        )
        do {
            try client.reorderTabs(projectID: p.projectID, tabIDs: p.tabIDs)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func projectReorder(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCProjectReorderParams.self,
            expected: ["project_ids"]
        )
        do {
            try client.reorderProjects(p.projectIDs)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabFocus(params: AnyCodable?) async throws -> IPCTabFocusResult {
        let p = try decodeParams(
            params, as: IPCTabFocusParams.self, expected: ["tab_id"]
        )
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
        let p = try decodeParams(
            params, as: IPCTabSetTitleParams.self,
            expected: ["tab_id", "title"]
        )
        do {
            try client.setTabTitle(p.tabID, title: p.title)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabSetState(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCTabSetStateParams.self,
            expected: ["tab_id", "state"]
        )
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
        let p = try decodeParams(
            params, as: IPCTabClearNotificationParams.self,
            expected: ["tab_id"]
        )
        do {
            try client.clearTabNotification(p.tabID)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func tabSetHookActive(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCTabSetHookActiveParams.self,
            expected: ["tab_id", "active"]
        )
        do {
            try client.setTabHookActive(p.tabID, active: p.active)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    @MainActor
    private func notificationCreate(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCNotificationCreateParams.self,
            expected: ["tab_id", "title", "body"]
        )
        do {
            try client.fireNotification(p.tabID, title: p.title, body: p.body)
        } catch let err as Workspace.WorkspaceError {
            throw mapWorkspace(err)
        }
    }

    // MARK: tab dump

    @MainActor
    private func tabDump(params: AnyCodable?) async throws -> IPCTabDumpResult {
        let p = try decodeParams(
            params, as: IPCTabDumpParams.self, expected: ["tab_id"]
        )
        // Reach the running UI through the one registered bridge (the
        // handler is already on the main actor).
        guard let ui = RoostBackend.shared.ui else {
            throw IPCHandlerError.internalError("no UI to read terminal")
        }
        guard let dump = ui.dumpTab(tabID: p.tabID) else {
            throw IPCHandlerError(
                code: "not-found",
                message: "tab \(p.tabID) has no live terminal"
            )
        }
        return IPCTabDumpResult(
            cols: dump.cols,
            rows: dump.rows,
            cursor: dump.cursor.map {
                IPCTabDumpCursor(row: $0.row, col: $0.col, visible: $0.visible)
            },
            rowsText: dump.rowsText
        )
    }

    // MARK: command palette (palette.* ops)
    //
    // UI-only ops: routed through the registered `UiBridge`, not the
    // workspace — the palette is overlay state, not persisted state.
    // Each returns the resulting `PaletteStateResult` so a driver needs
    // no follow-up `palette.state`.

    @MainActor
    private func paletteOpen(params: AnyCodable?) async throws -> IPCPaletteStateResult {
        let p = try decodeParams(params, as: IPCPaletteOpenParams.self, expected: ["kind"])
        let kind = p.kind ?? ""
        guard ["", "commands", "launcher"].contains(kind) else {
            throw IPCHandlerError.invalidParam(
                "unknown palette kind \"\(kind)\" (want \"commands\" or \"launcher\")")
        }
        let ui = try paletteUI()
        return IPCPaletteStateResult(ui.openPalette(kind: kind))
    }

    @MainActor
    private func paletteState(params: AnyCodable?) async throws -> IPCPaletteStateResult {
        _ = try decodeParams(params, as: IPCEmptyParams.self, expected: [])
        return IPCPaletteStateResult(try paletteUI().paletteState())
    }

    @MainActor
    private func paletteQuery(params: AnyCodable?) async throws -> IPCPaletteStateResult {
        let p = try decodeParams(params, as: IPCPaletteQueryParams.self, expected: ["query"])
        return IPCPaletteStateResult(try paletteUI().paletteQuery(p.query))
    }

    @MainActor
    private func paletteActivate(params: AnyCodable?) async throws -> IPCPaletteStateResult {
        let p = try decodeParams(params, as: IPCPaletteActivateParams.self, expected: ["id"])
        guard let snap = try paletteUI().paletteActivate(id: p.id) else {
            throw IPCHandlerError(
                code: "not-found",
                message: "no palette open, or no row with id \"\(p.id)\"")
        }
        return IPCPaletteStateResult(snap)
    }

    @MainActor
    private func paletteDismiss(params: AnyCodable?) async throws -> IPCPaletteStateResult {
        _ = try decodeParams(params, as: IPCEmptyParams.self, expected: [])
        return IPCPaletteStateResult(try paletteUI().dismissPaletteOverlay())
    }

    /// The registered UI bridge, or `internal` if none (headless).
    @MainActor
    private func paletteUI() throws -> any UiBridge {
        guard let ui = RoostBackend.shared.ui else {
            throw IPCHandlerError.internalError("no UI for palette")
        }
        return ui
    }

    // MARK: selection + clipboard test ops

    @MainActor
    private func selectionSet(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCSelectionSetParams.self,
            expected: ["tab_id", "anchor", "cursor"]
        )
        guard let ui = RoostBackend.shared.ui else {
            throw IPCHandlerError.internalError("no UI to drive selection")
        }
        let ok = ui.setTabSelection(
            tabID: p.tabID,
            anchorCol: Int(p.anchor.col),
            anchorRow: Int(p.anchor.row),
            cursorCol: Int(p.cursor.col),
            cursorRow: Int(p.cursor.row)
        )
        if !ok {
            throw IPCHandlerError(
                code: "not-found",
                message: "tab \(p.tabID) has no live terminal"
            )
        }
    }

    @MainActor
    private func selectionClear(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCSelectionClearParams.self, expected: ["tab_id"]
        )
        guard let ui = RoostBackend.shared.ui else {
            throw IPCHandlerError.internalError("no UI to drive selection")
        }
        if !ui.clearTabSelection(tabID: p.tabID) {
            throw IPCHandlerError(
                code: "not-found",
                message: "tab \(p.tabID) has no live terminal"
            )
        }
    }

    @MainActor
    private func selectionDump(params: AnyCodable?) async throws -> IPCSelectionDumpResult {
        let p = try decodeParams(
            params, as: IPCSelectionDumpParams.self, expected: ["tab_id"]
        )
        guard let ui = RoostBackend.shared.ui else {
            throw IPCHandlerError.internalError("no UI to read selection")
        }
        guard let outer = ui.dumpTabSelection(tabID: p.tabID) else {
            throw IPCHandlerError(
                code: "not-found",
                message: "tab \(p.tabID) has no live terminal"
            )
        }
        if let dump = outer {
            return IPCSelectionDumpResult(
                text: dump.text,
                anchorVisible: dump.anchorVisible,
                cursorVisible: dump.cursorVisible
            )
        }
        return IPCSelectionDumpResult(
            text: nil, anchorVisible: false, cursorVisible: false
        )
    }

    @MainActor
    private func clipboardDump(params: AnyCodable?) async throws -> IPCClipboardDumpResult {
        let p = try decodeParams(
            params, as: IPCClipboardDumpParams.self, expected: ["target"]
        )
        let pb = try resolvePasteboard(target: p.target)
        return IPCClipboardDumpResult(text: pb.string(forType: .string))
    }

    @MainActor
    private func clipboardWrite(params: AnyCodable?) async throws {
        let p = try decodeParams(
            params, as: IPCClipboardWriteParams.self, expected: ["target", "text"]
        )
        let pb = try resolvePasteboard(target: p.target)
        pb.clearContents()
        pb.setString(p.text, forType: .string)
    }

    // MARK: test-only ops (ROOST_TEST_MODE=1)
    //
    // `tab.feed_pty_bytes` + `tab.capture_pty_input` are gated by
    // `RoostBackend.shared.testMode` (which mirrors `ROOST_TEST_MODE=1`
    // at launch). Without the env var the handlers return
    // `not-enabled` so a user-local script can't drive PTY output
    // into a live tab or read its keystrokes. `tab.dump_resolved`
    // is intentionally ungated — it's a richer read of existing
    // render state.

    @MainActor
    private func tabFeedPtyBytes(params: AnyCodable?) async throws {
        guard RoostBackend.shared.testMode else {
            throw IPCHandlerError(
                code: "not-enabled",
                message: "tab.feed_pty_bytes requires ROOST_TEST_MODE=1 at UI launch"
            )
        }
        let p = try decodeParams(
            params, as: IPCTabFeedPtyBytesParams.self, expected: ["tab_id", "data"]
        )
        guard let ui = RoostBackend.shared.ui else {
            throw IPCHandlerError.internalError("no UI to drive tab.feed_pty_bytes")
        }
        if !ui.feedTabPtyBytes(tabID: p.tabID, data: p.data) {
            throw IPCHandlerError(
                code: "not-found",
                message: "tab \(p.tabID) has no live terminal"
            )
        }
    }

    @MainActor
    private func tabCapturePtyInput(
        params: AnyCodable?
    ) async throws -> IPCTabCapturePtyInputResult {
        guard RoostBackend.shared.testMode else {
            throw IPCHandlerError(
                code: "not-enabled",
                message: "tab.capture_pty_input requires ROOST_TEST_MODE=1 at UI launch"
            )
        }
        let p = try decodeParams(
            params, as: IPCTabCapturePtyInputParams.self, expected: ["tab_id", "drain"]
        )
        // First confirm the tab actually exists (`not-found`
        // signaling). If the capture buffer simply hasn't been
        // allocated yet — no onKey traffic since open — return
        // empty bytes, not `not-found`. This matches the
        // `drain=true` contract where two back-to-back calls
        // produce a non-empty + empty response.
        guard let ui = RoostBackend.shared.ui,
              ui.dumpTab(tabID: p.tabID) != nil
        else {
            throw IPCHandlerError(
                code: "not-found",
                message: "tab \(p.tabID) has no live terminal"
            )
        }
        let bytes =
            RoostBackend.shared.readInputCapture(tabID: p.tabID, drain: p.drain) ?? Data()
        return IPCTabCapturePtyInputResult(data: bytes)
    }

    @MainActor
    private func tabDumpResolved(
        params: AnyCodable?
    ) async throws -> IPCTabDumpResolvedResult {
        let p = try decodeParams(
            params, as: IPCTabDumpResolvedParams.self, expected: ["tab_id"]
        )
        guard let ui = RoostBackend.shared.ui else {
            throw IPCHandlerError.internalError("no UI to read tab.dump_resolved")
        }
        guard let dump = ui.dumpResolvedCells(tabID: p.tabID) else {
            throw IPCHandlerError(
                code: "not-found",
                message: "tab \(p.tabID) has no live terminal"
            )
        }
        return IPCTabDumpResolvedResult(
            cols: UInt16(dump.cols),
            rows: UInt16(dump.rows),
            cells: dump.cells.map { c in
                IPCResolvedCell(
                    row: UInt32(c.row),
                    col: UInt16(c.col),
                    text: c.text,
                    fg: hexFromNSColor(c.foreground),
                    bg: hexFromNSColor(c.background),
                    hasExplicitBg: c.hasExplicitBg,
                    bold: c.bold,
                    italic: c.italic,
                    inverse: c.inverse
                )
            }
        )
    }

    /// Map the wire `target` string to the matching `NSPasteboard`.
    /// "system" → `NSPasteboard.general` (the ⌘V target). "selection"
    /// → the custom named pasteboard shared with `TerminalView` for
    /// drag-selection (`copy-on-select = .on`). Unknown values are
    /// `invalid-param` so a typo doesn't silently fall through.
    @MainActor
    private func resolvePasteboard(target: String) throws -> NSPasteboard {
        switch target {
        case "system":
            return NSPasteboard.general
        case "selection":
            return TerminalView.selectionPasteboard
        default:
            throw IPCHandlerError(
                code: "invalid-param",
                message: "clipboard target must be \"system\" or \"selection\" (got \"\(target)\")"
            )
        }
    }

    // MARK: screenshot

    /// Render the whole window (sidebar + tab bar + active terminal)
    /// to a PNG in-process. `cacheDisplay(in:to:)` re-invokes each
    /// view's `draw(_:)` into an off-screen bitmap, so this works on
    /// the non-layer-backed `TerminalView` and regardless of whether
    /// the window is focused, occluded, or offscreen — the whole point
    /// versus OS screen capture.
    @MainActor
    private func screenshotCapture(params: AnyCodable?) async throws -> IPCScreenshotResult {
        let p = try decodeParams(
            params, as: IPCScreenshotParams.self, expected: ["scale"]
        )
        guard (1...2).contains(p.scale) else {
            throw IPCHandlerError.invalidParam("scale must be 1 or 2, got \(p.scale)")
        }
        guard let window = RoostBackend.shared.ui?.mainWindow else {
            throw IPCHandlerError.internalError("no UI window to capture")
        }
        if window.isMiniaturized {
            throw IPCHandlerError.internalError("window is minimized; cannot capture")
        }
        guard let contentView = window.contentView else {
            throw IPCHandlerError.internalError("window has no content view")
        }
        let bounds = contentView.bounds
        let pixelsWide = Int((bounds.width * CGFloat(p.scale)).rounded())
        let pixelsHigh = Int((bounds.height * CGFloat(p.scale)).rounded())
        guard pixelsWide > 0, pixelsHigh > 0 else {
            throw IPCHandlerError.internalError("window has zero size")
        }
        guard
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil,
                pixelsWide: pixelsWide,
                pixelsHigh: pixelsHigh,
                bitsPerSample: 8,
                samplesPerPixel: 4,
                hasAlpha: true,
                isPlanar: false,
                colorSpaceName: .deviceRGB,
                bytesPerRow: 0,
                bitsPerPixel: 0
            )
        else {
            throw IPCHandlerError.internalError("failed to allocate bitmap rep")
        }
        // A point-sized rep over a pixel-sized buffer makes AppKit
        // super-sample `draw(_:)` across the larger grid — this is the
        // supported lever for crisp 2x (the no-arg
        // `bitmapImageRepForCachingDisplay` would render at 1x points).
        rep.size = bounds.size
        // Draw under the window's appearance so the dark chrome
        // resolves correctly even though we render off-screen.
        window.effectiveAppearance.performAsCurrentDrawingAppearance {
            contentView.cacheDisplay(in: bounds, to: rep)
        }
        guard let png = rep.representation(using: .png, properties: [:]) else {
            throw IPCHandlerError.internalError("PNG encoding failed")
        }
        // Preflight the 16 MiB IPC frame cap: the response rides one
        // newline-delimited JSON frame and `png` dominates it once
        // base64-expanded (~4/3). Fail with a structured error here
        // rather than writing an oversized frame the client rejects.
        let encodedLen = (png.count + 2) / 3 * 4
        if encodedLen + 1024 > ipcMaxFrameBytes {
            throw IPCHandlerError.internalError(
                "screenshot too large: \(encodedLen) base64 bytes exceeds the "
                    + "\(ipcMaxFrameBytes) byte IPC frame cap (try --scale 1)"
            )
        }
        return IPCScreenshotResult(
            png: png,
            width: UInt32(pixelsWide),
            height: UInt32(pixelsHigh),
            scale: p.scale
        )
    }
}

// MARK: - Param decoding helpers

private func decodeParams<T: Decodable>(
    _ params: AnyCodable?,
    as: T.Type,
    expected: Set<String>
) throws -> T {
    let raw = params?.value ?? [String: Any]()
    // Strict server policy (matches `roost-ipc::messages`'s
    // `#[serde(deny_unknown_fields)]` on every request struct):
    // reject params containing fields the op doesn't recognize.
    // Swift's `JSONDecoder` silently ignores unknown keys, which
    // would hide caller-side typos and let the Mac IPC diverge
    // from the documented wire contract. CR (codex) flagged this
    // on PR #78.
    if let dict = raw as? [String: Any] {
        let extras = Set(dict.keys).subtracting(expected)
        if !extras.isEmpty {
            let joined = extras.sorted().joined(separator: ", ")
            throw IPCHandlerError(
                code: "unknown-field",
                message: "unknown params: \(joined)"
            )
        }
    }
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
    @StringInt64 var tabID: Int64
    enum CodingKeys: String, CodingKey { case tabID = "tab_id" }
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

private struct IPCScreenshotParams: Codable {
    var scale: UInt32 = 1
    enum CodingKeys: String, CodingKey { case scale }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.scale = try c.decodeIfPresent(UInt32.self, forKey: .scale) ?? 1
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(scale, forKey: .scale)
    }
}

private struct IPCScreenshotResult: Codable {
    let png: Data
    let width: UInt32
    let height: UInt32
    let scale: UInt32
    enum CodingKeys: String, CodingKey { case png, width, height, scale }
    init(png: Data, width: UInt32, height: UInt32, scale: UInt32) {
        self.png = png
        self.width = width
        self.height = height
        self.scale = scale
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let b64 = try c.decode(String.self, forKey: .png)
        guard let d = Data(base64Encoded: b64) else {
            throw DecodingError.dataCorruptedError(
                forKey: .png, in: c, debugDescription: "png must be base64"
            )
        }
        self.png = d
        self.width = try c.decode(UInt32.self, forKey: .width)
        self.height = try c.decode(UInt32.self, forKey: .height)
        self.scale = try c.decode(UInt32.self, forKey: .scale)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(png.base64EncodedString(), forKey: .png)
        try c.encode(width, forKey: .width)
        try c.encode(height, forKey: .height)
        try c.encode(scale, forKey: .scale)
    }
}

private struct IPCTabResizeParams: Codable {
    @StringInt64 var tabID: Int64
    let cols: UInt32
    let rows: UInt32
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case cols, rows
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
    @StringInt64 var projectID: Int64
    let name: String
    enum CodingKeys: String, CodingKey {
        case projectID = "project_id"
        case name
    }
}

private struct IPCProjectDeleteParams: Codable {
    @StringInt64 var projectID: Int64
    enum CodingKeys: String, CodingKey { case projectID = "project_id" }
}

private struct IPCTabReorderParams: Codable {
    @StringInt64 var projectID: Int64
    @StringInt64Array var tabIDs: [Int64]
    enum CodingKeys: String, CodingKey {
        case projectID = "project_id"
        case tabIDs = "tab_ids"
    }
}

private struct IPCProjectReorderParams: Codable {
    @StringInt64Array var projectIDs: [Int64]
    enum CodingKeys: String, CodingKey { case projectIDs = "project_ids" }
}

/// Codable wrapper for the wire's string-encoded int64 ids (JSON numbers
/// lose precision past 2^53). `@StringInt64 var tabID: Int64` + a
/// `CodingKeys` remap replaces the per-struct hand-rolled string↔Int64
/// decode/encode. Mirrors the Rust `string_int64` serde module.
@propertyWrapper
struct StringInt64: Codable {
    var wrappedValue: Int64
    init(wrappedValue: Int64) { self.wrappedValue = wrappedValue }
    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorrupted(
                .init(
                    codingPath: decoder.codingPath,
                    debugDescription: "expected string int64, got \"\(raw)\""
                ))
        }
        self.wrappedValue = v
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try c.encode(String(wrappedValue))
    }
}

/// Same, for the wire's `[String]` id arrays (`tab_ids`, `project_ids`).
/// Mirrors the Rust `vec_string_int64` serde module.
@propertyWrapper
struct StringInt64Array: Codable {
    var wrappedValue: [Int64]
    init(wrappedValue: [Int64]) { self.wrappedValue = wrappedValue }
    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode([String].self)
        self.wrappedValue = try raw.map { s in
            guard let v = Int64(s) else {
                throw DecodingError.dataCorrupted(
                    .init(
                        codingPath: decoder.codingPath,
                        debugDescription: "expected string int64, got \"\(s)\""
                    ))
            }
            return v
        }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try c.encode(wrappedValue.map { String($0) })
    }
}

private struct IPCTabFocusParams: Codable {
    @StringInt64 var tabID: Int64
    enum CodingKeys: String, CodingKey { case tabID = "tab_id" }
}

private struct IPCTabFocusResult: Codable {
    @StringInt64 var previousProjectID: Int64
    @StringInt64 var previousTabID: Int64
    enum CodingKeys: String, CodingKey {
        case previousProjectID = "previous_project_id"
        case previousTabID = "previous_tab_id"
    }
}

private struct IPCTabDumpParams: Codable {
    @StringInt64 var tabID: Int64
    enum CodingKeys: String, CodingKey { case tabID = "tab_id" }
}

/// Cursor position inside a dumped viewport. Plain JSON numbers (not
/// string-int64) — these are small viewport coordinates, matching the
/// Rust `TabDumpCursor`.
private struct IPCTabDumpCursor: Codable {
    let row: Int
    let col: Int
    let visible: Bool
}

private struct IPCTabDumpResult: Codable {
    let cols: Int
    let rows: Int
    let cursor: IPCTabDumpCursor?
    let rowsText: [String]
    enum CodingKeys: String, CodingKey {
        case cols
        case rows
        case cursor
        case rowsText = "rows_text"
    }
}

/// Params for the nullary palette ops (`palette.state` / `palette.dismiss`):
/// an empty object. `decodeParams(expected: [])` then rejects any stray
/// field, matching the strict server policy.
private struct IPCEmptyParams: Codable {}

private struct IPCPaletteOpenParams: Codable {
    let kind: String?
    enum CodingKeys: String, CodingKey { case kind }
}

private struct IPCPaletteQueryParams: Codable {
    let query: String
    enum CodingKeys: String, CodingKey { case query }
}

private struct IPCPaletteActivateParams: Codable {
    let id: String
    enum CodingKeys: String, CodingKey { case id }
}

private struct IPCPaletteItemView: Codable {
    let id: String
    let title: String
    let subtitle: String?
    enum CodingKeys: String, CodingKey { case id, title, subtitle }
}

/// `palette.*` response. Mirrors `roost_ipc::messages::PaletteStateResult`
/// — `frame` is omitted (Swift drops nil optionals) when the palette is
/// closed, matching the Rust `skip_serializing_if`.
private struct IPCPaletteStateResult: Codable {
    let open: Bool
    let frame: String?
    let query: String
    let selection: Int
    let items: [IPCPaletteItemView]
    enum CodingKeys: String, CodingKey { case open, frame, query, selection, items }

    init(_ s: PaletteSnapshot) {
        self.open = s.open
        self.frame = s.frame
        self.query = s.query
        self.selection = s.selection
        self.items = s.items.map {
            IPCPaletteItemView(id: $0.id, title: $0.title, subtitle: $0.subtitle)
        }
    }
}

private struct IPCTabSetTitleParams: Codable {
    @StringInt64 var tabID: Int64
    let title: String
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case title
    }
}

private struct IPCTabSetStateParams: Codable {
    @StringInt64 var tabID: Int64
    let state: IPCTabState
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case state
    }
}

private struct IPCTabClearNotificationParams: Codable {
    @StringInt64 var tabID: Int64
    enum CodingKeys: String, CodingKey { case tabID = "tab_id" }
}

private struct IPCTabSetHookActiveParams: Codable {
    @StringInt64 var tabID: Int64
    let active: Bool
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case active
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

// MARK: selection + clipboard test-op wire types

private struct IPCSelectionPoint: Codable {
    let col: UInt16
    let row: UInt16
}

private struct IPCSelectionSetParams: Codable {
    let tabID: Int64
    let anchor: IPCSelectionPoint
    let cursor: IPCSelectionPoint
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case anchor, cursor
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c,
                debugDescription: "tab_id must be a stringified int64"
            )
        }
        self.tabID = v
        self.anchor = try c.decode(IPCSelectionPoint.self, forKey: .anchor)
        self.cursor = try c.decode(IPCSelectionPoint.self, forKey: .cursor)
    }
}

private struct IPCSelectionClearParams: Codable {
    let tabID: Int64
    enum CodingKeys: String, CodingKey { case tabID = "tab_id" }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c,
                debugDescription: "tab_id must be a stringified int64"
            )
        }
        self.tabID = v
    }
}

private struct IPCSelectionDumpParams: Codable {
    let tabID: Int64
    enum CodingKeys: String, CodingKey { case tabID = "tab_id" }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try c.decode(String.self, forKey: .tabID)
        guard let v = Int64(raw) else {
            throw DecodingError.dataCorruptedError(
                forKey: .tabID, in: c,
                debugDescription: "tab_id must be a stringified int64"
            )
        }
        self.tabID = v
    }
}

private struct IPCSelectionDumpResult: Codable {
    let text: String?
    let anchorVisible: Bool
    let cursorVisible: Bool
    enum CodingKeys: String, CodingKey {
        case text
        case anchorVisible = "anchor_visible"
        case cursorVisible = "cursor_visible"
    }
}

private struct IPCClipboardDumpParams: Codable {
    let target: String
}

private struct IPCClipboardDumpResult: Codable {
    let text: String?
}

private struct IPCClipboardWriteParams: Codable {
    let target: String
    let text: String
}

// MARK: test-only ops (ROOST_TEST_MODE=1)

/// `tab.feed_pty_bytes` params. Same shape as `IPCTabWriteParams`
/// (string-int64 tab id + base64 byte payload); kept as a separate
/// struct rather than aliased so the wire schemas can diverge if
/// they need to.
private struct IPCTabFeedPtyBytesParams: Codable {
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

private struct IPCTabCapturePtyInputParams: Codable {
    let tabID: Int64
    let drain: Bool
    enum CodingKeys: String, CodingKey {
        case tabID = "tab_id"
        case drain
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
        // `drain` defaults to false so a caller can omit it (peek
        // semantics). Matches the Rust `#[serde(default)] pub drain:
        // bool` shape in `TabCapturePtyInputParams`.
        self.drain = try c.decodeIfPresent(Bool.self, forKey: .drain) ?? false
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(String(tabID), forKey: .tabID)
        try c.encode(drain, forKey: .drain)
    }
}

private struct IPCTabCapturePtyInputResult: Codable {
    let data: Data
    enum CodingKeys: String, CodingKey { case data }
    init(data: Data) {
        self.data = data
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
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
        try c.encode(data.base64EncodedString(), forKey: .data)
    }
}

private struct IPCTabDumpResolvedParams: Codable {
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

private struct IPCResolvedCell: Codable {
    let row: UInt32
    let col: UInt16
    let text: String
    let fg: String
    let bg: String
    let hasExplicitBg: Bool
    let bold: Bool
    let italic: Bool
    let inverse: Bool
    enum CodingKeys: String, CodingKey {
        case row, col, text, fg, bg
        case hasExplicitBg = "has_explicit_bg"
        case bold, italic, inverse
    }
}

private struct IPCTabDumpResolvedResult: Codable {
    let cols: UInt16
    let rows: UInt16
    let cells: [IPCResolvedCell]
}

/// Format an `NSColor` as `#RRGGBB` for the `tab.dump_resolved` wire
/// shape. Matches the GTK-side `rgb_hex` helper in
/// `crates/roost-linux/src/ipc.rs` so cross-UI dumps compare equal.
/// Returns `"#000000"` for any color that can't be converted to sRGB
/// (defensive — every theme color in the bundled set converts).
private func hexFromNSColor(_ color: NSColor) -> String {
    guard let srgb = color.usingColorSpace(.sRGB) else { return "#000000" }
    let r = UInt8(round(srgb.redComponent * 255))
    let g = UInt8(round(srgb.greenComponent * 255))
    let b = UInt8(round(srgb.blueComponent * 255))
    return String(format: "#%02x%02x%02x", r, g, b)
}
