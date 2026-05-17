// Roost Mac client — Phase 6a step 2b: sidebar + multi-project.
//
// The window splits horizontally into a project sidebar (left) and the
// existing tab-bar + terminal area (right). Each project owns its own
// set of `TabSession`s; switching the sidebar selection rebuilds the
// tab bar with only that project's tabs.
//
// Project lifecycle is end-to-end against the daemon's new RPCs:
//   * `+ New Project` at the bottom of the sidebar → CreateProject;
//   * right-click on a project row → Rename / Delete (Delete cascades
//     the project's tabs daemon-side, which we mirror locally before
//     refreshing the sidebar);
//   * the File menu gains "New Project" (⌘N).
//
// WatchEvents subscription for cross-client convergence is the
// follow-up slice; everything in this commit reads daemon state via
// `listProjects` on launch and otherwise drives mutations directly.

import AppKit
import Foundation

@main
@MainActor
final class RoostApp: NSObject, NSApplicationDelegate {
    private var window: NSWindow?

    /// Sidebar widgets. The project list is an `NSOutlineView` styled
    /// as a source list (Phase 6a step 2i / goal doc M2). The outline
    /// view's selection state is the single source of truth for the
    /// active-row affordance — no `"● "` text marker; AppKit's native
    /// row selection draws the highlight. `+ New Project` is a footer
    /// button anchored at the pane's bottom.
    private var projectsOutlineView: NSOutlineView?
    private var projectsScrollView: NSScrollView?
    private var newProjectButton: NSButton?

    /// Set while a programmatic selection change is in flight, so the
    /// `outlineViewSelectionDidChange` delegate method doesn't bounce
    /// the user-initiated path. Without it, every
    /// `applySidebarSelection()` would round-trip through
    /// `selectProject(id:)` and cause spurious tab spawns / focus
    /// changes when the selection is being driven from a non-click
    /// path (WatchEvents, `⌘1..⌘9`, project lifecycle).
    private var isSyncingSidebarSelection = false

    private var tabBar: NSStackView?
    private var addTabButton: NSButton?
    private var terminalContainer: NSView?
    private var windowMenu: NSMenu?

    /// Captured at `init` time so the M3 toggle handler can flip the
    /// pane's `isHidden` without re-finding it in the view hierarchy.
    /// `NSSplitView.addArrangedSubview` honors hidden subviews by
    /// collapsing their slot — no separate "collapsed" API needed.
    private var sidebarPane: NSView?

    /// Persistence key for the M3 toggle-sidebar state. Read at launch,
    /// written on every toggle. Default = true (visible) for new users.
    private static let sidebarVisibleDefaultsKey = "RoostSidebarVisible"

    /// Workspace model. `projects` mirrors the daemon's project list
    /// in display order; `tabs` is a flat list of every open
    /// TabSession across all projects, filtered into the tab bar by
    /// `activeProjectID`. `activeSessionByProject` remembers each
    /// project's last-focused TabSession by reference (rather than by
    /// daemon tab id) so the active marker survives the window
    /// between `OpenTab` being called and the daemon assigning an id.
    private var projects: [ProjectSnapshot] = []
    private var tabs: [TabSession] = []
    private var activeProjectID: Int64?
    private var activeSessionByProject: [Int64: TabSession] = [:]

    private var socketPath: String = ""
    private var daemonReachable: Bool = false

    /// User config (Phase 6a M6). Resolved once on launch from
    /// `~/.config/roost/config.conf`; the values flow into theme
    /// + font selection.
    private var config: RoostConfig = .empty

    /// Resolved keybind table — `(keyEquivalent, modifierMask) →
    /// action`. Built from `defaultBindingsMac()` layered with
    /// `config.keybinds` via `canonicalizeBindings`. Phase 6a P1
    /// uses this in `installMainMenu` to drive each
    /// `NSMenuItem.keyEquivalent` + `keyEquivalentModifierMask`.
    /// Inverted lookup at install time (`action → first matching
    /// Accel`) — actions with no entry in the table install with
    /// an empty key equivalent (effectively unbound).
    private var keybinds: [Accel: String] = [:]
    private var activeTheme: Theme = .fallback
    private var activeFont: NSFont = .monospacedSystemFont(
        ofSize: 14,
        weight: .regular
    )

    /// Long-lived WatchEvents subscription task. `nil` until
    /// `bootstrapWorkspace` resolves and `subscribeToEvents` runs; the
    /// task runs forever (reconnecting on stream end) until
    /// `applicationWillTerminate` cancels it.
    private var eventsTask: Task<Void, Never>?

    /// Phase 6a P8: desktop notification coordinator. Owns the
    /// UNUserNotificationCenter delegate (retained for lifetime
    /// of the app) + the authorized flag. `applicationDidFinishLaunching`
    /// fires `requestAuthorization`; `handleEvent`'s
    /// `notification(e)` case routes `NotificationEvent`s here.
    private let desktopNotifications = DesktopNotifications()

    nonisolated static func main() {
        let app = NSApplication.shared
        let delegate = RoostApp()
        app.delegate = delegate
        app.setActivationPolicy(.regular)
        app.run()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let socketPath = Self.defaultSocketPath()
        self.socketPath = socketPath

        // Phase 6a M6: read user config + resolve theme + font
        // before anything draws. Missing config → `.empty`; missing
        // theme name → bundled `roost-dark`. Font defaults to
        // `.monospacedSystemFont(ofSize: 14)` unless the user
        // overrides `font-family` / `font-size`.
        self.config = RoostConfig.load()
        self.activeTheme = Theme.loadBundled(name: config.themeName ?? "roost-dark")
        self.activeFont = resolveFont(
            family: config.fontFamily,
            size: config.fontSize ?? 14
        )

        // Phase 6a P1: resolve the keybind table BEFORE the menu
        // installs, so installMainMenu can drive every shortcut
        // off the user's config (with the macOS defaults as the
        // fallback layer).
        self.keybinds = canonicalizeBindings(
            defaults: defaultBindingsMac(),
            user: config.keybinds,
            warn: { msg, trigger, action in
                NSLog(
                    "roost-mac: keybind %@: trigger=%@ action=%@",
                    msg,
                    trigger,
                    action
                )
            }
        )

        installMainMenu()

        // Phase 6a P8: prompt for notification permissions at launch
        // so the system dialog arrives at a predictable moment rather
        // than mid-session when the first NotificationEvent fires.
        // Hook the click handler to focus the originating tab —
        // walks `projects` / `tabs` and reuses the M2 selectProject
        // + M3 selectTab paths.
        desktopNotifications.requestAuthorization()
        desktopNotifications.onActivate = { [weak self] tabID in
            guard let self else { return }
            guard let session = self.tabs.first(where: { $0.id == tabID }) else {
                return
            }
            // Switch project first if needed, then focus the tab
            // within it. selectProject is idempotent when the id
            // matches.
            if session.projectID != self.activeProjectID {
                self.selectProject(id: session.projectID)
            }
            let projectTabs = self.tabs.filter { $0.projectID == session.projectID }
            if let idx = projectTabs.firstIndex(where: { $0 === session }) {
                self.selectTab(at: idx)
            }
            NSApp.activate(ignoringOtherApps: true)
        }

        // Probe the cell-grid intrinsic size so the right pane reserves
        // enough room for an 80×24 terminal — `TerminalView` still pins
        // its own width/height to that size in `selectTab(at:)`. The
        // window itself opens at a generous default and is freely
        // resizable; reflow to the larger cell grid is Phase 6a step 2g.
        let metricsProbe = TerminalView(
            cols: 80,
            rows: 24,
            theme: activeTheme,
            font: activeFont
        )
        let terminalSize = metricsProbe.intrinsicContentSize
        let sidebarWidth: CGFloat = 220
        let tabBarHeight: CGFloat = 32
        let defaultContentWidth: CGFloat = 1100
        let defaultContentHeight: CGFloat = 700

        let window = NSWindow(
            contentRect: NSRect(
                x: 200,
                y: 200,
                width: defaultContentWidth,
                height: defaultContentHeight
            ),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Roost"
        window.minSize = NSSize(width: 720, height: 420)
        // Dark chrome (toolbar/titlebar) so the white frame doesn't
        // clash with the terminal's dark background. Will become a
        // theme setting once `phase-6a` step 2d (keybind/config) lands.
        window.appearance = NSAppearance(named: .darkAqua)

        // ---- Split view: sidebar | content ---------------------------
        let split = NSSplitView()
        split.isVertical = true
        split.dividerStyle = .thin
        split.translatesAutoresizingMaskIntoConstraints = false

        let sidebar = makeSidebarPane(width: sidebarWidth)
        let content = makeContentPane(
            socketPath: socketPath,
            terminalSize: terminalSize,
            tabBarHeight: tabBarHeight
        )

        split.addArrangedSubview(sidebar)
        split.addArrangedSubview(content)
        split.setHoldingPriority(.defaultHigh, forSubviewAt: 0)
        self.sidebarPane = sidebar
        // Restore the user's last-known toggle state (M3). UserDefaults
        // returns `false` for an unset key, so we read it back as
        // Optional<Bool>-shaped to distinguish "not set" (default
        // visible) from "explicitly false" (user hid it).
        let stored = UserDefaults.standard.object(forKey: Self.sidebarVisibleDefaultsKey) as? Bool
        sidebar.isHidden = stored == false

        let root = NSView(frame: window.contentRect(forFrameRect: window.frame))
        root.addSubview(split)
        NSLayoutConstraint.activate([
            split.topAnchor.constraint(equalTo: root.topAnchor),
            split.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            split.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            split.trailingAnchor.constraint(equalTo: root.trailingAnchor),
        ])
        window.contentView = root

        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        self.window = window

        Task { [weak self] in
            let outcome = await runIdentify(socketPath: socketPath)
            await MainActor.run { [weak self] in
                self?.applyIdentifyOutcome(outcome)
                if case .ok = outcome {
                    self?.daemonReachable = true
                    self?.bootstrapWorkspace()
                }
            }
        }
    }

    // MARK: - Layout

    @MainActor
    private func makeSidebarPane(width: CGFloat) -> NSView {
        let pane = NSView()
        pane.translatesAutoresizingMaskIntoConstraints = false

        let header = NSTextField(labelWithString: "PROJECTS")
        header.font = .systemFont(ofSize: 11, weight: .semibold)
        header.textColor = .secondaryLabelColor
        header.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(header)

        // ---- Outline view --------------------------------------------------
        let outline = NSOutlineView()
        outline.headerView = nil
        outline.style = .sourceList
        outline.rowSizeStyle = .default
        outline.indentationPerLevel = 0
        outline.allowsMultipleSelection = false
        outline.allowsEmptySelection = true
        outline.focusRingType = .none
        outline.translatesAutoresizingMaskIntoConstraints = false

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("name"))
        column.title = ""
        column.resizingMask = .autoresizingMask
        outline.addTableColumn(column)
        outline.outlineTableColumn = column

        outline.dataSource = self
        outline.delegate = self
        outline.action = #selector(sidebarRowClicked(_:))
        outline.target = self

        // Right-click context menu — items target `clickedRow` so the
        // same NSMenu serves every project row without bespoke
        // per-row construction.
        let rowMenu = NSMenu()
        rowMenu.delegate = self
        let rename = NSMenuItem(
            title: "Rename…",
            action: #selector(renameProjectFromMenu(_:)),
            keyEquivalent: ""
        )
        rename.target = self
        rowMenu.addItem(rename)
        let delete = NSMenuItem(
            title: "Delete",
            action: #selector(deleteProjectFromMenu(_:)),
            keyEquivalent: ""
        )
        delete.target = self
        rowMenu.addItem(delete)
        outline.menu = rowMenu

        let scrollView = NSScrollView()
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = false
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false
        scrollView.borderType = .noBorder
        scrollView.documentView = outline
        scrollView.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(scrollView)

        // ---- Footer --------------------------------------------------------
        let addProject = NSButton(
            title: "+ New Project",
            target: self,
            action: #selector(newProject(_:))
        )
        addProject.bezelStyle = .rounded
        addProject.toolTip = "New project (⌘N)"
        addProject.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(addProject)

        NSLayoutConstraint.activate([
            pane.widthAnchor.constraint(greaterThanOrEqualToConstant: width),

            header.topAnchor.constraint(equalTo: pane.topAnchor, constant: 12),
            header.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 16),
            header.trailingAnchor.constraint(equalTo: pane.trailingAnchor, constant: -12),

            scrollView.topAnchor.constraint(equalTo: header.bottomAnchor, constant: 6),
            scrollView.leadingAnchor.constraint(equalTo: pane.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: pane.trailingAnchor),
            scrollView.bottomAnchor.constraint(equalTo: addProject.topAnchor, constant: -8),

            addProject.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 12),
            addProject.bottomAnchor.constraint(equalTo: pane.bottomAnchor, constant: -12),
        ])

        self.projectsOutlineView = outline
        self.projectsScrollView = scrollView
        self.newProjectButton = addProject
        return pane
    }

    @MainActor
    private func makeContentPane(
        socketPath: String,
        terminalSize: NSSize,
        tabBarHeight: CGFloat
    ) -> NSView {
        let pane = NSView()
        pane.translatesAutoresizingMaskIntoConstraints = false

        let tabBar = NSStackView()
        tabBar.orientation = .horizontal
        tabBar.alignment = .centerY
        tabBar.spacing = 6
        tabBar.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(tabBar)

        // "+" is a plain bordered button to the right of the pills.
        // Lighter affordance than a full-bezel rounded button so the
        // tab pills carry the visual weight.
        let addTabButton = NSButton(
            title: "＋",
            target: self,
            action: #selector(newTab(_:))
        )
        addTabButton.bezelStyle = .accessoryBar
        addTabButton.isBordered = false
        addTabButton.toolTip = "New tab (⌘T)"
        addTabButton.font = .systemFont(ofSize: 16, weight: .regular)
        addTabButton.contentTintColor = .secondaryLabelColor
        tabBar.addArrangedSubview(addTabButton)

        let terminalContainer = NSView()
        terminalContainer.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(terminalContainer)

        NSLayoutConstraint.activate([
            tabBar.topAnchor.constraint(equalTo: pane.topAnchor, constant: 12),
            tabBar.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 16),
            tabBar.trailingAnchor.constraint(lessThanOrEqualTo: pane.trailingAnchor, constant: -16),
            // Tab bar height stays intrinsic to its tallest button.

            // Terminal container fills the content pane below the
            // tab bar. Width is unconstrained from above — when the
            // window resizes, the container grows and `TerminalView`
            // reflows its cell grid in `setFrameSize`. `terminalSize`
            // (the 80x24 cell-grid intrinsic) is preserved as the
            // floor so the terminal can't be squeezed below a
            // minimal usable shape.
            terminalContainer.topAnchor.constraint(equalTo: tabBar.bottomAnchor, constant: 8),
            terminalContainer.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 16),
            terminalContainer.trailingAnchor.constraint(equalTo: pane.trailingAnchor, constant: -16),
            terminalContainer.bottomAnchor.constraint(equalTo: pane.bottomAnchor, constant: -16),
            terminalContainer.widthAnchor.constraint(
                greaterThanOrEqualToConstant: terminalSize.width
            ),
            terminalContainer.heightAnchor.constraint(
                greaterThanOrEqualToConstant: terminalSize.height
            ),
        ])

        _ = socketPath    // retained for future toolbar/diagnostics surfacing
        _ = tabBarHeight  // reserved for the window's min-size math

        self.tabBar = tabBar
        self.addTabButton = addTabButton
        self.terminalContainer = terminalContainer
        return pane
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func applicationWillTerminate(_ notification: Notification) {
        eventsTask?.cancel()
        eventsTask = nil
        for tab in tabs {
            tab.close(socketPath: socketPath)
        }
        tabs.removeAll()
        activeSessionByProject.removeAll()
    }

    // MARK: - Workspace bootstrap

    /// Right after Identify, fetch the daemon's project list and seat
    /// the UI. If the daemon has no projects yet (first run), ask for
    /// one — the UI is much friendlier with a populated sidebar than
    /// with an empty one waiting for the user to discover the "+"
    /// button. Tabs reported by the daemon are intentionally ignored
    /// here: Phase 5's StreamPty re-spawns the shell on attach, so
    /// reattaching to "old" tabs would just create fresh shells with
    /// stale IDs. Each fresh launch starts clean.
    @MainActor
    private func bootstrapWorkspace() {
        let socketPath = self.socketPath
        Task { [weak self] in
            var fetched = await listProjects(socketPath: socketPath)
            if fetched.isEmpty {
                if let created = await createProject(
                    socketPath: socketPath,
                    name: "",
                    cwd: ""
                ) {
                    fetched = [created]
                }
            }
            await MainActor.run { [weak self] in
                guard let self else { return }
                self.projects = fetched
                self.rebuildSidebar()
                if let first = self.projects.first {
                    self.selectProject(id: first.id)
                }
                self.subscribeToEvents()
            }
        }
    }

    /// Long-lived WatchEvents subscription. Drains the daemon's
    /// server-stream and dispatches each event to a `@MainActor`
    /// handler. On stream end (daemon shutdown, network error,
    /// `Lagged` from the broadcast buffer overflowing) the helper
    /// performs a full `listProjects` resync and reconnects, so a
    /// transient disconnect doesn't permanently leave the UI stale.
    @MainActor
    private func subscribeToEvents() {
        guard eventsTask == nil else { return }
        let socketPath = self.socketPath
        eventsTask = Task { [weak self] in
            while !Task.isCancelled {
                let stream = watchEvents(socketPath: socketPath)
                for await event in stream {
                    if Task.isCancelled { return }
                    let kind = event.kind
                    await MainActor.run { [weak self] in
                        guard let self else { return }
                        if let kind { self.handleEvent(kind) }
                    }
                }
                // Stream ended — resync from scratch and try again.
                // Without this any transient disconnect would leave
                // the UI silently stale.
                if Task.isCancelled { return }
                let fresh = await listProjects(socketPath: socketPath)
                await MainActor.run { [weak self] in
                    self?.applyProjectsResync(fresh)
                }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    /// Reconcile the daemon's project list with our local model after
    /// a stream reconnect. Adds new projects, removes deleted ones,
    /// renames the rest in place. Active selection is preserved when
    /// possible; if the active project was deleted server-side, fall
    /// back to the first available project.
    @MainActor
    private func applyProjectsResync(_ fresh: [ProjectSnapshot]) {
        let freshByID = Dictionary(uniqueKeysWithValues: fresh.map { ($0.id, $0) })
        let staleIDs = Set(projects.map(\.id)).subtracting(freshByID.keys)
        for staleID in staleIDs {
            removeProjectLocally(id: staleID)
        }
        projects = fresh
        rebuildSidebar()
        if let activeProjectID, freshByID[activeProjectID] == nil {
            self.activeProjectID = nil
            if let first = projects.first {
                selectProject(id: first.id)
            } else {
                updateWindowTitle()
                terminalContainer?.subviews.forEach { $0.removeFromSuperview() }
            }
        }
    }

    /// Project deletion path shared between `WatchEvents`-driven
    /// `ProjectDeleted` and the resync codepath. Closes any UI-side
    /// TabSession in the project so its StreamPty shuts down cleanly.
    @MainActor
    private func removeProjectLocally(id: Int64) {
        let condemned = tabs.filter { $0.projectID == id }
        for session in condemned {
            session.terminalView.removeFromSuperview()
            session.close(socketPath: socketPath)
        }
        tabs.removeAll { $0.projectID == id }
        activeSessionByProject.removeValue(forKey: id)
        projects.removeAll { $0.id == id }
    }

    /// Dispatch one event from the WatchEvents stream. Anything not
    /// surfaced visually in M1 is logged and dropped — later
    /// milestones (M3 tab strip, Phase 6b notifications) light up
    /// the remaining cases.
    @MainActor
    private func handleEvent(_ kind: Roost_V1_Event.OneOf_Kind) {
        switch kind {
        case .projectCreated(let e):
            let p = e.project
            let snap = ProjectSnapshot(id: p.id, name: p.name, cwd: p.cwd)
            if !projects.contains(where: { $0.id == snap.id }) {
                projects.append(snap)
                rebuildSidebar()
            }
        case .projectRenamed(let e):
            if let idx = projects.firstIndex(where: { $0.id == e.projectID }) {
                projects[idx] = ProjectSnapshot(
                    id: e.projectID,
                    name: e.name,
                    cwd: projects[idx].cwd
                )
                rebuildSidebar()
            }
        case .projectDeleted(let e):
            let wasActive = activeProjectID == e.projectID
            removeProjectLocally(id: e.projectID)
            rebuildSidebar()
            if wasActive {
                activeProjectID = nil
                if let next = projects.first {
                    selectProject(id: next.id)
                } else {
                    updateWindowTitle()
                    terminalContainer?.subviews.forEach { $0.removeFromSuperview() }
                }
            }
        case .tabDeleted(let e):
            // Headless `tab close` (M4) or any external `CloseTab`
            // call kills the daemon-side PTY; the Mac UI's
            // StreamPty for that tab receives PtyExit and the
            // TabSession's session task exits, but we still hold
            // the reference in `tabs` until we hear this event.
            // Tear it down now so the tab strip converges.
            guard let session = tabs.first(where: { $0.id == e.tabID }) else { break }
            let projectID = session.projectID
            let wasActive = activeSessionByProject[projectID] === session
            tabs.removeAll { $0 === session }
            if wasActive {
                activeSessionByProject.removeValue(forKey: projectID)
            }
            session.terminalView.removeFromSuperview()
            session.close(socketPath: socketPath)
            if projectID == activeProjectID {
                rebuildTabBar()
                if wasActive {
                    let remaining = tabsForActiveProject()
                    if remaining.isEmpty, daemonReachable {
                        openNewTab()
                    } else if !remaining.isEmpty {
                        selectTab(at: 0)
                    }
                }
            }
        case .tabOpened, .active:
            // Cross-client `OpenTab` produces a daemon-side tab the
            // UI doesn't yet hold a TabSession for. Surfacing it in
            // the strip would require an "attach to existing tab"
            // path through `TabSession.start` that skips the
            // OpenTab RPC (since the daemon tab already exists) —
            // separate-slice work. For now we drop the event.
            // `Active` is daemon-driven active selection: the UI's
            // local active state is authoritative within the UI,
            // so we drop this too.
            NSLog("roost-mac: watchEvents tab event ignored: %@", "\(kind)")
        case .tabTitle(let e):
            // Phase 6a P6: OSC 0/1/2 changed a tab's title. Mirror
            // into the matching TabSession so rebuildTabBar uses
            // the live title in the pill label.
            if let session = tabs.first(where: { $0.id == e.tabID }) {
                session.liveTitle = e.title
                if session.projectID == activeProjectID {
                    rebuildTabBar()
                }
            }
        case .tabCwd(let e):
            // OSC 7 changed cwd. Same flow — update + rebuild
            // when the affected tab is in the active project.
            if let session = tabs.first(where: { $0.id == e.tabID }) {
                session.liveCwd = e.cwd
                if session.projectID == activeProjectID {
                    rebuildTabBar()
                }
            }
        case .tabState(let e):
            // TabState updates light up the status-dot slot M3's
            // TabPillView reserved. Stash and rebuild.
            if let session = tabs.first(where: { $0.id == e.tabID }) {
                session.liveState = Int32(e.state.rawValue)
                if session.projectID == activeProjectID {
                    rebuildTabBar()
                }
            }
        case .tabNotification(let e):
            // Phase 6a P7: mirror has_pending onto the matching
            // TabSession + rebuild the strip + sidebar so the
            // badge slot reflects the new state. The daemon
            // already aggregates per-tab; per-project rollup
            // happens in `pillBadgeForProject` at render time.
            if let session = tabs.first(where: { $0.id == e.tabID }) {
                session.liveHasNotification = e.hasPending_p
                if session.projectID == activeProjectID {
                    rebuildTabBar()
                }
                rebuildSidebar()  // sidebar's per-project rollup
            }
        case .notification(let e):
            // Phase 6a P8: route the daemon-emitted notification
            // to a macOS banner via UNUserNotificationCenter.
            // The daemon already applied hook_active suppression
            // in P5; by the time we see a NotificationEvent here
            // the surface is ours to render.
            desktopNotifications.emit(
                tabID: e.tabID,
                title: e.title,
                body: e.body
            )
        case .tabsReordered, .projectsReordered, .hookActive:
            // Reorder + hookActive are out of scope for this goal.
            break
        }
    }

    // MARK: - Project management

    @MainActor
    private func rebuildSidebar() {
        guard let outline = projectsOutlineView else { return }
        outline.reloadData()
        applySidebarSelection()
        updateWindowTitle()
        // Window menu's Project section is driven off `projects`; keep
        // it in sync so ⌘1..⌘9 always reflects the current sidebar.
        rebuildWindowMenu()
    }

    /// Match the outline view's selected row to `activeProjectID`.
    /// Wrapped in `isSyncingSidebarSelection` so the corresponding
    /// `outlineViewSelectionDidChange` callback doesn't bounce the
    /// selection back through `selectProject(id:)`.
    @MainActor
    private func applySidebarSelection() {
        guard let outline = projectsOutlineView else { return }
        let row: Int
        if let activeProjectID,
           let idx = projects.firstIndex(where: { $0.id == activeProjectID })
        {
            row = idx
        } else {
            row = -1
        }
        isSyncingSidebarSelection = true
        if row >= 0 {
            outline.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
        } else {
            outline.deselectAll(nil)
        }
        isSyncingSidebarSelection = false
    }

    /// Single-click on a sidebar row. NSOutlineView fires `action` on
    /// every click, including in-place clicks on the already-selected
    /// row — guard against re-running `selectProject` in that case.
    /// The `selectionDidChange` delegate also fires the same path; we
    /// route through `selectProject` from one place to avoid double
    /// work.
    @objc @MainActor
    private func sidebarRowClicked(_ sender: Any?) {
        // No-op — selection changes route through
        // `outlineViewSelectionDidChange(_:)`. Keeping the action
        // wired so AppKit still flips selection on single click in
        // source-list style.
    }

    @MainActor
    private func selectProject(id: Int64) {
        // M3: Reveal the sidebar on ⌘1-9 / explicit project-switch
        // so the user can see which project they've landed on.
        // Mirrors Go `cmd/roost/app.go:1487`. Programmatic selection
        // paths that already have the sidebar in view (single-click
        // sidebar row, WatchEvents reconcile) flow through here too
        // — calling ensureVisible is idempotent when already shown.
        ensureSidebarVisible()
        activeProjectID = id
        applySidebarSelection()
        updateWindowTitle()
        rebuildTabBar()

        let projectTabs = tabsForActiveProject()
        if projectTabs.isEmpty {
            // Mirror the Go binary's "every project always shows a
            // tab" feel: auto-open one so the terminal area is never
            // stuck on a previous project's view. Lazy — only the
            // visited project incurs the spawn cost.
            if daemonReachable {
                openNewTab()
            } else {
                // No daemon, can't open a tab — at minimum clear the
                // container so the old project's terminal doesn't
                // linger behind a fresh selection.
                terminalContainer?.subviews.forEach { $0.removeFromSuperview() }
            }
            return
        }

        // Restore the project's last-active TabSession, falling back
        // to the first if the remembered one was closed.
        let preferred = activeSessionByProject[id]
        let index = projectTabs.firstIndex(where: { $0 === preferred }) ?? 0
        selectTab(at: index)
    }

    @objc @MainActor
    private func newProject(_ sender: Any?) {
        guard daemonReachable else { return }
        // M3: reveal the sidebar BEFORE the async create round-trip
        // so the user gets immediate visual feedback even if the
        // create fails. Matches Go `cmd/roost/app.go:1337`. The
        // follow-on `selectProject(id:)` call also ensures visibility
        // for the success path; this one defends the failure path.
        ensureSidebarVisible()
        let socketPath = self.socketPath
        Task { [weak self] in
            let created = await createProject(socketPath: socketPath, name: "", cwd: "")
            await MainActor.run { [weak self] in
                guard let self, let created else { return }
                self.projects.append(created)
                self.rebuildSidebar()
                self.selectProject(id: created.id)
            }
        }
    }

    /// Resolve the project the user right-clicked on in the sidebar.
    /// AppKit sets `NSOutlineView.clickedRow` to the row under the
    /// cursor at the moment the menu popped — which is what we want
    /// even if the row isn't the selected one. Returns `nil` if the
    /// click landed in empty space below the rows.
    @MainActor
    private func projectForClickedSidebarRow() -> ProjectSnapshot? {
        guard let outline = projectsOutlineView else { return nil }
        let row = outline.clickedRow
        guard row >= 0, row < projects.count else { return nil }
        return projects[row]
    }

    @objc @MainActor
    private func renameProjectFromMenu(_ sender: NSMenuItem) {
        guard let project = projectForClickedSidebarRow() else { return }
        let id = project.id

        let alert = NSAlert()
        alert.messageText = "Rename Project"
        alert.informativeText = "Choose a new name for \(project.name)."
        alert.addButton(withTitle: "Rename")
        alert.addButton(withTitle: "Cancel")
        let input = NSTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        input.stringValue = project.name
        alert.accessoryView = input
        alert.window.initialFirstResponder = input
        let response = alert.runModal()
        guard response == .alertFirstButtonReturn else { return }
        let newName = input.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !newName.isEmpty, newName != project.name else { return }

        let socketPath = self.socketPath
        Task { [weak self] in
            await renameProject(socketPath: socketPath, projectID: id, name: newName)
            await MainActor.run { [weak self] in
                guard let self else { return }
                if let idx = self.projects.firstIndex(where: { $0.id == id }) {
                    self.projects[idx] = ProjectSnapshot(
                        id: id,
                        name: newName,
                        cwd: self.projects[idx].cwd
                    )
                    self.rebuildSidebar()
                }
            }
        }
    }

    @objc @MainActor
    private func deleteProjectFromMenu(_ sender: NSMenuItem) {
        guard let project = projectForClickedSidebarRow() else { return }
        let id = project.id

        let alert = NSAlert()
        alert.messageText = "Delete \(project.name)?"
        alert.informativeText =
            "This will close every tab in the project. The action can't be undone."
        alert.addButton(withTitle: "Delete")
        alert.addButton(withTitle: "Cancel")
        alert.alertStyle = .warning
        guard alert.runModal() == .alertFirstButtonReturn else { return }

        // Close every UI-side TabSession in this project so the
        // StreamPty streams shut down before the daemon cascade-deletes
        // their rows. Without this the daemon-side CloseTab would race
        // the project DELETE — harmless but noisy in the logs.
        let condemned = tabs.filter { $0.projectID == id }
        for session in condemned {
            session.terminalView.removeFromSuperview()
            session.close(socketPath: socketPath)
        }
        tabs.removeAll { $0.projectID == id }
        activeSessionByProject.removeValue(forKey: id)

        let socketPath = self.socketPath
        Task { [weak self] in
            await deleteProject(socketPath: socketPath, projectID: id)
            await MainActor.run { [weak self] in
                guard let self else { return }
                self.projects.removeAll { $0.id == id }
                self.rebuildSidebar()
                if self.activeProjectID == id {
                    if let next = self.projects.first {
                        self.selectProject(id: next.id)
                    } else {
                        self.activeProjectID = nil
                        self.rebuildTabBar()
                        // Empty workspace — show nothing in the terminal area.
                        if let container = self.terminalContainer {
                            for subview in container.subviews {
                                subview.removeFromSuperview()
                            }
                        }
                    }
                }
            }
        }
    }

    // MARK: - Tab management

    private func tabsForActiveProject() -> [TabSession] {
        guard let activeProjectID else { return [] }
        return tabs.filter { $0.projectID == activeProjectID }
    }

    @MainActor
    private func openNewTab() {
        guard daemonReachable, let projectID = activeProjectID else { return }
        let session = TabSession(
            projectID: projectID,
            cols: 80,
            rows: 24,
            theme: activeTheme,
            font: activeFont
        )
        tabs.append(session)

        let projectTabs = tabsForActiveProject()
        let insertedIndex = projectTabs.count - 1
        rebuildTabBar()
        selectTab(at: insertedIndex)

        let title = "roost-mac \(insertedIndex + 1)"
        session.start(socketPath: socketPath, title: title) { [weak self] _ in
            // The id is now known; keep the window menu in sync so its
            // tag-driven ⌘1..⌘9 routes to the current tab order.
            self?.rebuildWindowMenu()
        }
    }

    @MainActor
    private func closeActiveTabImpl() {
        guard let activeProjectID else { return }
        let projectTabs = tabsForActiveProject()
        guard let active = activeSessionByProject[activeProjectID],
              let activeTabIndexInProject = projectTabs.firstIndex(where: { $0 === active })
        else { return }
        let session = projectTabs[activeTabIndexInProject]

        tabs.removeAll { $0 === session }
        activeSessionByProject.removeValue(forKey: activeProjectID)
        session.terminalView.removeFromSuperview()
        session.close(socketPath: socketPath)

        let remaining = tabsForActiveProject()
        if remaining.isEmpty {
            rebuildTabBar()
            if daemonReachable {
                openNewTab()
            }
            return
        }

        let nextIndex = min(activeTabIndexInProject, remaining.count - 1)
        rebuildTabBar()
        selectTab(at: nextIndex)
    }

    @MainActor
    private func selectTab(at indexInActiveProject: Int) {
        guard let activeProjectID else { return }
        let projectTabs = tabsForActiveProject()
        guard projectTabs.indices.contains(indexInActiveProject) else { return }
        guard let container = terminalContainer else { return }

        for subview in container.subviews {
            subview.removeFromSuperview()
        }

        let session = projectTabs[indexInActiveProject]
        let view = session.terminalView
        view.translatesAutoresizingMaskIntoConstraints = false
        view.clearSelection()
        container.addSubview(view)
        // Edge-pin instead of intrinsic-content-size pin so the
        // terminal fills whatever rectangle the container has.
        // `TerminalView.setFrameSize` recomputes cell-grid cols/rows
        // from the new bounds and propagates a PtyResize over
        // StreamPty (Phase 6a M3 / step 2g).
        NSLayoutConstraint.activate([
            view.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            view.topAnchor.constraint(equalTo: container.topAnchor),
            view.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            view.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        ])

        activeSessionByProject[activeProjectID] = session
        window?.makeFirstResponder(view)

        // Phase 6a P7: focusing a notified tab clears its badge
        // — fire ClearTabNotification daemon-side so every other
        // watching client converges via the broadcast event. Also
        // optimistically clear locally so the strip rebuild below
        // doesn't render a stale badge for one frame.
        if session.liveHasNotification, let tabID = session.id {
            session.liveHasNotification = false
            let socket = socketPath
            Task.detached {
                await clearTabNotification(socketPath: socket, tabID: tabID)
            }
        }
        rebuildTabBar()
    }

    @MainActor
    private func rebuildTabBar() {
        guard let tabBar = tabBar, let addTabButton = addTabButton else { return }
        for view in tabBar.arrangedSubviews where view !== addTabButton {
            tabBar.removeArrangedSubview(view)
            view.removeFromSuperview()
        }

        let projectTabs = tabsForActiveProject()
        let activeSession = activeProjectID.flatMap { activeSessionByProject[$0] }

        for (index, session) in projectTabs.enumerated() {
            let isActive = session === activeSession
            // Phase 6a P6 label: prefer title (OSC 0/1/2) if set,
            // else cwd (OSC 7) tilde-abbreviated, else "Tab N".
            // This is the visible payoff for P4 + P5 + P6 — the
            // pill stops saying "Tab N" once the shell emits OSCs.
            let pillTitle = pillLabel(for: session, index: index)
            let pill = TabPillView(
                index: index,
                title: pillTitle,
                isActive: isActive,
                statusColor: pillStatusColor(for: session),
                hasBadge: session.liveHasNotification,
                onSelect: { [weak self] idx in
                    self?.selectTab(at: idx)
                },
                onClose: { [weak self] idx in
                    self?.closeTab(at: idx)
                }
            )
            tabBar.insertArrangedSubview(pill, at: tabBar.arrangedSubviews.count - 1)
        }

        rebuildWindowMenu()
    }

    /// Close the tab at the given index in the active project. The
    /// only caller right now is `TabPillView.onClose` — the rest of
    /// the close paths route through `closeActiveTabImpl()` via the
    /// `⌘W` shortcut.
    @MainActor
    private func closeTab(at indexInActiveProject: Int) {
        guard let activeProjectID else { return }
        let projectTabs = tabsForActiveProject()
        guard projectTabs.indices.contains(indexInActiveProject) else { return }
        let session = projectTabs[indexInActiveProject]
        let wasActive = activeSessionByProject[activeProjectID] === session

        tabs.removeAll { $0 === session }
        if wasActive {
            activeSessionByProject.removeValue(forKey: activeProjectID)
        }
        session.terminalView.removeFromSuperview()
        session.close(socketPath: socketPath)

        let remaining = tabsForActiveProject()
        if remaining.isEmpty {
            rebuildTabBar()
            if daemonReachable { openNewTab() }
            return
        }
        rebuildTabBar()
        if wasActive {
            let nextIndex = min(indexInActiveProject, remaining.count - 1)
            selectTab(at: nextIndex)
        }
    }

    @MainActor
    private func rebuildWindowMenu() {
        guard let windowMenu = windowMenu else { return }
        windowMenu.removeAllItems()

        // Project switching first — defaults to ⌘1..⌘9 via
        // `switch_project_N` in `defaultBindingsMac()`. P1 keybind
        // table now drives both the key equivalent + modifier mask,
        // so a user's `keybind = alt+1 = switch_project_1` override
        // is honored.
        for (index, project) in projects.enumerated() {
            let item = NSMenuItem(
                title: project.name,
                action: #selector(selectProjectFromMenu(_:)),
                keyEquivalent: ""
            )
            item.target = self
            item.tag = Int(project.id)
            if project.id == activeProjectID {
                item.state = .on
            }
            if index < 9 {
                bind(item, to: KeybindAction.switchProject(index + 1))
            }
            windowMenu.addItem(item)
        }
        if !projects.isEmpty {
            windowMenu.addItem(.separator())
        }

        // Tab switching — defaults to ⌃1..⌃9 via `switch_tab_N` in
        // `defaultBindingsMac()`. Same keybind-table path.
        let projectTabs = tabsForActiveProject()
        let activeSession = activeProjectID.flatMap { activeSessionByProject[$0] }
        for (index, session) in projectTabs.enumerated() {
            let item = NSMenuItem(
                title: "Tab \(index + 1)",
                action: #selector(selectTabFromMenu(_:)),
                keyEquivalent: ""
            )
            item.target = self
            item.tag = index
            if session === activeSession {
                item.state = .on
            }
            if index < 9 {
                bind(item, to: KeybindAction.switchTab(index + 1))
            }
            windowMenu.addItem(item)
        }
        if !projectTabs.isEmpty {
            windowMenu.addItem(.separator())
        }

        let minimize = NSMenuItem(
            title: "Minimize",
            action: #selector(NSWindow.performMiniaturize(_:)),
            keyEquivalent: "m"
        )
        windowMenu.addItem(minimize)
        let zoom = NSMenuItem(
            title: "Zoom",
            action: #selector(NSWindow.performZoom(_:)),
            keyEquivalent: ""
        )
        windowMenu.addItem(zoom)
    }

    // MARK: - Menu actions

    @objc @MainActor
    private func newTab(_ sender: Any?) {
        openNewTab()
    }

    @objc @MainActor
    private func closeActiveTab(_ sender: Any?) {
        closeActiveTabImpl()
    }

    @objc @MainActor
    private func selectTabFromMenu(_ sender: NSMenuItem) {
        selectTab(at: sender.tag)
    }

    @objc @MainActor
    private func selectProjectFromMenu(_ sender: NSMenuItem) {
        let id = Int64(sender.tag)
        guard id != activeProjectID else { return }
        selectProject(id: id)
    }

    /// ⌘⇧R entry point — pulls up the same rename dialog as the
    /// right-click sidebar action, targeted at whichever project is
    /// currently shown.
    @objc @MainActor
    private func renameActiveProject(_ sender: Any?) {
        guard let id = activeProjectID else { return }
        // M3: reveal the sidebar so the user sees the project row
        // their rename will affect. Mirrors Go `app.go:1975`.
        ensureSidebarVisible()
        let placeholder = NSMenuItem()
        placeholder.tag = Int(id)
        renameProjectFromMenu(placeholder)
    }

    // MARK: - Menu installation

    @MainActor
    private func installMainMenu() {
        let mainMenu = NSMenu()

        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        appMenu.addItem(
            withTitle: "About Roost",
            action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: ""
        )
        appMenu.addItem(.separator())
        let hide = NSMenuItem(
            title: "Hide Roost",
            action: #selector(NSApplication.hide(_:)),
            keyEquivalent: "h"
        )
        appMenu.addItem(hide)
        let hideOthers = NSMenuItem(
            title: "Hide Others",
            action: #selector(NSApplication.hideOtherApplications(_:)),
            keyEquivalent: "h"
        )
        hideOthers.keyEquivalentModifierMask = [.command, .option]
        appMenu.addItem(hideOthers)
        appMenu.addItem(
            withTitle: "Show All",
            action: #selector(NSApplication.unhideAllApplications(_:)),
            keyEquivalent: ""
        )
        appMenu.addItem(.separator())
        appMenu.addItem(
            withTitle: "Quit Roost",
            action: #selector(NSApplication.terminate(_:)),
            keyEquivalent: "q"
        )
        appItem.submenu = appMenu
        mainMenu.addItem(appItem)

        // File menu — every shortcut driven through the keybind
        // table (Phase 6a P1). The hardcoded keyEquivalents that
        // used to live inline still apply by default (because
        // `defaultBindingsMac()` seeds them) but the user's
        // `~/.config/roost/config.conf` `keybind = … = …` lines
        // now layer cleanly on top.
        let fileItem = NSMenuItem()
        let fileMenu = NSMenu(title: "File")
        let newProjectItem = NSMenuItem(
            title: "New Project",
            action: #selector(newProject(_:)),
            keyEquivalent: ""
        )
        newProjectItem.target = self
        bind(newProjectItem, to: KeybindAction.newProject)
        fileMenu.addItem(newProjectItem)
        let newTabItem = NSMenuItem(
            title: "New Tab",
            action: #selector(newTab(_:)),
            keyEquivalent: ""
        )
        newTabItem.target = self
        bind(newTabItem, to: KeybindAction.newTab)
        fileMenu.addItem(newTabItem)
        let closeTabItem = NSMenuItem(
            title: "Close Tab",
            action: #selector(closeActiveTab(_:)),
            keyEquivalent: ""
        )
        closeTabItem.target = self
        bind(closeTabItem, to: KeybindAction.closeTab)
        fileMenu.addItem(closeTabItem)
        fileMenu.addItem(.separator())
        // M4: rename the active tab. ⌘R; pairs with ⌘⇧R for rename
        // project so the muscle-memory split mirrors Go's defaults.
        let renameTabItem = NSMenuItem(
            title: "Rename Tab…",
            action: #selector(renameActiveTab(_:)),
            keyEquivalent: ""
        )
        renameTabItem.target = self
        bind(renameTabItem, to: KeybindAction.renameTab)
        fileMenu.addItem(renameTabItem)
        let renameProjectItem = NSMenuItem(
            title: "Rename Project…",
            action: #selector(renameActiveProject(_:)),
            keyEquivalent: ""
        )
        renameProjectItem.target = self
        bind(renameProjectItem, to: KeybindAction.renameProject)
        fileMenu.addItem(renameProjectItem)
        fileMenu.addItem(.separator())
        // M4: cycle prev / next within the active project's tabs.
        // ⌘⇧[ / ⌘⇧]; wraps at ends. Matches Go cycle_tab_prev /
        // cycle_tab_next actions.
        let cyclePrevItem = NSMenuItem(
            title: "Previous Tab",
            action: #selector(cycleTabPrev(_:)),
            keyEquivalent: ""
        )
        cyclePrevItem.target = self
        bind(cyclePrevItem, to: KeybindAction.cycleTabPrev)
        fileMenu.addItem(cyclePrevItem)
        let cycleNextItem = NSMenuItem(
            title: "Next Tab",
            action: #selector(cycleTabNext(_:)),
            keyEquivalent: ""
        )
        cycleNextItem.target = self
        bind(cycleNextItem, to: KeybindAction.cycleTabNext)
        fileMenu.addItem(cycleNextItem)
        fileItem.submenu = fileMenu
        mainMenu.addItem(fileItem)

        // View menu (P2 font zoom) — same keybind-table lookup.
        let viewItem = NSMenuItem()
        let viewMenu = NSMenu(title: "View")
        let zoomInItem = NSMenuItem(
            title: "Zoom In",
            action: #selector(fontIncrease(_:)),
            keyEquivalent: ""
        )
        zoomInItem.target = self
        bind(zoomInItem, to: KeybindAction.fontIncrease)
        viewMenu.addItem(zoomInItem)
        let zoomOutItem = NSMenuItem(
            title: "Zoom Out",
            action: #selector(fontDecrease(_:)),
            keyEquivalent: ""
        )
        zoomOutItem.target = self
        bind(zoomOutItem, to: KeybindAction.fontDecrease)
        viewMenu.addItem(zoomOutItem)
        let zoomResetItem = NSMenuItem(
            title: "Actual Size",
            action: #selector(fontReset(_:)),
            keyEquivalent: ""
        )
        zoomResetItem.target = self
        bind(zoomResetItem, to: KeybindAction.fontReset)
        viewMenu.addItem(zoomResetItem)
        viewMenu.addItem(.separator())
        // M3: sidebar toggle. Routed through the standard responder
        // chain so the keybind config can override the default ⌘B.
        let toggleSidebarItem = NSMenuItem(
            title: "Toggle Sidebar",
            action: #selector(toggleSidebar(_:)),
            keyEquivalent: ""
        )
        toggleSidebarItem.target = self
        bind(toggleSidebarItem, to: KeybindAction.toggleSidebar)
        viewMenu.addItem(toggleSidebarItem)
        viewMenu.addItem(.separator())
        // Phase 6a P7: jump-to-unread shortcut.
        let jumpItem = NSMenuItem(
            title: "Jump to Unread",
            action: #selector(jumpToUnread(_:)),
            keyEquivalent: ""
        )
        jumpItem.target = self
        bind(jumpItem, to: KeybindAction.jumpToUnread)
        viewMenu.addItem(jumpItem)
        viewItem.submenu = viewMenu
        mainMenu.addItem(viewItem)

        // Edit menu — copy/paste route through NSText.copy /
        // NSText.paste selectors so AppKit's standard responder
        // chain reaches TerminalView's `@objc copy(_:)` /
        // `paste(_:)` (M5 wired those). Bind to the keybind
        // table for keyEquivalent — `copy` / `paste` actions in
        // the namespace can be overridden per user config.
        let editItem = NSMenuItem()
        let editMenu = NSMenu(title: "Edit")
        let cutItem = NSMenuItem(
            title: "Cut",
            action: #selector(NSText.cut(_:)),
            keyEquivalent: "x"
        )
        editMenu.addItem(cutItem)
        let copyItem = NSMenuItem(
            title: "Copy",
            action: #selector(NSText.copy(_:)),
            keyEquivalent: ""
        )
        bind(copyItem, to: KeybindAction.copy)
        editMenu.addItem(copyItem)
        let pasteItem = NSMenuItem(
            title: "Paste",
            action: #selector(NSText.paste(_:)),
            keyEquivalent: ""
        )
        bind(pasteItem, to: KeybindAction.paste)
        editMenu.addItem(pasteItem)
        editMenu.addItem(
            withTitle: "Select All",
            action: #selector(NSText.selectAll(_:)),
            keyEquivalent: "a"
        )
        editItem.submenu = editMenu
        mainMenu.addItem(editItem)

        let windowItem = NSMenuItem()
        let windowMenu = NSMenu(title: "Window")
        windowItem.submenu = windowMenu
        mainMenu.addItem(windowItem)
        self.windowMenu = windowMenu

        NSApp.mainMenu = mainMenu
        rebuildWindowMenu()
    }

    @MainActor
    private func applyIdentifyOutcome(_ outcome: IdentifyOutcome) {
        switch outcome {
        case .ok(let id):
            NSLog(
                "roost-mac: daemon connected pid=%d version=%@ proto=v%d active=project:%d/tab:%d",
                id.pid,
                id.daemonVersion,
                id.protocolVersion,
                id.activeProjectID,
                id.activeTabID
            )
        case .failed(let reason):
            NSLog("roost-mac: daemon not reachable: %@", reason)
            let alert = NSAlert()
            alert.alertStyle = .warning
            alert.messageText = "Can't reach the Roost daemon"
            alert.informativeText = """
                The Mac UI talks to `roost-core` over a Unix socket and \
                couldn't connect:

                \(reason)

                Start the daemon with `cargo run -p roost-core` and \
                relaunch the app.
                """
            alert.addButton(withTitle: "OK")
            if let window = self.window {
                alert.beginSheetModal(for: window, completionHandler: nil)
            } else {
                alert.runModal()
            }
        }
    }

    /// Mirror the active project's identity in the window chrome: the
    /// title becomes the project name and the subtitle becomes its cwd,
    /// matching the libadwaita `AdwWindowTitle` pattern the Go binary
    /// uses for the same window. Falls back to the plain product name
    /// before bootstrap has resolved a project.
    @MainActor
    private func updateWindowTitle() {
        guard let window else { return }
        if let activeProjectID,
           let project = projects.first(where: { $0.id == activeProjectID })
        {
            window.title = project.name.isEmpty ? "Roost" : project.name
            window.subtitle = project.cwd
        } else {
            window.title = "Roost"
            window.subtitle = ""
        }
    }

    /// Phase 6a M6: resolve the user's requested font into an
    /// NSFont, falling back gracefully when the requested family
    /// isn't installed. macOS's font fallback for monospaced text
    /// is unreliable on a missing family, so we explicitly probe
    /// before returning. `NSFont(name:size:)` returns nil for an
    /// unknown family, so a single nil check is enough.
    private func resolveFont(family: String?, size: CGFloat) -> NSFont {
        if let family,
           !family.isEmpty,
           let f = NSFont(name: family, size: size)
        {
            return f
        }
        // No family or unknown → system monospace. Same default the
        // Go binary uses when `font-family` is unset.
        return NSFont.monospacedSystemFont(ofSize: size, weight: .regular)
    }

    // MARK: - Tab pill labels (Phase 6a P6)

    /// Compute the label string for a tab's pill in the strip.
    /// Priority: live title (OSC 0/1/2) -> tilde-abbreviated cwd
    /// (OSC 7) -> "Tab N" fallback. P6's WatchEvents handlers
    /// populate `liveTitle` / `liveCwd` on the TabSession;
    /// rebuildTabBar calls this each time the strip is rebuilt.
    @MainActor
    private func pillLabel(for session: TabSession, index: Int) -> String {
        if let t = session.liveTitle, !t.isEmpty { return t }
        if let cwd = session.liveCwd, !cwd.isEmpty {
            return tildeAbbreviate(cwd)
        }
        return "Tab \(index + 1)"
    }

    /// Tilde-abbreviate `$HOME` prefixes in a path. Matches the Go
    /// binary's `cmd/roost/path_display.go::tildeAbbreviate`.
    private func tildeAbbreviate(_ path: String) -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        if path == home { return "~" }
        if path.hasPrefix(home + "/") {
            return "~" + path.dropFirst(home.count)
        }
        return path
    }

    /// Resolve the status-dot color for a tab pill based on
    /// `liveState`. The proto's `TabState` enum: 0=NONE,
    /// 1=RUNNING (green), 2=NEEDS_INPUT (yellow), 3=IDLE (gray),
    /// 4=ERROR (red). Returns nil for NONE / unknown — TabPillView
    /// then draws no dot (M3's empty slot).
    @MainActor
    private func pillStatusColor(for session: TabSession) -> NSColor? {
        guard let state = session.liveState else { return nil }
        switch state {
        case 1: return .systemGreen
        case 2: return .systemYellow
        case 3: return .systemGray
        case 4: return .systemRed
        default: return nil
        }
    }

    // MARK: - Keybind table → NSMenuItem (Phase 6a P1)

    /// Look up the first `Accel` in `self.keybinds` whose action
    /// matches `action`. Returns the canonical `(keyEquivalent,
    /// modifiers)` pair to install on an `NSMenuItem`. Returns an
    /// empty tuple when the action has no entry — produces an
    /// effectively unbound menu item rather than crashing or
    /// throwing on a user's `keybind = … = unbind` of a default.
    @MainActor
    private func accel(for action: String) -> (String, NSEvent.ModifierFlags) {
        for (accel, act) in keybinds where act == action {
            return (accel.key, accel.modifiers)
        }
        return ("", [])
    }

    /// Configure an NSMenuItem with the resolved keybind for
    /// `action`. Centralizes the "look up + assign" pattern so
    /// `installMainMenu` stays readable.
    @MainActor
    private func bind(_ item: NSMenuItem, to action: String) {
        let (key, mask) = accel(for: action)
        item.keyEquivalent = key
        item.keyEquivalentModifierMask = mask
    }

    // MARK: - Cycle + rename tab (M4)

    /// Move focus to the previous tab in the active project, wrapping
    /// from the first to the last. ⌘⇧[ by default.
    /// Mirrors Go `cmd/roost/app.go::cycleTab(delta=-1)`.
    @objc @MainActor
    private func cycleTabPrev(_ sender: Any?) {
        cycleTab(delta: -1)
    }

    /// Move focus to the next tab in the active project, wrapping
    /// from the last to the first. ⌘⇧] by default.
    /// Mirrors Go `cmd/roost/app.go::cycleTab(delta=+1)`.
    @objc @MainActor
    private func cycleTabNext(_ sender: Any?) {
        cycleTab(delta: 1)
    }

    @MainActor
    private func cycleTab(delta: Int) {
        guard let activeProjectID else { return }
        let projectTabs = tabsForActiveProject()
        guard !projectTabs.isEmpty else { return }
        let active = activeSessionByProject[activeProjectID]
        let currentIdx = projectTabs.firstIndex(where: { $0 === active }) ?? 0
        let n = projectTabs.count
        // ((i + delta) % n + n) % n for negative-safe modulo.
        let next = ((currentIdx + delta) % n + n) % n
        selectTab(at: next)
    }

    /// Rename the active tab via an NSAlert + text field, the same
    /// idiom `renameProjectFromMenu` uses. On commit the daemon
    /// sets the per-tab `user_titled` lock so shell-emitted OSC 1/2
    /// stops overwriting. ⌘R by default (Mac convention "rename" —
    /// also matches Go binary's `cmd/roost/app.go::renameActiveTab`).
    @objc @MainActor
    private func renameActiveTab(_ sender: Any?) {
        guard let activeProjectID,
              let session = activeSessionByProject[activeProjectID],
              let tabID = session.id
        else {
            return
        }
        let projectTabs = tabsForActiveProject()
        let index = projectTabs.firstIndex(where: { $0 === session }) ?? 0
        let currentTitle = pillLabel(for: session, index: index)

        let alert = NSAlert()
        alert.messageText = "Rename Tab"
        alert.informativeText = "Choose a new name for this tab. The shell can no longer change it after this."
        alert.addButton(withTitle: "Rename")
        alert.addButton(withTitle: "Cancel")
        let input = NSTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        input.stringValue = currentTitle
        alert.accessoryView = input
        alert.window.initialFirstResponder = input
        let response = alert.runModal()
        guard response == .alertFirstButtonReturn else { return }
        let newTitle = input.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !newTitle.isEmpty, newTitle != currentTitle else { return }

        // Optimistic local update so the pill flips immediately. The
        // daemon's TabTitleChangedEvent will reconcile if anything
        // drifts (shouldn't happen since the daemon accepts blindly).
        session.liveTitle = newTitle
        rebuildTabBar()

        let socketPath = self.socketPath
        Task {
            await setTabTitle(socketPath: socketPath, tabID: tabID, title: newTitle)
        }
    }

    // MARK: - Sidebar toggle (M3)

    /// `toggle_sidebar` action handler. Flips `sidebarPane.isHidden`
    /// (NSSplitView collapses hidden arranged subviews automatically),
    /// then writes the new state to UserDefaults so it survives across
    /// launches. Bound to ⌘B by default in `Keybind.swift`, overrideable
    /// via the config file's `keybind = … = toggle_sidebar` line.
    @objc @MainActor
    private func toggleSidebar(_ sender: Any?) {
        guard let sidebarPane else { return }
        let nextHidden = !sidebarPane.isHidden
        sidebarPane.isHidden = nextHidden
        UserDefaults.standard.set(!nextHidden, forKey: Self.sidebarVisibleDefaultsKey)
    }

    /// Force the sidebar visible without toggling. Called from the
    /// three user actions where Go (`cmd/roost/app.go:1337,1487,1975`)
    /// auto-expands the sidebar so the user sees the affected row:
    /// `newProject` (sidebar shows the freshly-created project),
    /// `selectProject` (the ⌘1-9 switcher reveals the focused project),
    /// and `beginRenameActiveProject` (M4 hookup — rename popover
    /// needs the row visible to anchor against).
    @MainActor
    private func ensureSidebarVisible() {
        guard let sidebarPane, sidebarPane.isHidden else { return }
        sidebarPane.isHidden = false
        UserDefaults.standard.set(true, forKey: Self.sidebarVisibleDefaultsKey)
    }

    // MARK: - Jump to next unread (Phase 6a P7)

    /// Find the next tab with `liveHasNotification == true` and
    /// focus it. Search order: tabs in the active project after
    /// the current index first (cycle through within project),
    /// then any tab in any other project. No-op if no tab has a
    /// pending notification.
    ///
    /// Default binding is `⌘⇧U` (cmux convention), overrideable
    /// via the P1 keybind table.
    @objc @MainActor
    private func jumpToUnread(_ sender: Any?) {
        // Walk current project first, then other projects, until
        // we find the first notified tab. Stable iteration order:
        // `tabs` array order within each project.
        if let activeID = activeProjectID {
            let activeTabs = tabs.filter { $0.projectID == activeID }
            let activeFocus = activeSessionByProject[activeID]
            let startIdx: Int
            if let activeFocus,
               let i = activeTabs.firstIndex(where: { $0 === activeFocus })
            {
                startIdx = i + 1
            } else {
                startIdx = 0
            }
            for offset in 0..<activeTabs.count {
                let idx = (startIdx + offset) % activeTabs.count
                if activeTabs[idx].liveHasNotification {
                    selectTab(at: idx)
                    return
                }
            }
        }
        // Search other projects in order.
        for project in projects where project.id != activeProjectID {
            let projectTabs = tabs
                .filter { $0.projectID == project.id }
            if let first = projectTabs.first(where: { $0.liveHasNotification }) {
                selectProject(id: project.id)
                if let idx = tabs
                    .filter({ $0.projectID == project.id })
                    .firstIndex(where: { $0 === first })
                {
                    selectTab(at: idx)
                }
                return
            }
        }
    }

    // MARK: - Font zoom (Phase 6a P2)

    /// Lower bound on the cell-grid font size. Smaller renders cell
    /// metrics that collapse the cursor / glyph into the wrong
    /// rect. The Go binary uses the same floor.
    private static let fontSizeMin: CGFloat = 8
    /// Upper bound on the cell-grid font size. Anything larger and a
    /// single tab's terminal eats the whole window before a useful
    /// shell prompt can render.
    private static let fontSizeMax: CGFloat = 32

    @objc @MainActor
    private func fontIncrease(_ sender: Any?) {
        adjustFont(by: +1)
    }

    @objc @MainActor
    private func fontDecrease(_ sender: Any?) {
        adjustFont(by: -1)
    }

    @objc @MainActor
    private func fontReset(_ sender: Any?) {
        let defaultSize = config.fontSize ?? 14
        applyFont(size: defaultSize)
    }

    /// Bump the font size in 1pt increments, clamped to
    /// `[fontSizeMin, fontSizeMax]`. The change is applied
    /// uniformly across every TabSession's TerminalView — global
    /// zoom, mirroring the Go binary's behaviour. (Per-tab zoom
    /// would need a TabSession-side stored size and a more
    /// elaborate keybind dispatch; not worth the complexity for the
    /// audience this serves.)
    @MainActor
    private func adjustFont(by delta: CGFloat) {
        let currentSize = activeFont.pointSize
        let proposed = (currentSize + delta).rounded()
        let clamped = max(Self.fontSizeMin, min(Self.fontSizeMax, proposed))
        if clamped == currentSize { return }
        applyFont(size: clamped)
    }

    /// Build a new NSFont at `size` (respecting the user's
    /// `font-family` config) and push it into every live
    /// TerminalView. The Mac UI's M3 reflow path picks up the new
    /// cell metrics + propagates a PtyResize over StreamPty
    /// automatically — no separate plumbing needed here.
    @MainActor
    private func applyFont(size: CGFloat) {
        let newFont = resolveFont(family: config.fontFamily, size: size)
        activeFont = newFont
        for session in tabs {
            session.terminalView.updateFont(newFont)
        }
    }

    /// Resolve the same default socket path as `roost-core`'s
    /// `default_socket_path` for macOS — always
    /// `~/Library/Caches/roost/roost.sock` when `HOME` is set;
    /// `/tmp/roost.sock` only as a last resort.
    nonisolated static func defaultSocketPath(
        environment env: [String: String] = ProcessInfo.processInfo.environment
    ) -> String {
        if let home = env["HOME"], !home.isEmpty, home.hasPrefix("/") {
            return "\(home)/Library/Caches/roost/roost.sock"
        }
        return "/tmp/roost.sock"
    }
}

// MARK: - Tab strip

/// One pill in the tab strip. Custom `NSView` so the layout is:
///   [status-dot slot 10x10] [label] [× close on active]
/// inside a rounded background that flips between two tints based
/// on active state. Clicks anywhere on the pill (except the close
/// glyph) fire `onSelect(index)`; the close glyph fires
/// `onClose(index)`. The status-dot slot draws nothing in M3 — it
/// goes live in Phase 6b when `TabStateChangedEvent` lands.
@MainActor
final class TabPillView: NSView {
    private let index: Int
    private let isActive: Bool
    private let onSelect: @MainActor (Int) -> Void
    private let onClose: @MainActor (Int) -> Void

    private let label: NSTextField
    private let closeButton: NSButton

    init(
        index: Int,
        title: String,
        isActive: Bool,
        statusColor: NSColor? = nil,
        hasBadge: Bool = false,
        onSelect: @escaping @MainActor (Int) -> Void,
        onClose: @escaping @MainActor (Int) -> Void
    ) {
        self.index = index
        self.isActive = isActive
        self.onSelect = onSelect
        self.onClose = onClose

        self.label = NSTextField(labelWithString: title)
        self.label.font = .systemFont(
            ofSize: 12,
            weight: isActive ? .medium : .regular
        )
        self.label.textColor = isActive ? .labelColor : .secondaryLabelColor
        self.label.lineBreakMode = .byTruncatingTail
        self.label.maximumNumberOfLines = 1
        self.label.translatesAutoresizingMaskIntoConstraints = false

        self.closeButton = NSButton(title: "×", target: nil, action: nil)
        self.closeButton.isBordered = false
        self.closeButton.font = .systemFont(ofSize: 13, weight: .regular)
        self.closeButton.contentTintColor = .secondaryLabelColor
        self.closeButton.isHidden = !isActive
        self.closeButton.translatesAutoresizingMaskIntoConstraints = false

        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 6
        layer?.backgroundColor = (isActive
            ? NSColor.controlAccentColor.withAlphaComponent(0.18)
            : NSColor.clear).cgColor

        // Status-dot slot — reserves 10pt on the leading edge so
        // the label position is stable when the dot turns on. P6
        // wires `TabStateChangedEvent` to fill `statusColor`.
        let statusSlot = NSView()
        statusSlot.translatesAutoresizingMaskIntoConstraints = false
        statusSlot.wantsLayer = true
        if let color = statusColor {
            statusSlot.layer?.backgroundColor = color.cgColor
            statusSlot.layer?.cornerRadius = 5
        }
        // Phase 6a P7: trailing accent-dot badge. Visible only on
        // inactive notified pills (active pills are about to be
        // cleared by the focus path; no need to badge the one the
        // user is already looking at). Shares the same 16×16
        // trailing slot as the close button — `closeButton.isHidden`
        // is true on inactive pills so the slot is free.
        let badgeDot = NSView()
        badgeDot.translatesAutoresizingMaskIntoConstraints = false
        badgeDot.wantsLayer = true
        badgeDot.layer?.backgroundColor = NSColor.controlAccentColor.cgColor
        badgeDot.layer?.cornerRadius = 4
        badgeDot.isHidden = !(hasBadge && !isActive)
        addSubview(statusSlot)
        addSubview(label)
        addSubview(closeButton)
        addSubview(badgeDot)

        // `closeButton.target/action` need self to be a captured
        // weak reference so the pill doesn't leak through the
        // closure → AppKit retain cycle.
        closeButton.target = self
        closeButton.action = #selector(closeClicked)

        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: 24),

            statusSlot.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),
            statusSlot.centerYAnchor.constraint(equalTo: centerYAnchor),
            statusSlot.widthAnchor.constraint(equalToConstant: 10),
            statusSlot.heightAnchor.constraint(equalToConstant: 10),

            label.leadingAnchor.constraint(equalTo: statusSlot.trailingAnchor, constant: 6),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),

            closeButton.leadingAnchor.constraint(
                greaterThanOrEqualTo: label.trailingAnchor,
                constant: 6
            ),
            closeButton.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -6),
            closeButton.centerYAnchor.constraint(equalTo: centerYAnchor),
            closeButton.widthAnchor.constraint(equalToConstant: 16),
            closeButton.heightAnchor.constraint(equalToConstant: 16),

            // Badge dot — same trailing slot the close button
            // occupies on active pills. Inactive notified pills
            // have closeButton hidden, so the slot is free for
            // the dot. 8×8 centered in the 16-slot.
            badgeDot.centerXAnchor.constraint(equalTo: closeButton.centerXAnchor),
            badgeDot.centerYAnchor.constraint(equalTo: centerYAnchor),
            badgeDot.widthAnchor.constraint(equalToConstant: 8),
            badgeDot.heightAnchor.constraint(equalToConstant: 8),

            // Trailing padding for the label — same fixed -28
            // either way now so the slot is always available for
            // either close button (active) or badge (notified).
            label.trailingAnchor.constraint(
                lessThanOrEqualTo: trailingAnchor,
                constant: -28
            ),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    /// Single-click anywhere on the pill (except over the close
    /// glyph) selects the tab. `mouseDown` short-circuits AppKit's
    /// drag tracking, which is what we want — clicks shouldn't have
    /// to wait for a drag-threshold timeout to fire.
    override func mouseDown(with event: NSEvent) {
        // The close button intercepts its own clicks via the
        // NSButton's hit-testing; if the event reaches the pill it
        // wasn't over the close button.
        onSelect(index)
    }

    @objc private func closeClicked() {
        onClose(index)
    }
}

// MARK: - Sidebar NSOutlineView data source + delegate

/// Cell view for one project row. Pulled out so the outline view's
/// `viewFor:` delegate path stays a one-liner. `NSTableCellView`'s
/// built-in `textField` outlet is what AppKit's source-list styling
/// targets for selection-state color flips, so we wire our label
/// through that outlet rather than holding a separate `NSTextField`
/// reference.
@MainActor
final class ProjectRowCellView: NSTableCellView {
    /// Phase 6a P7: small accent-tinted dot in the right column
    /// when any tab in the project has a pending notification.
    /// Hidden by default; `configure(with:notifying:)` flips it.
    private let badgeDot: NSView

    init() {
        let field = NSTextField(labelWithString: "")
        field.translatesAutoresizingMaskIntoConstraints = false
        field.lineBreakMode = .byTruncatingTail
        field.maximumNumberOfLines = 1
        field.usesSingleLineMode = true
        field.font = .systemFont(ofSize: 13)
        field.allowsDefaultTighteningForTruncation = true

        let dot = NSView()
        dot.translatesAutoresizingMaskIntoConstraints = false
        dot.wantsLayer = true
        dot.layer?.backgroundColor = NSColor.controlAccentColor.cgColor
        dot.layer?.cornerRadius = 4
        dot.isHidden = true
        self.badgeDot = dot

        super.init(frame: .zero)
        addSubview(field)
        addSubview(dot)
        textField = field
        NSLayoutConstraint.activate([
            field.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 2),
            field.trailingAnchor.constraint(equalTo: dot.leadingAnchor, constant: -6),
            field.centerYAnchor.constraint(equalTo: centerYAnchor),

            dot.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            dot.centerYAnchor.constraint(equalTo: centerYAnchor),
            dot.widthAnchor.constraint(equalToConstant: 8),
            dot.heightAnchor.constraint(equalToConstant: 8),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    func configure(with project: ProjectSnapshot, notifying: Bool) {
        textField?.stringValue = project.name
        badgeDot.isHidden = !notifying
    }
}

extension RoostApp: NSOutlineViewDataSource {
    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        item == nil ? projectCountForSidebar() : 0
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        // The model is flat — there's never a non-nil parent. Return
        // the project at `index` boxed in a tiny reference type so
        // NSOutlineView's identity-based caching stays consistent.
        projectRowItem(at: index)
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        false
    }
}

extension RoostApp: NSOutlineViewDelegate {
    func outlineView(
        _ outlineView: NSOutlineView,
        viewFor tableColumn: NSTableColumn?,
        item: Any
    ) -> NSView? {
        guard let row = item as? ProjectRowItem else { return nil }
        let cell = ProjectRowCellView()
        // Phase 6a P7 per-project rollup: badge the sidebar row
        // if ANY tab in this project has a pending notification.
        let notifying = tabs
            .contains { $0.projectID == row.project.id && $0.liveHasNotification }
        cell.configure(with: row.project, notifying: notifying)
        return cell
    }

    func outlineViewSelectionDidChange(_ notification: Notification) {
        guard !isSyncingSidebarSelection else { return }
        guard let outline = projectsOutlineView else { return }
        let row = outline.selectedRow
        guard row >= 0, row < projects.count else { return }
        let projectID = projects[row].id
        guard projectID != activeProjectID else { return }
        selectProject(id: projectID)
    }
}

extension RoostApp: NSMenuDelegate {
    /// Gray out menu items when the user right-clicks below the last
    /// row (`clickedRow == -1`) — `Rename` / `Delete` have nothing to
    /// act on in that case.
    func menuNeedsUpdate(_ menu: NSMenu) {
        let valid = projectForClickedSidebarRow() != nil
        for item in menu.items {
            item.isEnabled = valid
        }
    }
}

// Reference-typed row item — NSOutlineView caches items by identity,
// so passing a value type through `child(ofItem:)` would defeat the
// outline view's row-recycling. Wrapping `ProjectSnapshot` in a class
// gives the outline view a stable reference per project for the
// duration of a single `reloadData` cycle.
@MainActor
final class ProjectRowItem {
    let project: ProjectSnapshot
    init(_ project: ProjectSnapshot) { self.project = project }
}

extension RoostApp {
    /// Bridge between the outline view's flat data-source API and
    /// our `projects` array. Returns one `ProjectRowItem` per project,
    /// rebuilt fresh on every `reloadData()` cycle (cheap — bounded
    /// by the number of projects the user has).
    @MainActor
    fileprivate func projectCountForSidebar() -> Int {
        projects.count
    }

    @MainActor
    fileprivate func projectRowItem(at index: Int) -> ProjectRowItem {
        ProjectRowItem(projects[index])
    }
}
