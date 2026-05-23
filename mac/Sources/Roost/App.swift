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

    private var tabBar: TabBarStackView?
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

    /// Round-6 R6.B: persisted sidebar width, plus the clamp bounds
    /// used by the `NSSplitViewDelegate` callbacks. Width survives
    /// quit/relaunch via `UserDefaults`. Bounds: 160pt floor (any
    /// narrower and the "Untitled N" rows truncate awkwardly under
    /// the body font), 400pt cap (past that the terminal grid
    /// shrinks too aggressively on a 1200pt window).
    static let sidebarMinWidth: CGFloat = 160
    static let sidebarMaxWidth: CGFloat = 400
    private static let sidebarWidthDefaultsKey = "RoostSidebarWidth"

    /// Round-6 R6.B: gate the `splitViewDidResizeSubviews` save
    /// path. NSSplitView fires the resize-did callback DURING the
    /// initial layout pass, before our width constraints + the
    /// `setPosition(_:ofDividerAt:)` call in
    /// `applicationDidFinishLaunching` have settled — without this
    /// flag, that first callback would persist whatever
    /// NSSplitView's auto-layout picked (often > sidebarMaxWidth)
    /// to UserDefaults, polluting the saved value before the user
    /// has even seen the window. Flipped to `true` at the very end
    /// of `applicationDidFinishLaunching`.
    private var sidebarPersistenceActive = false

    /// Round-7 R7.A: references to the sidebar pane's width
    /// constraints so `toggleSidebar` can deactivate the `>= min`
    /// floor during a programmatic collapse. NSSplitView's
    /// `setPosition(0, …)` is otherwise clamped by either the
    /// constraint or the `constrainMinCoordinate` delegate; this
    /// flow opts out of both for the ⌘B-driven collapse without
    /// loosening the interactive-drag bounds.
    private var sidebarMinWidthConstraint: NSLayoutConstraint?
    private var sidebarMaxWidthConstraint: NSLayoutConstraint?
    private var sidebarPreferredWidthConstraint: NSLayoutConstraint?

    /// Round-7 R7.A: while `sidebarCollapsingProgrammatically` is
    /// true, `constrainMinCoordinate` returns 0 instead of
    /// `sidebarMinWidth` so `setPosition(0, …)` can collapse the
    /// pane all the way. Reset to false on the next runloop pass.
    private var sidebarCollapsingProgrammatically = false

    /// Round-7 R7.A: width to restore when ⌘B re-opens the sidebar.
    /// Captured at collapse-time from `sidebarPane.frame.width` so
    /// the user lands at exactly the width they had before pressing
    /// ⌘B. Falls back to 220pt for the very first restore (i.e.
    /// the app launched with the sidebar already hidden).
    private var sidebarRestoreWidth: CGFloat = 220

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

    /// M5 of `goal-mac-parity-2026-05-18.md`: cell views per project,
    /// reused across `reloadData` so inline rename's typing buffer
    /// survives a sibling-driven sidebar refresh. The Mac UI grows
    /// at most one row per project the user creates — bounded, and
    /// purged on `.projectDeleted`.
    private var projectRowCellViews: [Int64: ProjectRowCellView] = [:]

    /// Round-3 R5: cached tab pill views keyed by daemon tab id, so
    /// the pill survives `rebuildTabBar` while the user is mid-inline-
    /// rename. Mirrors the sidebar `projectRowCellViews` pattern.
    /// Purged on `.tabDeleted` + `closeTab` + project deletion.
    private var tabPillViews: [Int64: TabPillView] = [:]

    /// Round-2 F6: drop-indicator state for sidebar drag-reorder.
    /// `nil` means no drag in progress. Non-nil values are the
    /// `proposedChildIndex` AppKit reports — drop will land above the
    /// project at that index, or after the last project if index ==
    /// projects.count. Cleared on `acceptDrop` and on session-end
    /// callbacks.
    private var dropIndicatorIndex: Int?

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
        // M4b3a (daemon-removal refactor): stand up the
        // post-daemon backend immediately — Workspace +
        // PtySupervisor + IPC server. The IPC socket goes live
        // before any UI work so `roostctl` invocations during
        // app launch don't race the bind.
        let profile = BundleProfile.mac()
        RoostBackend.shared.start(profile: profile)

        let socketPath = profile.socketPath
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
        // Round-6 R6.B: load the user's last sidebar width (clamped
        // to the configured min/max). `UserDefaults.double(forKey:)`
        // returns 0 when the key is absent; the ternary below treats
        // 0 as "first launch, use default 220pt" — matching the
        // pre-round-6 fixed default. A stored value outside the
        // [min, max] band is clamped (which is also the implicit
        // recovery path for a previously-saved out-of-range width).
        let storedSidebarWidth = UserDefaults.standard.double(forKey: Self.sidebarWidthDefaultsKey)
        let sidebarWidth: CGFloat = {
            let candidate = storedSidebarWidth > 0 ? CGFloat(storedSidebarWidth) : 220
            return max(Self.sidebarMinWidth, min(Self.sidebarMaxWidth, candidate))
        }()
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
        // Round-6 R6.B: NSSplitViewDelegate callbacks clamp the
        // user's interactive drag to [sidebarMinWidth, sidebarMaxWidth]
        // and persist the result via `splitViewDidResizeSubviews`.
        split.delegate = self

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
        // Round-6 R6.B / Round-7 R7.A: remember the launch-time
        // restore width so toggling back from a fresh-launch
        // collapsed state lands at the user's last expanded width.
        self.sidebarRestoreWidth = sidebarWidth
        // Restore the user's last-known toggle state (M3). UserDefaults
        // returns `false` for an unset key, so we read it back as
        // Optional<Bool>-shaped to distinguish "not set" (default
        // visible) from "explicitly false" (user hid it).
        let stored = UserDefaults.standard.object(forKey: Self.sidebarVisibleDefaultsKey) as? Bool
        let startsCollapsed = stored == false

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

        // Round-6 R6.B: seat the divider at the persisted (or
        // default) sidebar width AFTER the window has been ordered
        // front and the initial layout pass has settled. Calling
        // `setPosition` earlier (right after `addArrangedSubview`)
        // happens before the split view has a meaningful frame in
        // the window's coord space, so the position is silently
        // discarded.
        let initialSidebarWidth = sidebarWidth
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            if let split = self.sidebarPane?.superview as? NSSplitView {
                if startsCollapsed {
                    // Round-7 R7.A: launch into the collapsed state.
                    // Same path as `toggleSidebar`'s collapse arm —
                    // deactivate `>= min` so setPosition(0) sticks.
                    self.sidebarPreferredWidthConstraint?.constant = 0
                    self.sidebarMinWidthConstraint?.isActive = false
                    self.sidebarMaxWidthConstraint?.isActive = false
                    self.sidebarCollapsingProgrammatically = true
                    split.setPosition(0, ofDividerAt: 0)
                    self.sidebarCollapsingProgrammatically = false
                } else {
                    split.setPosition(initialSidebarWidth, ofDividerAt: 0)
                }
            }
            // Now that the splitter is at its intended position,
            // enable the persistence callback. Any `frame.width`
            // we save from here on reflects a real user drag, not
            // the pre-settle default NSSplitView picks.
            self.sidebarPersistenceActive = true
        }

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

        // M3 drag-and-drop: rows are reorderable within the projects
        // section. Source-side pasteboard writing happens via
        // `outlineView(_:pasteboardWriterForItem:)`; the drop side
        // uses `roostProjectID` UTI so we don't accept arbitrary text.
        outline.registerForDraggedTypes([.roostProjectID])
        outline.setDraggingSourceOperationMask(.move, forLocal: true)
        outline.setDraggingSourceOperationMask([], forLocal: false)
        // Round-7 R7.C-extra: suppress AppKit's built-in thin-blue
        // outline around the whole outline view during a drop. Our
        // `ProjectRowCellView.setDropIndicator` accent band is the
        // user-visible cue; the AppKit indicator both duplicates
        // this and wraps the entire scrollable area in a single
        // outline that's visually noisy in dark theme.
        outline.draggingDestinationFeedbackStyle = .none

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

        // Round-6 R6.B: preferred width constraint at `.defaultHigh`
        // priority. NSSplitView's `setHoldingPriority(.defaultHigh,
        // forSubviewAt: 0)` resists resize against constraints
        // weaker than its holding priority and yields against ones
        // stronger — pairing them at the same priority makes the
        // sidebar hold this width across window resizes while still
        // allowing the user's interactive drag (which goes through
        // NSSplitView's gesture pipeline, not Auto Layout) to
        // override. Without this constraint, NSSplitView picks an
        // arbitrary default (often half the window width).
        let preferredWidth = pane.widthAnchor.constraint(equalToConstant: width)
        preferredWidth.priority = .defaultHigh

        // Round-7 R7.A: store the min/max width constraints by
        // reference so `toggleSidebar` can deactivate `>= min`
        // during a programmatic collapse — NSSplitView can't shrink
        // the pane below 160 while that constraint is active, and
        // the priority-dance approach (constraint at .defaultHigh+1,
        // hoping the implicit 0-width override wins) is unreliable
        // on macOS. Direct deactivate/reactivate is explicit and
        // robust.
        let minWidth = pane.widthAnchor.constraint(
            greaterThanOrEqualToConstant: Self.sidebarMinWidth
        )
        let maxWidth = pane.widthAnchor.constraint(
            lessThanOrEqualToConstant: Self.sidebarMaxWidth
        )
        self.sidebarMinWidthConstraint = minWidth
        self.sidebarMaxWidthConstraint = maxWidth
        self.sidebarPreferredWidthConstraint = preferredWidth

        NSLayoutConstraint.activate([
            minWidth,
            maxWidth,
            // Preferred width — anchors NSSplitView's auto-layout
            // pick. Initial position is also set explicitly via
            // `setPosition(_:ofDividerAt:)` after the window orders
            // front (see applicationDidFinishLaunching).
            preferredWidth,

            header.topAnchor.constraint(equalTo: pane.topAnchor, constant: 12),
            header.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 16),
            header.trailingAnchor.constraint(equalTo: pane.trailingAnchor, constant: -12),

            scrollView.topAnchor.constraint(equalTo: header.bottomAnchor, constant: 6),
            scrollView.leadingAnchor.constraint(equalTo: pane.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: pane.trailingAnchor),
            scrollView.bottomAnchor.constraint(equalTo: addProject.topAnchor, constant: -8),

            // Round-6 R6.A: center the + New Project footer button.
            // A single full-width footer affordance reads cleaner as
            // a primary action when centered (Calendar, Safari follow
            // the same pattern). HIG permits both; left-anchored
            // becomes ambiguous when the sidebar is wider than its
            // default 220pt (round-6 R6.B adds resize).
            addProject.centerXAnchor.constraint(equalTo: pane.centerXAnchor),
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

        let tabBar = TabBarStackView()
        tabBar.orientation = .horizontal
        tabBar.alignment = .centerY
        tabBar.spacing = 6
        tabBar.translatesAutoresizingMaskIntoConstraints = false
        tabBar.onDropTab = { [weak self] sourceTabID, rawTargetIdx in
            self?.handleTabDrop(sourceTabID: sourceTabID, rawTargetIdx: rawTargetIdx)
        }

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

        // Wrap the tab strip in an NSScrollView so that adding tabs
        // beyond the available width scrolls horizontally instead of
        // pushing the window wider. The stack view is the document
        // view; its intrinsic content size grows with its arranged
        // subviews, and the scroll view exposes whatever fits in the
        // pane's width.
        //
        // Round-3 R1: use `TabBarScrollView` (a thin subclass) so drag
        // events delivered to the scroll-view layer are forwarded to
        // the `TabBarStackView` document view. Without this, AppKit's
        // drag walk stops at the clip view and the stack view's
        // `performDragOperation` never fires. Elasticity is `.none`
        // because the rubber-band responder on the edges can swallow
        // drop events along the strip's margins.
        let tabScroll = TabBarScrollView()
        tabScroll.translatesAutoresizingMaskIntoConstraints = false
        tabScroll.hasHorizontalScroller = false
        tabScroll.hasVerticalScroller = false
        tabScroll.horizontalScrollElasticity = .none
        tabScroll.verticalScrollElasticity = .none
        tabScroll.borderType = .noBorder
        tabScroll.drawsBackground = false
        tabScroll.scrollerStyle = .overlay
        tabScroll.documentView = tabBar
        pane.addSubview(tabScroll)

        let terminalContainer = NSView()
        terminalContainer.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(terminalContainer)

        NSLayoutConstraint.activate([
            // Round-3 R3: match the Go GTK binary's implicit-zero
            // outer spacing — tab bar pinned to pane edges, terminal
            // flush against the scroll view + window edges. The 24pt
            // pill height inside the strip stays the same; only the
            // outer container paddings change.
            tabScroll.topAnchor.constraint(equalTo: pane.topAnchor),
            tabScroll.leadingAnchor.constraint(equalTo: pane.leadingAnchor),
            tabScroll.trailingAnchor.constraint(equalTo: pane.trailingAnchor),
            tabScroll.heightAnchor.constraint(equalToConstant: tabBarHeight),

            // Document view's height matches the scroll view's content
            // height; width grows with arranged subviews so the strip
            // scrolls horizontally when the pills exceed the available
            // pane width.
            tabBar.heightAnchor.constraint(equalTo: tabScroll.contentView.heightAnchor),
            tabBar.topAnchor.constraint(equalTo: tabScroll.contentView.topAnchor),
            tabBar.leadingAnchor.constraint(equalTo: tabScroll.contentView.leadingAnchor),
            // Trailing pin is intentionally NOT set — letting the stack
            // grow past the scroll view's right edge is what produces
            // horizontal scrolling.

            // Terminal container fills the content pane below the
            // tab bar. Width is unconstrained from above — when the
            // window resizes, the container grows and `TerminalView`
            // reflows its cell grid in `setFrameSize`. `terminalSize`
            // (the 80x24 cell-grid intrinsic) is preserved as the
            // floor so the terminal can't be squeezed below a
            // minimal usable shape. The min-width keeps a hard floor
            // at very narrow window widths; the user will still see a
            // small right-edge gap there until they widen the window.
            //
            // Re-anchored to `tabScroll.bottomAnchor` rather than the
            // raw `tabBar.bottomAnchor` so the intent — "terminal sits
            // flush against the strip" — is robust to changes in pill
            // height. They produce the same Y in current layout.
            terminalContainer.topAnchor.constraint(equalTo: tabScroll.bottomAnchor),
            terminalContainer.leadingAnchor.constraint(equalTo: pane.leadingAnchor),
            terminalContainer.trailingAnchor.constraint(equalTo: pane.trailingAnchor),
            terminalContainer.bottomAnchor.constraint(equalTo: pane.bottomAnchor),
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

    /// Round-2 fix: AppKit doesn't always restore first responder to
    /// the terminal view when the app regains focus (e.g. after
    /// switching apps, or after the rename popover closes). Without
    /// terminal-as-first-responder, ⌘V / ⌘C / ⌘X / ⌘A fall through to
    /// `NSText.paste(_:)` / `NSText.copy(_:)` etc. with no target
    /// and become no-ops. Snapping focus back here covers the
    /// common case without needing a click into the terminal area.
    func applicationDidBecomeActive(_ notification: Notification) {
        focusActiveTerminal()
    }

    /// Make the active project's terminal view the window's first
    /// responder. No-op if the workspace hasn't bootstrapped yet or
    /// the active project has no live tab. Safe to call repeatedly.
    @MainActor
    private func focusActiveTerminal() {
        guard let activeProjectID,
              let session = activeSessionByProject[activeProjectID]
        else { return }
        window?.makeFirstResponder(session.terminalView)
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
            // Round-3 R5: cancel mid-edit on each condemned pill so
            // its NSTextField stops being first responder before the
            // view is torn down.
            if let tabID = session.id, let pill = tabPillViews[tabID], pill.isEditing {
                pill.endEdit()
            }
            if let tabID = session.id {
                tabPillViews.removeValue(forKey: tabID)
            }
            session.terminalView.removeFromSuperview()
            session.close(socketPath: socketPath)
        }
        tabs.removeAll { $0.projectID == id }
        activeSessionByProject.removeValue(forKey: id)
        projects.removeAll { $0.id == id }
        // M5 of `goal-mac-parity-2026-05-18.md`: drop the cached cell
        // view for this project so the per-project map doesn't grow
        // unbounded. If the row was being edited at the moment of
        // delete the entry just vanishes — no orphan edit state.
        projectRowCellViews.removeValue(forKey: id)
    }

    /// Append `snap` to `projects` and rebuild the sidebar unless a row
    /// with the same id already exists. Insert-only — never replaces an
    /// existing row; use `.projectRenamed` for in-place updates. Both
    /// the optimistic `newProject` unary response and the `.projectCreated`
    /// WatchEvents handler funnel through here so they can't insert
    /// duplicate rows when they race (issue #57).
    @MainActor
    private func insertProjectLocallyIfMissing(_ snap: ProjectSnapshot) {
        guard !projects.contains(where: { $0.id == snap.id }) else { return }
        projects.append(snap)
        rebuildSidebar()
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
            insertProjectLocallyIfMissing(ProjectSnapshot(id: p.id, name: p.name, cwd: p.cwd))
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
                }
                // The empty-workspace close path is hoisted below so
                // it also runs for the `deleteProjectFromMenu` flow,
                // which clears `activeProjectID` itself before this
                // event arrives — in that path `wasActive` is already
                // false here even though the workspace just became
                // empty.
            }
            // M5: workspace is empty. Close the window unconditionally
            // — applicationShouldTerminateAfterLastWindowClosed is true
            // by default for NSWindow-based apps, so the window close
            // cascades to app termination. Matches Go
            // cmd/roost/app.go:2107-2115 ("len(a.projectViews) == 0 →
            // win.Close()"). Hoisted out of the `if wasActive` branch
            // so it fires after the menu-driven delete-last-project
            // path too (that path nils `activeProjectID` before this
            // event lands; `wasActive` is then false).
            if projects.isEmpty {
                updateWindowTitle()
                terminalContainer?.subviews.forEach { $0.removeFromSuperview() }
                window?.close()
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
            // Round-3 R5: cancel any in-progress inline rename on the
            // condemned pill before dropping the cached view. The
            // edit's first responder is the pill's NSTextField — if
            // we drop the view while it's first responder, AppKit
            // walks the chain looking for a successor and the focus
            // ends up at the window root instead of the terminal.
            if let id = session.id, let pill = tabPillViews[id], pill.isEditing {
                pill.endEdit()
            }
            tabs.removeAll { $0 === session }
            if let id = session.id {
                tabPillViews.removeValue(forKey: id)
            }
            if wasActive {
                activeSessionByProject.removeValue(forKey: projectID)
            }
            session.terminalView.removeFromSuperview()
            session.close(socketPath: socketPath)
            // M5: project-level cascade lives daemon-side now
            // (state.rs::close_tab cascades to delete_project when
            // the parent project is empty). The Mac UI's local `tabs`
            // list omits headless-CLI-opened tabs, so a UI-side empty
            // check could delete a project the daemon thinks still
            // has tabs — moved the policy to the authoritative side.
            // We just handle local cleanup here; the daemon's
            // ProjectDeletedEvent (when cascaded) lands in the
            // `.projectDeleted` arm below and closes the window if
            // the workspace is empty.
            if projectID == activeProjectID {
                rebuildTabBar()
                if wasActive {
                    let remaining = tabsForActiveProject()
                    if !remaining.isEmpty {
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
            // when the affected tab is in the active project. Refresh
            // the window subtitle when the change is on the *active*
            // tab so the headerbar tracks `cd` in real time, matching
            // `cmd/roost/app.go::updateHeader`.
            if let session = tabs.first(where: { $0.id == e.tabID }) {
                session.liveCwd = e.cwd
                if session.projectID == activeProjectID {
                    rebuildTabBar()
                    if let activeProjectID,
                       activeSessionByProject[activeProjectID] === session
                    {
                        updateWindowTitle()
                    }
                }
            }
        case .tabState(let e):
            // TabState updates light up the status-dot slot M3's
            // TabPillView reserved. Stash and rebuild. Also rebuild
            // the sidebar so M6's per-project rollup stripe picks up
            // the new state — `viewFor:item:` recomputes the rollup
            // from `tabs` on every reload.
            if let session = tabs.first(where: { $0.id == e.tabID }) {
                session.liveState = Int32(e.state.rawValue)
                if session.projectID == activeProjectID {
                    rebuildTabBar()
                }
                rebuildSidebar()
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
        case .hookActive(let e):
            // M6 of `goal-mac-parity-2026-05-18.md`: hookActive suppresses
            // the per-tab agent state from the sidebar rollup. The pill
            // dot still tracks the raw state — only the project-level
            // rollup demotes. Mirrors Linux `crates/roost-linux/src/rollup.rs`
            // semantics; the Go binary doesn't have this suppression at
            // all (deliberate extension past Go-parity).
            if let session = tabs.first(where: { $0.id == e.tabID }) {
                session.hookActive = e.active
                rebuildSidebar()
            }
        case .tabsReordered(let e):
            // M2 of `goal-mac-parity-2026-05-18.md`: apply daemon-
            // authoritative tab order so a Mac drag + a sibling
            // `roost-cli-rs tab reorder` both converge through the
            // same code path. Reverses Mac's pre-M2 stance of
            // dropping reorder events.
            applyTabsReorder(projectID: e.projectID, newOrder: e.tabIds)
        case .projectsReordered(let e):
            // M3 of `goal-mac-parity-2026-05-18.md`: apply daemon-
            // authoritative project order. Active project stays
            // selected through the rebuild.
            applyProjectsReorder(newOrder: e.projectIds)
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
                // MUST precede selectProject: selectProject ->
                // openNewTab doesn't check list membership and will
                // RPC OpenTab for a ghost id if the row isn't here yet.
                self.insertProjectLocallyIfMissing(created)
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
        beginRenameProject(id: project.id)
    }

    /// M5 of `goal-mac-parity-2026-05-18.md`: flip the project's
    /// sidebar row into inline-edit mode. Enter commits via
    /// `RenameProject` RPC; Escape (or click-away) cancels and
    /// restores the displayed name from the model. Mid-edit
    /// `ProjectRenamedEvent` arrivals for this project leave the
    /// typing buffer alone (race guard lives on
    /// `ProjectRowCellView.isEditing`).
    @MainActor
    private func beginRenameProject(id: Int64) {
        guard let outline = projectsOutlineView,
              let idx = projects.firstIndex(where: { $0.id == id })
        else { return }
        ensureSidebarVisible()
        // Trigger a viewFor:item: call for this row so the cached
        // cell view is in the hierarchy. `makeIfNecessary: true`
        // is what asks AppKit to materialize a row that may be
        // offscreen / not realized yet.
        guard let cell = outline.view(
            atColumn: 0,
            row: idx,
            makeIfNecessary: true
        ) as? ProjectRowCellView else { return }

        let initial = projects[idx].name
        cell.beginEdit(
            initial: initial,
            onCommit: { [weak self] newName in
                self?.commitRenameProject(id: id, newName: newName)
            },
            onCancel: { [weak self] in
                self?.cancelRenameProject(id: id)
            }
        )
    }

    @MainActor
    private func commitRenameProject(id: Int64, newName: String) {
        let trimmed = newName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let idx = projects.firstIndex(where: { $0.id == id }) else { return }
        // Empty / unchanged name is treated as cancel — the existing
        // displayed label re-renders on the next `configure()`.
        let current = projects[idx].name
        guard !trimmed.isEmpty, trimmed != current else {
            rebuildSidebar()
            // Round-2 polish: focus returns to the terminal so the
            // user can resume typing immediately after Escape /
            // unchanged-Enter, matching the popover behavior they
            // already liked for tab rename.
            focusActiveTerminal()
            return
        }

        // Optimistic local update — mirrors the pre-M5 NSAlert flow.
        // `.projectRenamed` event arrives next and reconciles if
        // anything drifts.
        projects[idx] = ProjectSnapshot(
            id: id,
            name: trimmed,
            cwd: projects[idx].cwd
        )
        rebuildSidebar()
        // Round-2 polish: focus back to the terminal so the user can
        // type immediately after committing the rename.
        focusActiveTerminal()

        let socket = socketPath
        Task {
            await renameProject(socketPath: socket, projectID: id, name: trimmed)
        }
    }

    @MainActor
    private func cancelRenameProject(id: Int64) {
        // Nothing model-wise to do — the next configure() call resyncs
        // textField.stringValue from `projects[id].name`.
        rebuildSidebar()
        // Round-2 polish: focus back to the terminal on Escape too.
        focusActiveTerminal()
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
        deleteProjectByID(id: id)
    }

    /// Round-4 R3: ⌘⇧W close-project handler. Targets the *active*
    /// project (which is what the user sees in the terminal area).
    /// Confirms when the project has 2+ tabs — single-tab projects
    /// are typically a freshly-created empty workspace and the
    /// confirmation would be friction.
    ///
    /// Round-5 (CR on #73): query the *daemon* for the
    /// authoritative tab count instead of trusting
    /// `tabsForActiveProject()`. The Mac UI deliberately doesn't
    /// attach to sibling-client tabs (App.swift:781 area), so its
    /// local count can under-report and skip the confirmation when
    /// the project actually has 2+ daemon-side tabs. On daemon
    /// failure (unreachable, etc.) fall back to the local count —
    /// at worst we show a needless confirmation.
    @objc @MainActor
    private func closeActiveProject(_ sender: Any?) {
        guard let activeProjectID,
              let project = projects.first(where: { $0.id == activeProjectID })
        else { return }
        let socket = socketPath
        let projectName = project.name
        Task { @MainActor [weak self] in
            guard let self else { return }
            // Round-5 (CR on #74): when the daemon RPC fails we
            // can't trust the local count — sibling-attached tabs
            // are invisible to the Mac UI. A nil response means
            // "unknown"; conservatively confirm rather than silently
            // delete a project that might have many daemon-side
            // tabs.
            let daemonCount = await daemonTabCount(
                socketPath: socket,
                projectID: activeProjectID
            )
            let shouldConfirm = daemonCount.map { $0 > 1 } ?? true
            if shouldConfirm {
                let alert = NSAlert()
                alert.messageText = "Close \(projectName)?"
                if let daemonCount {
                    alert.informativeText =
                        "This will close \(daemonCount) tabs in this project. The action can't be undone."
                } else {
                    alert.informativeText =
                        "The daemon's tab count is unavailable. Closing may close multiple tabs in this project. The action can't be undone."
                }
                alert.addButton(withTitle: "Close Project")
                alert.addButton(withTitle: "Cancel")
                alert.alertStyle = .warning
                guard alert.runModal() == .alertFirstButtonReturn else { return }
            }
            self.deleteProjectByID(id: activeProjectID)
        }
    }

    /// Shared delete-after-confirmation path used by both
    /// `deleteProjectFromMenu` (sidebar right-click) and round-4 R3's
    /// `closeActiveProject` (⌘⇧W). Closes every UI-side TabSession in
    /// the project so the StreamPty streams shut down before the
    /// daemon cascade-deletes their rows, then fires the daemon RPC
    /// and resyncs the sidebar.
    ///
    /// Round-3 R5 invariants preserved: cancel any mid-edit pill so
    /// its NSTextField stops being first responder before the view
    /// goes away, and drop the cache entry so a future tab id reuse
    /// can't resurrect stale pill state.
    @MainActor
    private func deleteProjectByID(id: Int64) {
        let condemned = tabs.filter { $0.projectID == id }
        for session in condemned {
            if let tabID = session.id, let pill = tabPillViews[tabID], pill.isEditing {
                pill.endEdit()
            }
            if let tabID = session.id {
                tabPillViews.removeValue(forKey: tabID)
            }
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
        // Round-2 polish: pre-seed `liveCwd` from the project's cwd
        // so the new pill renders the tilde-abbreviated path on
        // frame 1, instead of flashing "Tab N" while waiting for the
        // shell's OSC 7. OSC 7 will refine if the shell starts in a
        // different directory. Matches the Go binary's behavior.
        if let project = projects.first(where: { $0.id == projectID }),
           !project.cwd.isEmpty
        {
            session.liveCwd = project.cwd
        }
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

        // Round-3 R5: cancel any in-progress inline rename so its
        // first-responder NSTextField doesn't get orphaned by the
        // view teardown below.
        if let id = session.id, let pill = tabPillViews[id], pill.isEditing {
            pill.endEdit()
        }
        tabs.removeAll { $0 === session }
        if let id = session.id {
            tabPillViews.removeValue(forKey: id)
        }
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
        // Round-3 R2: keep the active pill visible when the strip
        // overflows horizontally. Only the tab-selection path scrolls
        // — `rebuildTabBar` is invoked for many unrelated state arms
        // (notifications, cwd, title, reorder), and scrolling on each
        // would jump the strip on sibling state changes.
        scrollActiveTabIntoView()
        // The subtitle tracks the active tab's live cwd, so switching
        // tabs within a project has to refresh it. rebuildSidebar (the
        // other call site that triggers updateWindowTitle) only fires
        // when projects mutate, not tab focus.
        updateWindowTitle()
    }

    /// Pure helper extracted so unit tests can hit the pill-index math
    /// without an AppKit dependency. Returns the index of `active`
    /// inside `tabs` (the active project's tab list in display order),
    /// or `nil` if not present.
    @MainActor
    static func activeTabIndex(tabs: [TabSession], active: TabSession?) -> Int? {
        guard let active else { return nil }
        return tabs.firstIndex(where: { $0 === active })
    }

    /// Scroll the active tab pill into the visible region of the
    /// horizontally-scrollable strip. Called from `selectTab(at:)`
    /// only — never from `rebuildTabBar`, which fires for many
    /// state-change arms that shouldn't jolt the scroll position.
    /// `layoutSubtreeIfNeeded()` settles pill frames synchronously
    /// (otherwise the freshly-rebuilt strip's geometry is still
    /// pending and `scrollToVisible` lands on the wrong rect).
    @MainActor
    private func scrollActiveTabIntoView() {
        guard let tabBar,
              let activeProjectID,
              let active = activeSessionByProject[activeProjectID]
        else { return }
        let projectTabs = tabsForActiveProject()
        guard let activeIdx = Self.activeTabIndex(tabs: projectTabs, active: active)
        else { return }
        let pills = tabBar.arrangedSubviews.compactMap { $0 as? TabPillView }
        guard pills.indices.contains(activeIdx) else { return }
        tabBar.layoutSubtreeIfNeeded()
        pills[activeIdx].scrollToVisible(pills[activeIdx].bounds)
    }

    @MainActor
    private func rebuildTabBar() {
        guard let tabBar = tabBar, let addTabButton = addTabButton else { return }

        let projectTabs = tabsForActiveProject()
        let activeSession = activeProjectID.flatMap { activeSessionByProject[$0] }

        // Round-3 R5: build the desired pill list, reusing cached
        // pill views per daemon tab id. Pills without a daemon id
        // (mid-OpenTab) are created fresh each rebuild — they can't
        // participate in inline rename anyway. Reuse via `configure(...)`
        // lets an in-progress rename survive a sibling-driven rebuild.
        var desired: [TabPillView] = []
        desired.reserveCapacity(projectTabs.count)
        for (index, session) in projectTabs.enumerated() {
            let isActive = session === activeSession
            // Phase 6a P6 label: prefer title (OSC 0/1/2) if set,
            // else cwd (OSC 7) tilde-abbreviated, else "Tab N".
            // This is the visible payoff for P4 + P5 + P6 — the
            // pill stops saying "Tab N" once the shell emits OSCs.
            let pillTitle = pillLabel(for: session, index: index)
            let statusColor = pillStatusColor(for: session)
            let hasBadge = session.liveHasNotification
            let select: @MainActor (Int) -> Void = { [weak self] idx in
                self?.selectTab(at: idx)
            }
            let close: @MainActor (Int) -> Void = { [weak self] idx in
                self?.closeTab(at: idx)
            }
            let rename: @MainActor (Int) -> Void = { [weak self] idx in
                self?.renameTab(at: idx)
            }

            let pill: TabPillView
            if let id = session.id, let cached = tabPillViews[id] {
                cached.configure(
                    index: index,
                    title: pillTitle,
                    isActive: isActive,
                    statusColor: statusColor,
                    hasBadge: hasBadge,
                    tabID: id,
                    onSelect: select,
                    onClose: close,
                    onRename: rename
                )
                pill = cached
            } else {
                pill = TabPillView(
                    index: index,
                    title: pillTitle,
                    isActive: isActive,
                    statusColor: statusColor,
                    hasBadge: hasBadge,
                    tabID: session.id,
                    minWidth: config.tabMinWidth ?? 80,
                    maxWidth: config.tabMaxWidth ?? 220,
                    onSelect: select,
                    onClose: close,
                    onRename: rename
                )
                if let id = session.id {
                    tabPillViews[id] = pill
                }
            }
            desired.append(pill)
        }

        // Purge cached pills whose tab no longer exists in the active
        // project. The map is bounded by daemon tab ids so this stays
        // tiny; the only growth path is `OpenTab` adding entries.
        let activeIDs: Set<Int64> = Set(projectTabs.compactMap { $0.id })
        for staleID in tabPillViews.keys where !activeIDs.contains(staleID) {
            tabPillViews.removeValue(forKey: staleID)
        }

        // Rebuild the arranged-subview order. Pulling reused pills
        // off the stack via `removeArrangedSubview` keeps them alive
        // (no `removeFromSuperview`) so subsequent insert places the
        // same instance in its new slot. Fresh pills come in via
        // `insertArrangedSubview` for the first time.
        let desiredSet = Set(desired.map { ObjectIdentifier($0) })
        for view in tabBar.arrangedSubviews
        where view !== addTabButton
            && !desiredSet.contains(ObjectIdentifier(view))
        {
            tabBar.removeArrangedSubview(view)
            view.removeFromSuperview()
        }
        // Move existing reused pills out of the arranged set (they'll
        // be re-inserted at the correct position below). This keeps
        // the views alive — `removeArrangedSubview` does not call
        // `removeFromSuperview` for unanchored arranged children.
        for pill in desired where tabBar.arrangedSubviews.contains(pill) {
            tabBar.removeArrangedSubview(pill)
        }
        for (slot, pill) in desired.enumerated() {
            tabBar.insertArrangedSubview(pill, at: slot)
        }

        rebuildWindowMenu()
    }

    /// M2 of `goal-mac-parity-2026-05-18.md`: handle a tab pill drop
    /// inside the active project's strip. The pill identifies itself
    /// by daemon-assigned id (pasteboard `roostTabID`); the strip's
    /// hit-test produces a raw target index. We resolve both back to
    /// our local model, compute the final insert index via the
    /// shared `ReorderMath`, and fire `ReorderTabs` to the daemon.
    /// The local order does *not* change here — we wait for the
    /// `.tabsReordered` event to apply, matching the cross-cutting
    /// "WatchEvents-only mutation" invariant.
    @MainActor
    private func handleTabDrop(sourceTabID: Int64, rawTargetIdx: Int) {
        guard let activeProjectID,
              daemonReachable
        else { return }
        let projectTabs = tabsForActiveProject()
        // CodeRabbit on PR #68: the reorder math and the daemon-bound
        // id sequence must live in the same index space. `projectTabs`
        // includes tabs with `id == nil` (mid-OpenTab); the daemon-
        // bound `ids` excludes them. Computing `mapped.index` in the
        // visual space and applying it to `ids` skews by one for every
        // nil-id tab before the drop target. Translate `rawTargetIdx`
        // into the persisted-id space before calling `computeInsertIdx`.
        let persisted: [(visualIdx: Int, id: Int64)] = projectTabs.enumerated().compactMap { idx, session in
            guard let id = session.id else { return nil }
            return (visualIdx: idx, id: id)
        }
        guard let sourcePersistedIdx = persisted.firstIndex(where: { $0.id == sourceTabID })
        else { return }
        let rawTargetPersistedIdx = persisted.reduce(into: 0) { count, entry in
            if entry.visualIdx < rawTargetIdx { count += 1 }
        }
        let mapped = computeInsertIdx(
            sourceIdx: sourcePersistedIdx,
            rawTargetIdx: rawTargetPersistedIdx
        )
        if mapped.isNoop { return }

        // Build the new id sequence: remove the source, insert at the
        // mapped index. Tabs without daemon ids skip out — they're
        // mid-OpenTab and can't be reordered yet, but they keep
        // their relative position in the array (the daemon-side
        // reorder only operates on persisted ids).
        var ids: [Int64] = persisted.map { $0.id }
        let source = ids.remove(at: sourcePersistedIdx)
        let clamped = min(max(mapped.index, 0), ids.count)
        ids.insert(source, at: clamped)

        let socket = socketPath
        let projectID = activeProjectID
        Task {
            await reorderTabs(socketPath: socket, projectID: projectID, tabIDs: ids)
        }
    }

    /// Apply a daemon-authoritative tab order to the local model. Called
    /// from the `.tabsReordered` event arm — every reorder, whether
    /// driven by a Mac UI drag or a sibling `roost-cli-rs tab reorder`,
    /// flows through this single point. Out-of-order ids (not in
    /// `newOrder`) keep their relative position at the tail.
    @MainActor
    private func applyTabsReorder(projectID: Int64, newOrder: [Int64]) {
        let positions = tabs.indices.filter { tabs[$0].projectID == projectID }
        if positions.isEmpty { return }
        let projectTabs = positions.map { tabs[$0] }
        let byID: [Int64: TabSession] = Dictionary(
            uniqueKeysWithValues: projectTabs.compactMap { session in
                session.id.map { ($0, session) }
            }
        )
        let listed = newOrder.compactMap { byID[$0] }
        let unlisted = projectTabs.filter { session in
            guard let id = session.id else { return true }
            return !newOrder.contains(id)
        }
        let finalOrder = listed + unlisted
        for (i, pos) in positions.enumerated() where i < finalOrder.count {
            tabs[pos] = finalOrder[i]
        }
        if projectID == activeProjectID {
            rebuildTabBar()
        }
    }

    /// Apply a daemon-authoritative project order. Mac's longstanding
    /// stance (App.swift:680) keeps active state local-authoritative;
    /// here we reverse it for projects-reordered specifically because
    /// drag-reorder is a cross-client signal a CLI mutation might
    /// arrive without a corresponding outbound RPC. We snapshot the
    /// active project id so the row stays selected through the shuffle
    /// (mirrors Linux `apply_sidebar_order` invariant from M10).
    @MainActor
    private func applyProjectsReorder(newOrder: [Int64]) {
        let byID = Dictionary(uniqueKeysWithValues: projects.map { ($0.id, $0) })
        let listed = newOrder.compactMap { byID[$0] }
        let unlisted = projects.filter { p in !newOrder.contains(p.id) }
        let combined = listed + unlisted
        guard combined.map({ $0.id }) != projects.map({ $0.id }) else { return }
        projects = combined
        rebuildSidebar()
    }

    /// Rename the tab at the given index in the active project.
    /// Wired from `TabPillView`'s right-click "Rename…" menu (M4 of
    /// `goal-mac-parity-2026-05-18.md`).
    ///
    /// Round-2 fix: open the popover directly on the right-clicked
    /// pill rather than routing through `selectTab → rebuildTabBar
    /// → renameActiveTab`. The previous chain rebuilt the strip
    /// mid-action, leaving the popover trying to anchor against a
    /// pill whose window-side layout hadn't settled, and the menu
    /// item appeared to do nothing.
    @MainActor
    private func renameTab(at indexInActiveProject: Int) {
        let projectTabs = tabsForActiveProject()
        guard projectTabs.indices.contains(indexInActiveProject) else { return }
        let session = projectTabs[indexInActiveProject]
        guard let tabID = session.id else { return }
        let currentTitle = pillLabel(for: session, index: indexInActiveProject)
        beginRenameActiveTab(
            tabID: tabID,
            currentTitle: currentTitle,
            atIndex: indexInActiveProject
        )
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

        // Round-3 R5: cancel any mid-edit so the pill's NSTextField
        // stops being first responder before we tear the view down.
        if let id = session.id, let pill = tabPillViews[id], pill.isEditing {
            pill.endEdit()
        }
        tabs.removeAll { $0 === session }
        if let id = session.id {
            tabPillViews.removeValue(forKey: id)
        }
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
        // M5 of `goal-mac-parity-2026-05-18.md`: route through the
        // shared inline-rename flow. The pre-M5 implementation
        // wrapped `renameProjectFromMenu` with a placeholder
        // NSMenuItem so it could reuse the modal alert — that path
        // is gone now (NSAlert replaced with inline edit), so call
        // beginRenameProject directly.
        beginRenameProject(id: id)
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
        let closeProjectItem = NSMenuItem(
            title: "Close Project",
            action: #selector(closeActiveProject(_:)),
            keyEquivalent: ""
        )
        closeProjectItem.target = self
        bind(closeProjectItem, to: KeybindAction.closeProject)
        fileMenu.addItem(closeProjectItem)
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
    /// title becomes the project name; the subtitle becomes the live
    /// cwd of the active tab (falling back to the project's static cwd
    /// before any tab is open or OSC 7 has fired). Matches the
    /// libadwaita `AdwWindowTitle` pattern the Go binary uses for the
    /// same window — see `cmd/roost/app.go::updateHeader` for the
    /// reference, which reads `sess.lastPWD` populated by OSC 7.
    @MainActor
    private func updateWindowTitle() {
        guard let window else { return }
        guard let activeProjectID,
              let project = projects.first(where: { $0.id == activeProjectID })
        else {
            window.title = "Roost"
            window.subtitle = ""
            return
        }
        window.title = project.name.isEmpty ? "Roost" : project.name

        let activeSession = activeSessionByProject[activeProjectID]
        let liveCwd = activeSession?.liveCwd ?? ""
        let cwd = liveCwd.isEmpty ? project.cwd : liveCwd
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        window.subtitle = cwd.isEmpty ? "" : pathDisplay(cwd, home: home, max: 48)
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

    /// Tilde-abbreviate `$HOME` prefixes in a path. Thin wrapper around
    /// the testable `pathDisplay` free function — `Int.max` disables the
    /// rune-budget so pill labels rely on AppKit's tail-truncation for
    /// width fitting (the window subtitle bounds the budget separately
    /// via `updateWindowTitle`).
    private func tildeAbbreviate(_ path: String) -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return pathDisplay(path, home: home, max: Int.max)
    }

    /// Resolve the status-dot color for a tab pill based on
    /// `liveState`. The proto's `TabState` enum is: 0=Unspecified,
    /// 1=None, 2=Running, 3=NeedsInput, 4=Idle (matches
    /// `proto/roost.proto`'s `TabState`). The dot picks the same
    /// palette as the M6 sidebar rollup so the two indicators agree:
    /// running → blue, needs-input → orange, idle → gray. None /
    /// unknown → no dot (M3's empty slot).
    @MainActor
    private func pillStatusColor(for session: TabSession) -> NSColor? {
        guard let state = session.liveState else { return nil }
        return RollupState(matchingProto: Int(state)).nsColor
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
        // Round-4 R2: clamp at endpoints instead of wrapping. ⌘⇧[ on
        // the first tab is a no-op; ⌘⇧] on the last tab is a no-op.
        let next = max(0, min(n - 1, currentIdx + delta))
        guard next != currentIdx else { return }
        selectTab(at: next)
    }

    /// Rename the active tab. M5 of `goal-mac-parity-2026-05-18.md`
    /// replaced the pre-existing NSAlert with an NSPopover anchored
    /// to the active pill — same UX as the Go binary's tab rename
    /// (`cmd/roost/app.go::renameActiveTab`) and Linux M9. Mirrors
    /// the popover-over-the-strip pattern documented at Linux
    /// `crates/roost-linux/src/app.rs:1057-1119`.
    /// On commit the daemon sets the per-tab `user_titled` lock so
    /// shell-emitted OSC 1/2 stops overwriting. ⌘R by default.
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
        beginRenameActiveTab(tabID: tabID, currentTitle: currentTitle, atIndex: index)
    }

    /// Round-3 R5: flip the cached pill into inline edit mode. The
    /// popover-based round-2 implementation was replaced with this
    /// inline flow for symmetry with the sidebar's label↔entry
    /// rename. Edit lifetime is bounded by the pill's own `isEditing`
    /// flag; the commit/cancel callbacks pop us back into normal
    /// mode and return focus to the terminal.
    @MainActor
    private func beginRenameActiveTab(tabID: Int64, currentTitle: String, atIndex: Int) {
        guard let pill = tabPillViews[tabID] else { return }
        pill.beginEdit(
            initial: currentTitle,
            onCommit: { [weak self] newTitle, initialTitle in
                self?.commitRenameTab(
                    tabID: tabID,
                    newTitle: newTitle,
                    initialTitle: initialTitle
                )
            },
            onCancel: { [weak self] in
                self?.cancelRenameTab()
            }
        )
    }

    @MainActor
    private func commitRenameTab(tabID: Int64, newTitle: String, initialTitle: String) {
        defer { focusActiveTerminal() }
        let trimmed = newTitle.trimmingCharacters(in: .whitespacesAndNewlines)
        guard tabs.contains(where: { $0.id == tabID }) else { return }
        // CodeRabbit on PR #69: compare against the popover's *initial*
        // text, not `session.liveTitle`. The popover prefills from
        // `pillLabel(for:index:)`, which falls back to the
        // tilde-abbreviated cwd or "Tab N" when liveTitle is empty;
        // pressing Enter without edits in that state would otherwise
        // commit + set the daemon's `user_titled` lock against a
        // value the user never typed. Empty + unchanged are cancel.
        guard !trimmed.isEmpty, trimmed != initialTitle else {
            // No-op commit — rebuild to resync the pill's label
            // (`isEditing` is already false from `endEdit`).
            if let session = tabs.first(where: { $0.id == tabID }),
               session.projectID == activeProjectID
            {
                rebuildTabBar()
            }
            return
        }
        guard let session = tabs.first(where: { $0.id == tabID }) else { return }

        // Optimistic local update so the pill flips immediately —
        // matches the pre-M5 NSAlert flow.
        session.liveTitle = trimmed
        if session.projectID == activeProjectID {
            rebuildTabBar()
        }

        let socket = socketPath
        Task {
            await setTabTitle(socketPath: socket, tabID: tabID, title: trimmed)
        }
    }

    @MainActor
    private func cancelRenameTab() {
        // Round-3 R5: the pill already returned to non-edit mode via
        // `endEdit`. Re-render so the label re-syncs from the model
        // (the user may have typed something we never committed),
        // then return focus to the terminal — sidebar rename has the
        // same explicit `focusActiveTerminal()` call on cancel.
        rebuildTabBar()
        focusActiveTerminal()
    }

    // MARK: - Sidebar toggle (M3)

    /// `toggle_sidebar` action handler. Round-7 R7.A: collapse via
    /// `setPosition(_:ofDividerAt:)` plus deactivating the `>= min`
    /// width constraint, rather than `isHidden = true` (which left
    /// the pane occupying its constrained width and the divider
    /// visible). The interactive-drag bounds are unchanged — they're
    /// only consulted while `sidebarCollapsingProgrammatically` is
    /// false. Bound to ⌘B by default in `Keybind.swift`, overrideable
    /// via the config file's `keybind = … = toggle_sidebar` line.
    @objc @MainActor
    private func toggleSidebar(_ sender: Any?) {
        guard let sidebarPane,
              let split = sidebarPane.superview as? NSSplitView
        else { return }
        let currentWidth = sidebarPane.frame.width
        let isCurrentlyCollapsed = currentWidth < 1
        if isCurrentlyCollapsed {
            // Restore.
            let restore = sidebarRestoreWidth > 0 ? sidebarRestoreWidth : 220
            sidebarMinWidthConstraint?.isActive = true
            sidebarMaxWidthConstraint?.isActive = true
            sidebarPreferredWidthConstraint?.constant = restore
            split.setPosition(restore, ofDividerAt: 0)
            UserDefaults.standard.set(true, forKey: Self.sidebarVisibleDefaultsKey)
        } else {
            // Collapse.
            sidebarRestoreWidth = currentWidth
            sidebarPreferredWidthConstraint?.constant = 0
            sidebarMinWidthConstraint?.isActive = false
            sidebarMaxWidthConstraint?.isActive = false
            // Bypass `constrainMinCoordinate`'s 160pt floor for this
            // one programmatic setPosition call. The flag is reset
            // synchronously after the call returns.
            sidebarCollapsingProgrammatically = true
            split.setPosition(0, ofDividerAt: 0)
            sidebarCollapsingProgrammatically = false
            UserDefaults.standard.set(false, forKey: Self.sidebarVisibleDefaultsKey)
        }
    }

    /// Force the sidebar visible without toggling. Called from the
    /// three user actions where Go (`cmd/roost/app.go:1337,1487,1975`)
    /// auto-expands the sidebar so the user sees the affected row:
    /// `newProject` (sidebar shows the freshly-created project),
    /// `selectProject` (the ⌘1-9 switcher reveals the focused project),
    /// and `beginRenameActiveProject` (M4 hookup — rename popover
    /// needs the row visible to anchor against).
    ///
    /// Round-7 R7.A: collapse is now a width-0 state, not
    /// `isHidden = true`. Delegate to `toggleSidebar` when the pane
    /// is currently collapsed so we reuse the restore path
    /// (constraint reactivation + `setPosition`).
    @MainActor
    private func ensureSidebarVisible() {
        guard let sidebarPane else { return }
        guard sidebarPane.frame.width < 1 else { return }
        toggleSidebar(nil)
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

    /// Resolve the same default socket path as `roost-common`'s Mac
    /// bundle profile — always `~/Library/Caches/Roost/roost.sock`
    /// when `HOME` is set; `/tmp/Roost/roost.sock` only as a last
    /// resort. Capital `Roost` since M1 of the daemon-removal
    /// refactor; pre-M1 stale state under lowercase `roost/` is
    /// intentionally not migrated.
    nonisolated static func defaultSocketPath(
        environment env: [String: String] = ProcessInfo.processInfo.environment
    ) -> String {
        BundleProfile.mac(environment: env).socketPath
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
final class TabPillView: NSView, NSTextFieldDelegate {
    /// Round-3 R5: position-and-callback state is mutable so a single
    /// pill instance can be reused across `rebuildTabBar` cycles
    /// (caching pills per tabID lets in-progress inline rename survive
    /// sibling-driven strip rebuilds, mirroring the sidebar's
    /// `projectRowCellViews` pattern).
    fileprivate var index: Int
    private var isActive: Bool
    /// Daemon-assigned tab id, or `nil` between OpenTab and the
    /// daemon's reply. Used by M2's drag source: pasteboard payload
    /// is the tab id so the destination can resolve the source row
    /// independent of any intervening order changes. Pre-id pills
    /// drop their drag silently.
    /// Round-6 R6.C: `internal` (was `fileprivate`) so
    /// `TabBarStackView` in `DragReorder.swift` can resolve the
    /// source pill from a pasteboard payload when setting up the
    /// drop placeholder.
    internal var tabID: Int64?
    private var onSelect: @MainActor (Int) -> Void
    private var onClose: @MainActor (Int) -> Void
    private var onRename: @MainActor (Int) -> Void

    private let label: NSTextField
    private let closeButton: NSButton
    private let statusSlot: NSView
    private let badgeDot: NSView

    /// Round-3 R5: edit-mode swap. In normal mode the label's trailing
    /// anchor leaves room for closeButton / badgeDot; while editing,
    /// it pins to the pill's trailing edge so the bezeled field uses
    /// the full pill width. `labelTrailingEdit` is `lazy` because the
    /// pill's trailingAnchor isn't valid until after `super.init`.
    private var labelTrailingNormal: NSLayoutConstraint!
    private lazy var labelTrailingEdit: NSLayoutConstraint = label.trailingAnchor
        .constraint(equalTo: trailingAnchor, constant: -8)

    /// Round-3 R5: force the pill to a usable minimum width while
    /// editing. The label's intrinsic content size shrinks to its
    /// (possibly short) initial title in bezeled mode; without this
    /// the field shows as a sliver too narrow to read or type into.
    /// 220pt is wide enough for a typical tab title and matches the
    /// rename popover's previous content width.
    private lazy var pillEditMinWidth: NSLayoutConstraint = widthAnchor
        .constraint(greaterThanOrEqualToConstant: 220)

    /// Round-3 R5: inline rename state. `isEditing == true` blocks
    /// `configure(...)` from overwriting `label.stringValue` (race
    /// guard against sibling-driven rebuilds) and short-circuits the
    /// mouseDown/Dragged/Up drag plumbing so the editable field
    /// handles clicks normally.
    private(set) var isEditing = false
    private var onCommit: (@MainActor (String) -> Void)?
    private var onCancel: (@MainActor () -> Void)?

    /// Test-only accessor for the label's displayed string. Used by
    /// the inline-rename race-guard tests to peek at the typing
    /// buffer without an NSWindow.
    internal var editBufferTextForTesting: String { label.stringValue }

    /// M2 drag-source state. `mouseDown` records the event so
    /// `mouseDragged` can use it as the dragging session's seed event;
    /// `isDragging` debounces (re-entrant drag is a no-op).
    private var mouseDownEvent: NSEvent?
    private var isDragging = false

    init(
        index: Int,
        title: String,
        isActive: Bool,
        statusColor: NSColor? = nil,
        hasBadge: Bool = false,
        tabID: Int64? = nil,
        // Round-4 R4: per-pill width bounds for the strip's
        // Ghostty-style shrink-and-truncate behavior. Both default
        // to nil, meaning "use the compiled-in defaults" (80 / 220).
        // A `0` value at the config layer disables that bound;
        // by the time it reaches here, callers should have already
        // resolved 0 → nil so unbound axes simply pass nothing.
        minWidth: CGFloat? = 80,
        maxWidth: CGFloat? = 220,
        onSelect: @escaping @MainActor (Int) -> Void,
        onClose: @escaping @MainActor (Int) -> Void,
        onRename: @escaping @MainActor (Int) -> Void
    ) {
        self.index = index
        self.isActive = isActive
        self.tabID = tabID
        self.onSelect = onSelect
        self.onClose = onClose
        self.onRename = onRename

        let label = NSTextField(labelWithString: title)
        label.font = .systemFont(
            ofSize: 12,
            weight: isActive ? .medium : .regular
        )
        label.textColor = isActive ? .labelColor : .secondaryLabelColor
        label.lineBreakMode = .byTruncatingTail
        label.maximumNumberOfLines = 1
        label.usesSingleLineMode = true
        label.translatesAutoresizingMaskIntoConstraints = false
        self.label = label

        let closeButton = NSButton(title: "×", target: nil, action: nil)
        closeButton.isBordered = false
        closeButton.font = .systemFont(ofSize: 13, weight: .regular)
        closeButton.contentTintColor = .secondaryLabelColor
        closeButton.isHidden = !isActive
        closeButton.translatesAutoresizingMaskIntoConstraints = false
        self.closeButton = closeButton

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
        self.statusSlot = statusSlot

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
        self.badgeDot = badgeDot

        super.init(frame: .zero)

        // Trailing constraint for normal mode — built after super.init
        // because it could conceivably need self-relative anchors in
        // the future. `labelTrailingEdit` is constructed lazily on
        // first access (see property declaration).
        self.labelTrailingNormal = label.trailingAnchor.constraint(
            lessThanOrEqualTo: closeButton.leadingAnchor,
            constant: -6
        )

        wantsLayer = true
        layer?.cornerRadius = 6
        layer?.backgroundColor = (isActive
            ? NSColor.controlAccentColor.withAlphaComponent(0.18)
            : NSColor.clear).cgColor

        addSubview(statusSlot)
        addSubview(label)
        addSubview(closeButton)
        addSubview(badgeDot)

        // `closeButton.target/action` need self to be a captured
        // weak reference so the pill doesn't leak through the
        // closure → AppKit retain cycle.
        closeButton.target = self
        closeButton.action = #selector(closeClicked)

        // Round-4 R4: pill width bounds (Ghostty-style shrink-and-
        // truncate). Activated only when the corresponding bound is
        // non-nil; nil/`0` means "let the pill grow with its content"
        // (pre-round-4 behavior). The edit-mode 220pt min from round
        // 3 (`pillEditMinWidth`) coexists fine: greaterThanOrEqual.
        var widthConstraints: [NSLayoutConstraint] = []
        if let minW = minWidth, minW > 0 {
            widthConstraints.append(
                widthAnchor.constraint(greaterThanOrEqualToConstant: minW)
            )
        }
        if let maxW = maxWidth, maxW > 0 {
            widthConstraints.append(
                widthAnchor.constraint(lessThanOrEqualToConstant: maxW)
            )
        }

        NSLayoutConstraint.activate(widthConstraints + [
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

            // Round-3 R5: label's trailing constraint is one of two
            // swappable constraints — normal mode leaves room for
            // closeButton + badgeDot, edit mode pins to the pill's
            // trailing edge so the bezeled field fills the width.
            labelTrailingNormal,
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    /// Round-3 R5: update the mutable state of a cached pill to
    /// reflect new daemon-side state. Called from `rebuildTabBar`
    /// when the strip is re-rendered without destroying the
    /// underlying pill view (so an in-progress inline rename can
    /// keep its typing buffer through unrelated rebuilds).
    func configure(
        index: Int,
        title: String,
        isActive: Bool,
        statusColor: NSColor?,
        hasBadge: Bool,
        tabID: Int64?,
        onSelect: @escaping @MainActor (Int) -> Void,
        onClose: @escaping @MainActor (Int) -> Void,
        onRename: @escaping @MainActor (Int) -> Void
    ) {
        self.index = index
        self.isActive = isActive
        self.tabID = tabID
        self.onSelect = onSelect
        self.onClose = onClose
        self.onRename = onRename

        // Race guard: while editing, the label IS the typing buffer.
        // Sibling-driven `setTabTitle` events that race the user's
        // typing must NOT clobber it. The mid-edit rebuild happens
        // legitimately for unrelated reasons (notifications, cwd,
        // sibling state) — the model carries the new title, but
        // the displayed `stringValue` stays as the user types.
        if !isEditing {
            label.stringValue = title
            label.font = .systemFont(
                ofSize: 12,
                weight: isActive ? .medium : .regular
            )
            label.textColor = isActive ? .labelColor : .secondaryLabelColor
        }

        layer?.backgroundColor = (isActive
            ? NSColor.controlAccentColor.withAlphaComponent(0.18)
            : NSColor.clear).cgColor

        if let color = statusColor {
            statusSlot.layer?.backgroundColor = color.cgColor
            statusSlot.layer?.cornerRadius = 5
        } else {
            statusSlot.layer?.backgroundColor = NSColor.clear.cgColor
        }

        if !isEditing {
            closeButton.isHidden = !isActive
            badgeDot.isHidden = !(hasBadge && !isActive)
        }
    }

    /// Round-3 R5: flip the pill into editable mode. The label
    /// becomes a bezeled NSTextField, closeButton + badgeDot hide
    /// to free the trailing slot, and the label's trailing anchor
    /// swaps to the pill edge so the entry spans the full width.
    /// `initial` is captured + threaded through to `onCommit` so the
    /// commit handler's no-op detection (round-2 CR-fix) compares
    /// against the actually-displayed text instead of `liveTitle`.
    @MainActor
    func beginEdit(
        initial: String,
        onCommit: @escaping @MainActor (String, String) -> Void,
        onCancel: @escaping @MainActor () -> Void
    ) {
        guard !isEditing else { return }
        isEditing = true
        let initialTitle = initial
        self.onCommit = { committed in onCommit(committed, initialTitle) }
        self.onCancel = onCancel

        // Hide the trailing-slot competitors. `statusSlot` is leading
        // and doesn't compete for width, so it stays.
        closeButton.isHidden = true
        badgeDot.isHidden = true

        labelTrailingNormal.isActive = false
        labelTrailingEdit.isActive = true
        pillEditMinWidth.isActive = true

        label.stringValue = initial
        label.isEditable = true
        label.isSelectable = true
        label.drawsBackground = true
        label.backgroundColor = .textBackgroundColor
        label.isBezeled = true
        label.bezelStyle = .squareBezel
        label.delegate = self
        window?.makeFirstResponder(label)
        if let editor = label.currentEditor() as? NSTextView {
            editor.selectAll(nil)
        }
    }

    /// End edit mode without firing commit/cancel callbacks (those
    /// are called from the Enter / Escape / focus-loss paths). The
    /// next `configure(...)` call from `rebuildTabBar` re-syncs the
    /// label text + closeButton/badgeDot visibility.
    @MainActor
    func endEdit() {
        guard isEditing else { return }
        isEditing = false
        label.isEditable = false
        label.isSelectable = false
        label.drawsBackground = false
        label.isBezeled = false
        label.delegate = nil
        onCommit = nil
        onCancel = nil
        labelTrailingEdit.isActive = false
        labelTrailingNormal.isActive = true
        pillEditMinWidth.isActive = false
        closeButton.isHidden = !isActive
        badgeDot.isHidden = true  // next configure() re-sets correctly
    }

    /// NSControlTextEditingDelegate hook for Enter / Escape. Matches
    /// `ProjectRowCellView`'s pattern — Enter commits, Escape cancels,
    /// both transitions end the edit synchronously before invoking
    /// the callback so a re-entrant rebuild observes `isEditing ==
    /// false`.
    func control(
        _ control: NSControl,
        textView: NSTextView,
        doCommandBy commandSelector: Selector
    ) -> Bool {
        if commandSelector == #selector(NSResponder.insertNewline(_:)) {
            let committed = label.stringValue
            let commit = onCommit
            endEdit()
            commit?(committed)
            return true
        }
        if commandSelector == #selector(NSResponder.cancelOperation(_:)) {
            let cancel = onCancel
            endEdit()
            cancel?()
            return true
        }
        return false
    }

    /// Focus-loss == cancel, matching the sidebar's policy. Better
    /// to lose typing the user didn't confirm than to surprise them
    /// with a commit they didn't intend.
    func controlTextDidEndEditing(_ obj: Notification) {
        guard isEditing else { return }
        let cancel = onCancel
        endEdit()
        cancel?()
    }

    /// `mouseDown` only captures the event for the drag-start path;
    /// the click action (`onSelect`) is deferred to `mouseUp` so that
    /// a drag isn't mis-fired as a click. This was the round-1 M2
    /// drag bug: firing `onSelect` here triggered `selectTab` →
    /// `rebuildTabBar`, which destroyed this view before
    /// `mouseDragged` could begin a dragging session. AppKit's
    /// canonical pattern for "draggable button-like view" is to track
    /// mouseDown → mouseDragged (start drag at threshold) → mouseUp
    /// (fire click if no drag happened).
    ///
    /// Round-3 R5: bypass the drag plumbing while editing so the
    /// bezeled NSTextField handles clicks for caret placement and
    /// text selection.
    override func mouseDown(with event: NSEvent) {
        if isEditing {
            super.mouseDown(with: event)
            return
        }
        mouseDownEvent = event
        isDragging = false
    }

    /// Begin a dragging session once the user drags more than ~3 pts
    /// from the mouse-down origin. Pasteboard payload is the tab id
    /// (M2's `roostTabID` UTI) so `TabBarStackView` can resolve the
    /// source row independent of intervening order changes. Pills
    /// that haven't received their daemon id yet (still mid-OpenTab)
    /// quietly skip the drag.
    override func mouseDragged(with event: NSEvent) {
        if isEditing {
            super.mouseDragged(with: event)
            return
        }
        guard !isDragging,
              let downEvent = mouseDownEvent,
              let tabID = tabID
        else { return }
        let dx = event.locationInWindow.x - downEvent.locationInWindow.x
        let dy = event.locationInWindow.y - downEvent.locationInWindow.y
        if (dx * dx) + (dy * dy) < 9 { return }  // 3pt threshold
        isDragging = true

        let item = NSPasteboardItem()
        item.setString(String(tabID), forType: .roostTabID)
        let draggingItem = NSDraggingItem(pasteboardWriter: item)
        draggingItem.setDraggingFrame(bounds, contents: snapshotImage())
        beginDraggingSession(with: [draggingItem], event: downEvent, source: self)
    }

    /// Click action fires on `mouseUp` rather than `mouseDown` (see
    /// `mouseDown` comment) — only when no drag took place. A drag
    /// that begins via `mouseDragged` calls `beginDraggingSession`,
    /// which intercepts the rest of the event stream; this `mouseUp`
    /// only runs for plain clicks.
    override func mouseUp(with event: NSEvent) {
        if isEditing {
            super.mouseUp(with: event)
            return
        }
        let wasClick = !isDragging && mouseDownEvent != nil
        mouseDownEvent = nil
        isDragging = false
        if wasClick {
            onSelect(index)
        }
    }

    /// Snapshot the pill's current rendering so the drag image looks
    /// like the pill the user grabbed. AppKit's
    /// `dataWithPDF(inside:)` is the path used by every TextEdit / Mail
    /// snippet drag — works for any layer-backed NSView regardless of
    /// subview composition.
    private func snapshotImage() -> NSImage {
        let data = self.dataWithPDF(inside: bounds)
        return NSImage(data: data) ?? NSImage(size: bounds.size)
    }

    @objc private func closeClicked() {
        onClose(index)
    }

    /// Right-click context menu (M4 of `goal-mac-parity-2026-05-18.md`).
    /// AppKit calls `menu(for:)` on every right-click; returning a
    /// per-call NSMenu instead of installing one statically keeps the
    /// item targets fresh — each menu holds a reference to *this*
    /// pill's index even after the strip rebuilds. The Go binary doesn't
    /// have a tab-pill menu (sidebar-only); we extend Mac slightly past
    /// Go parity here because cmux + Linux M8 do this and the user has
    /// been training the muscle memory.
    override func menu(for event: NSEvent) -> NSMenu? {
        let menu = NSMenu()

        let rename = NSMenuItem(
            title: "Rename…",
            action: #selector(renameFromMenu(_:)),
            keyEquivalent: ""
        )
        rename.target = self
        menu.addItem(rename)

        let close = NSMenuItem(
            title: "Close Tab",
            action: #selector(closeFromMenu(_:)),
            keyEquivalent: ""
        )
        close.target = self
        menu.addItem(close)

        return menu
    }

    @objc private func renameFromMenu(_ sender: NSMenuItem) {
        onRename(index)
    }

    @objc private func closeFromMenu(_ sender: NSMenuItem) {
        onClose(index)
    }
}

extension TabPillView: NSDraggingSource {
    nonisolated func draggingSession(
        _ session: NSDraggingSession,
        sourceOperationMaskFor context: NSDraggingContext
    ) -> NSDragOperation {
        // Only allow drops inside the same app — cross-app pill drags
        // aren't a thing.
        context == .withinApplication ? .move : []
    }

}

// MARK: - Sidebar NSOutlineView data source + delegate

/// Cell view for one project row. Pulled out so the outline view's
/// `viewFor:` delegate path stays a one-liner. `NSTableCellView`'s
/// built-in `textField` outlet is what AppKit's source-list styling
/// targets for selection-state color flips, so we wire our label
/// through that outlet rather than holding a separate `NSTextField`
/// reference.
/// Indicator position for `ProjectRowCellView.setDropIndicator(_:)`.
/// Round-2 F6: AppKit's built-in NSOutlineView drop indicator is too
/// subtle on the dark theme — the user reported it was hard to tell
/// where a dragged project would land. The cell view paints its own
/// 2pt accent band when set.
enum DropIndicator {
    /// No indicator.
    case none
    /// 2pt accent band along the cell's top edge — drop will land
    /// above this row.
    case above
    /// 2pt accent band along the cell's bottom edge — used only for
    /// the last row when the drop is past the end of the list.
    case below
}

@MainActor
final class ProjectRowCellView: NSTableCellView, NSTextFieldDelegate {
    /// Phase 6a P7: small accent-tinted dot in the right column
    /// when any tab in the project has a pending notification.
    /// Hidden by default; `configure(with:notifying:rollup:)` flips it.
    private let badgeDot: NSView

    /// M6 of `goal-mac-parity-2026-05-18.md`: 3px stripe on the
    /// leading edge colored by the per-project rollup. Hidden when
    /// the rollup is `.none`. The Go binary's GTK row uses a CSS
    /// `box-shadow: inset 3px 0 0 <color>`; we reproduce the same
    /// visual with an explicit NSView so the row owns the rendering
    /// (NSTableCellView styling can be subtle).
    private let stripe: NSView

    /// M5 of `goal-mac-parity-2026-05-18.md`: true while the user is
    /// typing into the row's `textField` after a context-menu Rename.
    /// `configure(...)` skips overwriting `stringValue` while editing
    /// so a sibling `roost-cli-rs project rename` arriving mid-edit
    /// doesn't clobber the user's in-progress text. Mirrors Linux M9
    /// (`crates/roost-linux/src/app.rs::commit_rename_project`).
    private(set) var isEditing = false
    private var onCommit: (@MainActor (String) -> Void)?
    private var onCancel: (@MainActor () -> Void)?

    /// Round-2 F6: 2pt accent bands toggled by `setDropIndicator(_:)`
    /// to give a visible cue during sidebar drag-reorder. AppKit's
    /// built-in indicator is too subtle on the dark theme.
    private let dropTopBand: NSView
    private let dropBottomBand: NSView

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

        let stripe = NSView()
        stripe.translatesAutoresizingMaskIntoConstraints = false
        stripe.wantsLayer = true
        stripe.isHidden = true
        self.stripe = stripe

        let top = NSView()
        top.translatesAutoresizingMaskIntoConstraints = false
        top.wantsLayer = true
        top.layer?.backgroundColor = NSColor.controlAccentColor.cgColor
        top.isHidden = true
        self.dropTopBand = top

        let bottom = NSView()
        bottom.translatesAutoresizingMaskIntoConstraints = false
        bottom.wantsLayer = true
        bottom.layer?.backgroundColor = NSColor.controlAccentColor.cgColor
        bottom.isHidden = true
        self.dropBottomBand = bottom

        super.init(frame: .zero)
        addSubview(stripe)
        addSubview(field)
        addSubview(dot)
        addSubview(top)
        addSubview(bottom)
        textField = field
        NSLayoutConstraint.activate([
            stripe.leadingAnchor.constraint(equalTo: leadingAnchor),
            stripe.topAnchor.constraint(equalTo: topAnchor),
            stripe.bottomAnchor.constraint(equalTo: bottomAnchor),
            stripe.widthAnchor.constraint(equalToConstant: 3),

            field.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),
            field.trailingAnchor.constraint(equalTo: dot.leadingAnchor, constant: -6),
            field.centerYAnchor.constraint(equalTo: centerYAnchor),

            dot.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            dot.centerYAnchor.constraint(equalTo: centerYAnchor),
            dot.widthAnchor.constraint(equalToConstant: 8),
            dot.heightAnchor.constraint(equalToConstant: 8),

            top.leadingAnchor.constraint(equalTo: leadingAnchor),
            top.trailingAnchor.constraint(equalTo: trailingAnchor),
            top.topAnchor.constraint(equalTo: topAnchor),
            top.heightAnchor.constraint(equalToConstant: 3),

            bottom.leadingAnchor.constraint(equalTo: leadingAnchor),
            bottom.trailingAnchor.constraint(equalTo: trailingAnchor),
            bottom.bottomAnchor.constraint(equalTo: bottomAnchor),
            bottom.heightAnchor.constraint(equalToConstant: 3),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    /// Round-2 F6: toggle the 3pt accent band that marks this row as
    /// the drop target during sidebar drag-reorder. Cheap visual cue
    /// added because AppKit's built-in NSOutlineView drop indicator
    /// is too subtle on the dark theme. Round-7 R7.C bumped the band
    /// from 2pt → 3pt so it matches the tab strip's drop indicator
    /// line — same width, same `controlAccentColor`, uniform UX.
    func setDropIndicator(_ position: DropIndicator) {
        dropTopBand.isHidden = position != .above
        dropBottomBand.isHidden = position != .below
    }

    func configure(with project: ProjectSnapshot, notifying: Bool, rollup: RollupState) {
        // M5 race guard: while the user is editing this row, never
        // overwrite the textField from the model. A `.projectRenamed`
        // event arriving for *this* project mid-edit updates the model
        // (so cancel/commit pick up the new name when the user
        // eventually exits edit mode) but leaves the typing buffer
        // alone.
        if !isEditing {
            textField?.stringValue = project.name
        }
        badgeDot.isHidden = !notifying
        if let color = rollup.nsColor {
            stripe.layer?.backgroundColor = color.cgColor
            stripe.isHidden = false
        } else {
            stripe.isHidden = true
        }
    }

    /// Begin inline rename. The row's `textField` flips to an editable
    /// bezeled style, the user's caret is placed inside, and text is
    /// pre-selected so they can type to replace. Enter fires `onCommit`,
    /// Escape fires `onCancel`. Both transitions end the edit
    /// synchronously (the displayed `stringValue` will then re-sync
    /// from the model on the next `configure(...)`).
    func beginEdit(
        initial: String,
        onCommit: @escaping @MainActor (String) -> Void,
        onCancel: @escaping @MainActor () -> Void
    ) {
        guard let field = textField, !isEditing else { return }
        isEditing = true
        self.onCommit = onCommit
        self.onCancel = onCancel
        field.stringValue = initial
        field.isEditable = true
        field.isSelectable = true
        field.drawsBackground = true
        field.backgroundColor = .textBackgroundColor
        field.isBezeled = true
        field.bezelStyle = .roundedBezel
        field.delegate = self
        window?.makeFirstResponder(field)
        if let editor = field.currentEditor() as? NSTextView {
            editor.selectAll(nil)
        }
    }

    private func endEdit() {
        guard isEditing, let field = textField else { return }
        isEditing = false
        field.isEditable = false
        field.isSelectable = false
        field.drawsBackground = false
        field.isBezeled = false
        field.delegate = nil
        onCommit = nil
        onCancel = nil
    }

    /// NSControlTextEditingDelegate hook for Enter / Escape.
    /// `insertNewline:` fires for Enter; `cancelOperation:` for Escape.
    /// Returning `true` tells AppKit we handled the command so the
    /// underlying NSTextView doesn't also process it.
    func control(
        _ control: NSControl,
        textView: NSTextView,
        doCommandBy commandSelector: Selector
    ) -> Bool {
        if commandSelector == #selector(NSResponder.insertNewline(_:)) {
            let text = textField?.stringValue ?? ""
            let commit = onCommit
            endEdit()
            commit?(text)
            return true
        }
        if commandSelector == #selector(NSResponder.cancelOperation(_:)) {
            let cancel = onCancel
            endEdit()
            cancel?()
            return true
        }
        return false
    }

    /// Treat focus-loss as "cancel": the user clicked away without
    /// pressing Enter. Linux M9 treats it the same way to avoid silent
    /// commits — better to lose typing the user didn't confirm than
    /// surprise them with a rename they didn't intend.
    func controlTextDidEndEditing(_ obj: Notification) {
        guard isEditing else { return }
        let cancel = onCancel
        endEdit()
        cancel?()
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

    // MARK: - M3 drag-and-drop

    /// Emit a `roostProjectID` pasteboard item carrying the project id
    /// when the user starts dragging a sidebar row. The drop target
    /// resolves this back to a local row position; we don't carry the
    /// row index because the displayed order may change mid-drag if
    /// a sibling client reorders.
    func outlineView(
        _ outlineView: NSOutlineView,
        pasteboardWriterForItem item: Any
    ) -> NSPasteboardWriting? {
        guard let row = item as? ProjectRowItem else { return nil }
        let writer = NSPasteboardItem()
        writer.setString(String(row.project.id), forType: .roostProjectID)
        return writer
    }

    /// Accept drops between top-level rows only. NSOutlineView
    /// proposes `.on item` drops by default — clamp to `.above index`
    /// so the user can't accidentally drop a project onto another
    /// project's row (which would visually be a no-op anyway, but the
    /// move arrow looks confusing).
    ///
    /// Round-2 F6: also drive the custom drop indicator on
    /// `ProjectRowCellView` — `setDropIndicator(_:)` paints a 2pt
    /// accent band so the user sees where the drop will land.
    /// AppKit's built-in indicator is too subtle on dark theme.
    func outlineView(
        _ outlineView: NSOutlineView,
        validateDrop info: any NSDraggingInfo,
        proposedItem item: Any?,
        proposedChildIndex index: Int
    ) -> NSDragOperation {
        guard item == nil, index >= 0 else {
            updateDropIndicator(to: nil)
            return []
        }
        guard info.draggingPasteboard.types?.contains(.roostProjectID) == true else {
            updateDropIndicator(to: nil)
            return []
        }
        outlineView.setDropItem(nil, dropChildIndex: index)
        updateDropIndicator(to: index)
        return .move
    }

    /// Toggle the custom drop indicator bands on the cached
    /// `ProjectRowCellView`s. Clears the previous indicator before
    /// setting the new one so transitioning between rows during a
    /// drag never leaves two indicators visible. `nil` clears
    /// everything (used on drop / drag-end / drag-exit).
    @MainActor
    private func updateDropIndicator(to newIndex: Int?) {
        // Clear the previous indicator if it's different from the new
        // target. Walking the small cache map is cheap.
        if dropIndicatorIndex != newIndex {
            for cell in projectRowCellViews.values {
                cell.setDropIndicator(.none)
            }
        }
        dropIndicatorIndex = newIndex
        guard let newIndex else { return }
        if newIndex < projects.count {
            // Drop above the project at this index.
            if let cell = projectRowCellViews[projects[newIndex].id] {
                cell.setDropIndicator(.above)
            }
        } else if let last = projects.last,
                  let cell = projectRowCellViews[last.id]
        {
            // Drop past the last row — paint the indicator on the
            // bottom edge of the final cell.
            cell.setDropIndicator(.below)
        }
    }

    /// Resolve the drop to a final order and fire `ReorderProjects`.
    /// Local mutation is *not* applied here — the `.projectsReordered`
    /// event arm is the single source of truth (cross-cutting
    /// "WatchEvents-only mutation" invariant from the goal doc).
    func outlineView(
        _ outlineView: NSOutlineView,
        acceptDrop info: any NSDraggingInfo,
        item: Any?,
        childIndex index: Int
    ) -> Bool {
        defer { updateDropIndicator(to: nil) }
        guard let idStr = info.draggingPasteboard.string(forType: .roostProjectID),
              let sourceID = Int64(idStr),
              let sourceIdx = projects.firstIndex(where: { $0.id == sourceID })
        else { return false }
        let mapped = computeInsertIdx(sourceIdx: sourceIdx, rawTargetIdx: index)
        // CodeRabbit on PR #68: returning `false` for a same-position
        // drop makes AppKit play the "rejected drop" animation, which
        // is misleading — the gesture was valid, the order just didn't
        // change. Return `true` so AppKit treats it as a successful
        // (zero-effect) drop.
        if mapped.isNoop { return true }

        var ids = projects.map { $0.id }
        let source = ids.remove(at: sourceIdx)
        let clamped = min(max(mapped.index, 0), ids.count)
        ids.insert(source, at: clamped)

        let socket = socketPath
        Task {
            await reorderProjects(socketPath: socket, projectIDs: ids)
        }
        return true
    }

    /// Round-2 F6: clear the drop indicator when the drag ends — fires
    /// even when the user releases outside the outline view (cancelled
    /// drag). Without this, a cancelled drag would leave a stray
    /// indicator band visible until the next drag.
    func outlineView(
        _ outlineView: NSOutlineView,
        draggingSession session: NSDraggingSession,
        endedAt screenPoint: NSPoint,
        operation: NSDragOperation
    ) {
        updateDropIndicator(to: nil)
    }
}

extension RoostApp: NSOutlineViewDelegate {
    func outlineView(
        _ outlineView: NSOutlineView,
        viewFor tableColumn: NSTableColumn?,
        item: Any
    ) -> NSView? {
        guard let row = item as? ProjectRowItem else { return nil }
        // M5 of `goal-mac-parity-2026-05-18.md`: reuse cell views per
        // project so inline rename's typing buffer survives reload.
        // Map keyed by project id — purged in `.projectDeleted` arm.
        let cell: ProjectRowCellView
        if let existing = projectRowCellViews[row.project.id] {
            cell = existing
        } else {
            cell = ProjectRowCellView()
            projectRowCellViews[row.project.id] = cell
        }
        // Phase 6a P7 per-project rollup: badge the sidebar row
        // if ANY tab in this project has a pending notification.
        let projectTabs = tabs.filter { $0.projectID == row.project.id }
        let notifying = projectTabs.contains { $0.liveHasNotification }
        // M6 of `goal-mac-parity-2026-05-18.md`: per-project rollup
        // stripe colored by the highest-priority tab state. Computed
        // on every reload — bounded by N tabs in the project.
        let pairs: [(state: TabAgentState, hookActive: Bool)] = projectTabs.map {
            (state: TabAgentState.fromProto(Int($0.liveState ?? 0)),
             hookActive: $0.hookActive)
        }
        let rollup = projectRollup(tabs: pairs)
        cell.configure(with: row.project, notifying: notifying, rollup: rollup)
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

extension RoostApp: NSSplitViewDelegate {
    /// Round-6 R6.B: clamp + persist the sidebar pane's width.
    /// `splitViewDidResizeSubviews` fires on every divider drag
    /// (live, not on mouse-up) so the saved value tracks the user's
    /// final position. Skip when the sidebar is hidden so a `⌘B`
    /// toggle-collapse doesn't overwrite the saved width with `0`.
    func splitViewDidResizeSubviews(_ notification: Notification) {
        // Round-6 R6.B: skip the persistence save until launch has
        // fully settled. NSSplitView fires this during initial
        // layout before our setPosition + width constraints have
        // taken effect; persisting that pre-settle width would
        // pollute the saved value with NSSplitView's auto-picked
        // default.
        guard sidebarPersistenceActive else { return }
        guard let sidebar = sidebarPane else { return }
        let w = sidebar.frame.width
        // Round-7 R7.A: skip persistence while the pane is in the
        // ⌘B-collapsed state (frame.width = 0). The user's
        // pre-collapse width is held in `sidebarRestoreWidth` and
        // already persisted at the moment the user dragged the
        // divider before pressing ⌘B.
        guard w >= Self.sidebarMinWidth, w <= Self.sidebarMaxWidth else { return }
        UserDefaults.standard.set(Double(w), forKey: Self.sidebarWidthDefaultsKey)
    }

    /// Lower bound for the interactive divider drag. AppKit calls
    /// this every frame during drag; `max(proposed, ours)` honors
    /// both AppKit's geometry (window-edge insets, etc.) and our
    /// `sidebarMinWidth` floor without ever inverting the bounds.
    /// CR on PR #75: returning a fixed constant could exceed
    /// AppKit's proposed max on narrow windows and cause jitter.
    ///
    /// Round-7 R7.A: when `sidebarCollapsingProgrammatically` is
    /// true (the brief window inside `toggleSidebar`'s collapse
    /// path), return 0 so `setPosition(0, …)` can land the divider
    /// flush against the window's leading edge. The flag flips
    /// back to false synchronously after the setPosition call so
    /// the user's next interactive drag still honors the 160pt
    /// floor.
    func splitView(
        _ splitView: NSSplitView,
        constrainMinCoordinate proposedMinimumPosition: CGFloat,
        ofSubviewAt dividerIndex: Int
    ) -> CGFloat {
        if sidebarCollapsingProgrammatically { return 0 }
        return max(proposedMinimumPosition, Self.sidebarMinWidth)
    }

    /// Upper bound for the interactive divider drag. Symmetric to
    /// the lower bound above — `min(proposed, ours)` so a window
    /// narrower than `sidebarMaxWidth` doesn't get our 400pt cap
    /// pushed past the right edge.
    func splitView(
        _ splitView: NSSplitView,
        constrainMaxCoordinate proposedMaximumPosition: CGFloat,
        ofSubviewAt dividerIndex: Int
    ) -> CGFloat {
        min(proposedMaximumPosition, Self.sidebarMaxWidth)
    }

    /// Round-7 R7.A: hide the sidebar/content divider while the
    /// sidebar pane is collapsed via ⌘B (frame.width == 0). Without
    /// this, NSSplitView keeps the 1pt divider visible as a seam
    /// against the window's left edge even when the pane has fully
    /// collapsed. AppKit consults this on every layout pass.
    func splitView(_ splitView: NSSplitView, shouldHideDividerAt dividerIndex: Int) -> Bool {
        guard dividerIndex == 0 else { return false }
        return (sidebarPane?.frame.width ?? 1) < 1
    }

    /// Round-7 R7.A: marks the sidebar (subview 0) as collapsible
    /// so NSSplitView treats the `setPosition(0, …)` call as a
    /// proper collapse — without this, NSSplitView may snap the
    /// divider back to the constrainMin floor on the next layout
    /// pass. Required for the ⌘B collapse to stick.
    func splitView(_ splitView: NSSplitView, canCollapseSubview subview: NSView) -> Bool {
        return subview === sidebarPane
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
