// Roost Mac client — top-level AppKit app + view-model glue.
//
// The window splits horizontally into a project sidebar (left) and
// the tab-bar + terminal area (right). Each project owns its own
// set of `TabSession`s; switching the sidebar selection rebuilds
// the tab bar with only that project's tabs.
//
// Project lifecycle calls go through the in-process `LocalClient`
// (see `RoostClient.swift`'s thin wrappers around
// `RoostBackend.shared.localClient`):
//   * `+ New Project` at the bottom of the sidebar → `createProject`.
//   * Right-click a project row → Rename / Delete (Delete cascades
//     the project's tabs in `Workspace.deleteProject`, which we
//     mirror locally before refreshing the sidebar).
//   * The File menu gains "New Project" (⌘N).
//
// Cross-client convergence (e.g. when `roostctl` mutates the
// workspace from another shell) flows through `RoostEvent` via
// `watchEvents`, which subscribes to `Workspace.subscribe` and
// converts each event for `handleEvent` below.

import AppKit
import CGhosttyVT
import Foundation
import Sparkle

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
    // nonisolated so the nonisolated `sidebarVisibleOnLaunch` helper can
    // read it (a plain Sendable String constant — safe from any context).
    nonisolated private static let sidebarVisibleDefaultsKey = "RoostSidebarVisible"

    /// Round-6 R6.B: persisted sidebar width, plus the clamp bounds
    /// used by the `NSSplitViewDelegate` callbacks. Width survives
    /// quit/relaunch via `UserDefaults`. Bounds: 160pt floor (any
    /// narrower and the "Untitled N" rows truncate awkwardly under
    /// the body font), 400pt cap (past that the terminal grid
    /// shrinks too aggressively on a 1200pt window).
    static let sidebarMinWidth: CGFloat = 160
    static let sidebarMaxWidth: CGFloat = 400
    private static let sidebarWidthDefaultsKey = "RoostSidebarWidth"

    /// Defaults store for UI prefs (sidebar visibility + width). Normally
    /// `UserDefaults.standard`; when `ROOST_DEFAULTS_SUITE` is set (the E2E
    /// harness) it redirects to a throwaway suite so a test run never reads
    /// or writes the developer's real prefs — the UserDefaults analog of
    /// the `ROOST_STATE_DIR` state.json isolation (which can't reach
    /// UserDefaults). Prod behavior is unchanged when the env is unset.
    // Computed (not a stored `static let`): `UserDefaults` is non-Sendable
    // under Swift 6, so a stored nonisolated global is rejected. A computed
    // property has no shared storage; `UserDefaults(suiteName:)` returns the
    // shared backing store for a given suite, so repeated access is cheap +
    // consistent.
    nonisolated static var uiDefaults: UserDefaults {
        if let suite = ProcessInfo.processInfo.environment["ROOST_DEFAULTS_SUITE"],
           !suite.isEmpty, let store = UserDefaults(suiteName: suite) {
            return store
        }
        return .standard
    }

    /// Whether the sidebar should start visible, given a defaults store.
    /// A missing value (never toggled) → visible; an explicit `false` →
    /// collapsed. Pure + injectable so the launch-restore decision is
    /// unit-tested without standing up the full app — the Mac analog of
    /// the Rust `sidebar_collapsed_persists_across_reopen` test, covering
    /// the regression class the CI-skipped relaunch e2e can't.
    nonisolated static func sidebarVisibleOnLaunch(_ defaults: UserDefaults) -> Bool {
        (defaults.object(forKey: sidebarVisibleDefaultsKey) as? Bool) ?? true
    }

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

    /// Monotonic counter for provider sub-frame ids (see
    /// `nextProviderFrameID`). Main-actor isolated like the rest of the app.
    private var providerFrameSeq = 0

    /// Monotonic token bumped per provider run, so a superseded run's late
    /// result (provider A in flight, user picks B) is dropped rather than
    /// pushing a stale frame onto the current one.
    private var providerReq = 0

    /// M4c: the single-instance flock held for the lifetime of the
    /// process. Released when the holder is dropped (the lock fd is
    /// closed in `SingleInstance.deinit`), which happens at app
    /// shutdown.
    private var singleInstance: SingleInstance?
    private var daemonReachable: Bool = false

    /// Sparkle 2 auto-update controller (issue #122). Retained for the
    /// app's lifetime; it owns the background update scheduler and
    /// backs the "Check for Updates…" menu item. Updates are
    /// authenticated by Sparkle's EdDSA signature (`SUPublicEDKey` in
    /// Info.plist) — independent of Apple Developer ID, which is still
    /// pending (#83). Created in `applicationDidFinishLaunching` before
    /// `installMainMenu()` so the menu item can target it.
    private var updaterController: SPUStandardUpdaterController?

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
    /// Name of the active theme (the bundled file name, e.g. `Dracula`,
    /// `roost-dark`). `activeTheme` is a nameless value struct, so we
    /// track the name separately to pre-highlight the live theme in the
    /// palette and to express revert-by-name. Not persisted — relaunch
    /// re-reads `config.conf`.
    private var activeThemeName: String = "roost-dark"

    /// Command palette (Cmd+Shift+P). Nil when closed. `paletteOpen`
    /// gates app shortcuts (defense-in-depth in `validateMenuItem`) and
    /// the focus-snap in `applicationDidBecomeActive` while it's up.
    /// `themeNameAtOpen` is the theme to restore if a live preview is
    /// dismissed without confirming.
    private var palette: PalettePanel?
    private var paletteOpen = false
    private var themeNameAtOpen: String?
    /// Font family captured when the palette opened, restored on
    /// dismiss-without-confirm so an in-flight live preview reverts.
    /// `nil` while the palette is closed. The inner `String?` carries
    /// "no family configured" (= system default) distinctly from the
    /// outer "palette not open" state.
    private var fontFamilyAtOpen: String??
    /// Live font family (post-`Select Font` selection). `nil` means
    /// the user has no `font-family =` line in their config; the
    /// renderer falls back to the system monospace default. Tracked
    /// separately from `config.fontFamily` because that's the
    /// boot-time snapshot — the live value can drift past it via the
    /// palette without touching the boot config.
    private var activeFontFamily: String?
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

    /// Live inbox of pending agent notifications, surfaced through the
    /// command palette ("View Notifications") and the Dock badge.
    /// Membership is driven off the `has_notification` edges in
    /// `handleEvent`; see `NotificationInbox`.
    private var notificationInbox = NotificationInbox()

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

        // M4c: file logger goes live before any other startup work
        // so the early logs land in roost.log alongside the os.Logger
        // output. Idempotent — `attach` swaps the handle without
        // dropping queued writes.
        RoostLogger.shared.attach(path: profile.logPath)

        // M4c: single-instance enforcement. Two Mac UIs cannot share
        // an IPC socket; the second launch detects the live lock,
        // logs the holder PID, and exits 0. `ROOST_ALLOW_MULTI=1`
        // bypasses for dev / test workflows (Xcode debug, swift test).
        //
        // `holdsSingleInstanceLock` is forwarded to RoostBackend so
        // M6's stale-socket recovery (IPCServer.bindWithRecovery)
        // only fires when we genuinely own the flock; on the
        // ROOST_ALLOW_MULTI bypass we surface .alreadyBound on
        // EADDRINUSE rather than unlinking the live other instance.
        var holdsLock = false
        do {
            switch try SingleInstance.acquire(lockPath: profile.lockPath) {
            case .acquired(let lock):
                self.singleInstance = lock
                holdsLock = true
                RoostLogger.shared.info(
                    "single-instance: acquired lock at \(profile.lockPath)"
                )
            case .alreadyHeld(let pid):
                RoostLogger.shared.info(
                    "single-instance: lock at \(profile.lockPath) already held by pid \(pid); exiting"
                )
                NSApp.terminate(nil)
                return
            case .bypassed:
                RoostLogger.shared.info(
                    "single-instance: bypassed via ROOST_ALLOW_MULTI=1"
                )
            }
        } catch {
            // Lock fd open / write failed for a reason other than
            // contention. Log and continue — better to start with no
            // single-instance guard than to refuse to launch.
            RoostLogger.shared.error(
                "single-instance: acquire failed: \(error); continuing without enforcement"
            )
        }

        // Register the UI bridge *before* `start()` binds the IPC
        // socket, so `RoostBackend.shared.ui` is never nil while the
        // socket is reachable. The window + tabs are built below, so
        // bridge-backed ops still surface their own honest errors until
        // then (`mainWindow` nil → "no window"; no tab → "not-found")
        // rather than a misleading "no UI attached". Storing `self` is
        // side-effect-free; `self` is fully initialized as the delegate.
        RoostBackend.shared.registerUI(self)
        RoostBackend.shared.start(
            profile: profile,
            holdsSingleInstanceLock: holdsLock
        )

        let socketPath = profile.socketPath
        self.socketPath = socketPath

        // Phase 6a M6: read user config + resolve theme + font
        // before anything draws. Missing config → `.empty`; missing
        // theme name → bundled `roost-dark`. Font defaults to
        // `.monospacedSystemFont(ofSize: 14)` unless the user
        // overrides `font-family` / `font-size`.
        self.config = RoostConfig.load()
        self.activeThemeName = config.themeName ?? "roost-dark"
        self.activeTheme = Theme.loadBundled(name: activeThemeName)
        self.activeFontFamily = config.fontFamily
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

        // Sparkle 2 auto-update (issue #122). Stand the updater up
        // before installMainMenu() so the "Check for Updates…" item
        // can target it. `startingUpdater: true` kicks off the
        // background scheduler; whether it checks automatically is
        // governed by SUEnableAutomaticChecks / SUScheduledCheckInterval
        // in Info.plist. Feed URL + EdDSA public key also come from
        // Info.plist (SUFeedURL / SUPublicEDKey).
        self.updaterController = SPUStandardUpdaterController(
            startingUpdater: true,
            updaterDelegate: nil,
            userDriverDelegate: nil
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
            // Guard tab existence BEFORE the reveal so a click on a
            // banner for a since-closed tab doesn't uncollapse the
            // sidebar (and rewrite `RoostSidebarVisible = true`) for
            // a navigation that won't happen. Same guard pattern as
            // `jumpToUnread`. `focusTab` itself is pure data mutation
            // per DL-11.
            guard let self,
                  self.tabs.contains(where: { $0.id == tabID })
            else { return }
            self.ensureSidebarVisible()
            self.focusTab(tabID: tabID)
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
        let storedSidebarWidth = Self.uiDefaults.double(forKey: Self.sidebarWidthDefaultsKey)
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
        // visible) from "explicitly false" (user hid it) — see
        // `sidebarVisibleOnLaunch`.
        let startsCollapsed = !Self.sidebarVisibleOnLaunch(Self.uiDefaults)

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
        // Persist + fsync the tab layout BEFORE any teardown, so we
        // capture the full in-memory layout. This fires on Cmd+Q, menu
        // Quit, the red close button (applicationShouldTerminateAfter-
        // LastWindowClosed == true), and the empty-workspace
        // window?.close() path. flush() then freezes further
        // persistence so the tab-close loop below (which can drive
        // PTY-exit closes back through the workspace) can't overwrite
        // the flushed layout with an empty one.
        RoostBackend.shared.workspace?.flush()
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
        // While the palette is up it owns focus (its text field); don't
        // yank first responder back to the terminal on reactivation.
        guard !paletteOpen else { return }
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

    /// Switch the active theme at runtime and broadcast it to every
    /// open terminal (all tabs, all projects — `tabs` is the flat
    /// list). Not persisted on its own — used by both live preview
    /// (`previewTheme`) and revert (`revertTheme`); the commit-only
    /// persist lives in `commitTheme`. Resolves the palette once and
    /// reuses it across terminals so live-preview arrowing stays
    /// cheap. New tabs read `activeTheme` on spawn, so both confirm
    /// and revert propagate forward.
    @MainActor
    private func setActiveTheme(_ theme: Theme, name: String) {
        activeTheme = theme
        activeThemeName = name
        let resolved = theme.resolved()
        for session in tabs {
            session.terminalView.setTheme(theme, resolved: resolved)
        }
    }

    // MARK: - Command palette (Cmd+Shift+P)

    @objc @MainActor
    private func showCommandPalette(_ sender: Any?) {
        guard palette == nil, let window else { return }
        themeNameAtOpen = activeThemeName
        fontFamilyAtOpen = .some(activeFontFamily)
        paletteOpen = true

        let root = PaletteFrame(
            id: "commands",
            placeholder: "Execute a command…",
            items: paletteCommandItems()
        )
        let behavior = PaletteBehavior(onConfirm: { [weak self] item in
            self?.confirmPaletteCommand(item) ?? .close
        })
        let panel = PalettePanel(
            parent: window,
            contentRegion: terminalContainer,
            root: root,
            behavior: behavior
        ) { [weak self] in
            self?.dismissPalette()
        }
        palette = panel
        panel.present()
    }

    /// Panel teardown callback: clear state, re-key the main window,
    /// then restore terminal focus (order matters — focus won't take on
    /// a non-key window).
    @MainActor
    private func dismissPalette() {
        palette = nil
        paletteOpen = false
        themeNameAtOpen = nil
        fontFamilyAtOpen = nil
        window?.makeKeyAndOrderFront(nil)
        focusActiveTerminal()
    }

    // MARK: - Command launcher (Cmd+Shift+T)

    /// Open the custom command launcher directly on the configured
    /// command list — a dedicated root picker (Esc closes), mirroring
    /// `showCommandPalette` but with the launcher frame + behavior.
    /// No-op if a palette is already open.
    @objc @MainActor
    private func showCommandLauncher(_ sender: Any?) {
        guard palette == nil, let window else { return }
        paletteOpen = true
        // Snapshot the config once and thread it through both the frame
        // and the behavior, so the row the user sees and the command that
        // launches are guaranteed to be the same entry even if config.conf
        // changes while the picker is open. Reloading on each open (rather
        // than caching on App) still picks up edits without a restart,
        // matching how `showCommandPalette` reads config fresh.
        let commands = RoostConfig.load().commands
        let panel = PalettePanel(
            parent: window,
            contentRegion: terminalContainer,
            root: launcherFrame(commands: commands),
            behavior: launcherBehavior(commands: commands)
        ) { [weak self] in
            self?.dismissPalette()
        }
        palette = panel
        panel.present()
    }

    /// Build the launcher frame from a config snapshot's `command =`
    /// list. An empty list yields the "No commands configured" sentinel.
    @MainActor
    private func launcherFrame(commands: [CustomCommand]) -> PaletteFrame {
        PaletteFrame(
            id: "launcher",
            placeholder: "Run a command…",
            items: launcherItems(commands)
        )
    }

    /// Confirm on a launcher row → spawn that command in a new tab. The
    /// row id's index is resolved against the same `commands` snapshot the
    /// frame was built from. The `launch:none` sentinel (and any stale id)
    /// is a no-op (stay open). Mac launches directly in the confirm and
    /// returns `.close` (matches the notification jump's direct `focusTab`).
    @MainActor
    private func launcherBehavior(commands: [CustomCommand]) -> PaletteBehavior {
        PaletteBehavior(onConfirm: { [weak self] item in
            guard let self else { return .close }
            guard let index = launchIndex(item.id), commands.indices.contains(index) else {
                return .none  // "No commands configured" sentinel / stale id
            }
            self.launchCommand(commands[index])
            return .close
        })
    }

    /// Spawn `cmd` in a new tab of the active project, in the active tab's
    /// live cwd, running it through the user's login shell. Auto-close on
    /// exit + the non-sticky title are handled by the existing tab
    /// infrastructure — everything else rides in the argv built by
    /// `launchArgv`.
    @MainActor
    private func launchCommand(_ cmd: CustomCommand) {
        guard daemonReachable, let projectID = activeProjectID else { return }
        let shell = ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/sh"
        let argv = launchArgv(shell: shell, command: cmd)
        let cwd = activeLaunchCwd(projectID: projectID)
        guard let session = openTab(inProject: projectID, cwd: cwd, title: cmd.title, argv: argv)
        else { return }
        let projectTabs = tabsForActiveProject()
        let insertedIndex = projectTabs.firstIndex(where: { $0 === session })
            ?? max(0, projectTabs.count - 1)
        rebuildTabBar()
        selectTab(at: insertedIndex)
    }

    // MARK: - Custom palette (Cmd+Shift+E) — script-backed providers

    /// Open the custom palette on the configured provider list
    /// (`provider =` entries + discovered scripts), presented as the root
    /// frame like the launcher. No-op if a palette is already open.
    @objc @MainActor
    private func showCustomPalette(_ sender: Any?) {
        guard palette == nil, let window else { return }
        paletteOpen = true
        let providers = RoostConfig.load().providers
        let panel = PalettePanel(
            parent: window,
            contentRegion: terminalContainer,
            root: providerListFrame(providers),
            behavior: providerListBehavior(providers)
        ) { [weak self] in
            self?.dismissPalette()
        }
        palette = panel
        panel.present()
    }

    /// The provider-list frame: one row per configured provider (or the
    /// "No providers configured" sentinel when empty).
    @MainActor
    private func providerListFrame(_ providers: [Provider]) -> PaletteFrame {
        PaletteFrame(id: "custom", placeholder: "Custom commands…", items: providerItems(providers))
    }

    /// Confirm on a provider row → run its `list` phase off-main, then
    /// drill into the resulting rows (pushed asynchronously). The
    /// `provider:none` sentinel and any stale id are a no-op.
    @MainActor
    private func providerListBehavior(_ providers: [Provider]) -> PaletteBehavior {
        PaletteBehavior(onConfirm: { [weak self] item in
            guard let self else { return .close }
            guard let idx = providerIndex(item.id), providers.indices.contains(idx) else {
                return .none
            }
            let provider = providers[idx]
            Task { @MainActor in await self.openProviderList(provider) }
            return .none
        })
    }

    /// Run a provider's `list` phase and push its rows as a sub-frame. If
    /// the palette closed while the script ran, the result is dropped.
    @MainActor
    private func openProviderList(_ provider: Provider) async {
        let want = palette
        providerReq += 1
        let req = providerReq
        let result = await runProvider(provider, phase: .list, selectedID: nil)
        // Apply only if no newer provider run superseded this one and the
        // palette that asked for it is still on screen — a dismiss + reopen
        // during the spawn must not be clobbered.
        guard req == providerReq, let panel = palette, panel === want, panel.isLive else { return }
        pushProviderResult(panel: panel, provider: provider, result: result)
    }

    /// Confirm on a provider item → run its `activate` phase with the
    /// selected id. The script acts (usually via `$ROOST_SOCKET`); its
    /// stdout may drill in (more rows) or be empty (close the palette).
    @MainActor
    private func providerItemBehavior(_ provider: Provider) -> PaletteBehavior {
        PaletteBehavior(onConfirm: { [weak self] item in
            // Non-actionable rows (the overflow hint, a provider's
            // `actionable:false` row) never reach here — `confirm` skips
            // them before invoking the behavior.
            guard let self else { return .close }
            Task { @MainActor in await self.activateProviderItem(provider, selectedID: item.id) }
            return .none
        })
    }

    @MainActor
    private func activateProviderItem(_ provider: Provider, selectedID: String) async {
        let want = palette
        providerReq += 1
        let req = providerReq
        let result = await runProvider(provider, phase: .activate, selectedID: selectedID)
        // Same stale-result guard as `openProviderList` (newer run + panel).
        guard req == providerReq, let panel = palette, panel === want, panel.isLive else { return }
        // Empty success = "done, close"; rows or error drills in / shows it.
        if case .success(let out) = result, out.items.isEmpty {
            panel.driveDismiss()
        } else {
            pushProviderResult(panel: panel, provider: provider, result: result)
        }
    }

    /// Push a provider's parsed output (or an error row) as a sub-frame.
    @MainActor
    private func pushProviderResult(
        panel: PalettePanel, provider: Provider, result: Result<ProviderOutput, ProviderError>
    ) {
        switch result {
        case .success(let out):
            let placeholder = out.placeholder.isEmpty ? "\(provider.title)…" : out.placeholder
            let frame = PaletteFrame(
                id: nextProviderFrameID(), placeholder: placeholder,
                items: providerOutputPaletteItems(out, limit: provider.limit))
            panel.drivePush(frame: frame, behavior: providerItemBehavior(provider))
        case .failure(let err):
            let msg = err.message
            NSLog("roost-mac: provider '%@' failed: %@", provider.label, msg)
            let frame = PaletteFrame(
                id: nextProviderFrameID(), placeholder: "Provider error",
                items: [PaletteItem(id: "provider:_error", title: "Provider failed", subtitle: msg)])
            panel.drivePush(frame: frame, behavior: PaletteBehavior(onConfirm: { _ in .none }))
        }
    }

    /// Assemble the active-tab context handed to a provider run.
    @MainActor
    private func providerContext(selectedID: String?) -> ProviderContext {
        var ctx = ProviderContext()
        ctx.socket = socketPath
        ctx.selectedID = selectedID
        ctx.query = palette?.driveSnapshot().query ?? ""
        if let pid = activeProjectID {
            ctx.activeProjectID = pid
            ctx.activeCwd = activeLaunchCwd(projectID: pid)
            if let session = activeSessionByProject[pid] {
                ctx.activeTabID = session.id
                ctx.activeTitle = session.liveTitle ?? ""
            }
        }
        return ctx
    }

    /// Run one provider phase as a subprocess (off the main actor, with
    /// the provider's timeout) and parse its stdout.
    @MainActor
    private func runProvider(
        _ provider: Provider, phase: ProviderPhase, selectedID: String?
    ) async -> Result<ProviderOutput, ProviderError> {
        let shell = ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/sh"
        let ctx = providerContext(selectedID: selectedID)
        let argv = providerInvocationArgv(
            shell: shell, run: provider.run, shellInterpret: provider.shellInterpret, phase: phase)
        let env = providerInvocationEnv(phase: phase, ctx: ctx)
        let stdinStr = providerInvocationStdin(phase: phase, ctx: ctx)
        let cwd = ctx.activeCwd
        let timeout = provider.timeoutSecs
        return await Task.detached(priority: .userInitiated) {
            runProviderProcess(argv: argv, env: env, stdin: stdinStr, cwd: cwd, timeoutSecs: timeout)
        }.value
    }

    /// Monotonic id for a pushed provider sub-frame, so nested drill-ins
    /// don't share a behaviors-map key (which a pop would remove for both).
    @MainActor
    private func nextProviderFrameID() -> String {
        providerFrameSeq += 1
        return "provider:items:\(providerFrameSeq)"
    }

    /// The launch cwd: the active tab's live (OSC 7-tracked) cwd, else
    /// the project's cwd, else "" (the open-tab path then resolves
    /// $HOME). Mirrors `updateWindowTitle`'s cwd resolution.
    @MainActor
    private func activeLaunchCwd(projectID: Int64) -> String {
        let session = activeSessionByProject[projectID]
        // Prefer a native read of the active tab's shell cwd: it reflects
        // the *current* directory even for shells that don't emit OSC 7
        // (e.g. stock /bin/bash), and a new tab spawns a LOCAL shell, so
        // the local path is what it should inherit. Fall back to the
        // OSC 7-tracked cwd, then the project's stored cwd.
        var native: String?
        if let tabID = session?.id {
            native = RoostBackend.shared.supervisor?.foregroundCwd(tabID: tabID)
        }
        let live = session?.liveCwd ?? ""
        let project = projects.first(where: { $0.id == projectID })?.cwd ?? ""
        return Self.resolveLaunchCwd(native: native, live: live, project: project)
    }

    /// New-tab cwd precedence: native shell cwd (current, local) →
    /// OSC 7-tracked cwd → project cwd. Pure + `nonisolated` so it's
    /// unit-testable without standing up the app.
    nonisolated static func resolveLaunchCwd(native: String?, live: String, project: String) -> String {
        if let native, !native.isEmpty { return native }
        if !live.isEmpty { return live }
        return project
    }

    @MainActor
    private func confirmPaletteCommand(_ item: PaletteItem) -> PaletteOutcome {
        switch item.id {
        case PaletteCommands.selectThemeID:
            return .push(themeFrame(), themeBehavior())
        case PaletteCommands.selectFontID:
            return .push(fontFrame(), fontBehavior())
        case PaletteCommands.viewNotificationsID:
            return .push(notificationsFrame(), notificationsBehavior())
        case PaletteCommands.clearNotificationsID:
            clearAllNotifications()
            return .close
        case "custom_commands":
            // Dynamic drill-in surfaced in the command palette when
            // providers are configured. Reload fresh (matches the
            // launcher) and push the custom frame.
            let providers = RoostConfig.load().providers
            return .push(providerListFrame(providers), providerListBehavior(providers))
        default:
            runCommand(item.id)
            return .close
        }
    }

    /// Curated first-cut command list. Ids are `KeybindAction` ids
    /// (except `selectTheme` + the notification commands), so they
    /// dispatch through `runCommand` and show their shortcut. Indexed
    /// switch actions, clipboard, and `command_palette` itself are
    /// intentionally excluded.
    ///
    /// The two notification commands sit just below the Select Theme… /
    /// Select Font… drill-ins and are built here rather than in `specs`
    /// so "View Notifications" can carry the live pending count.
    @MainActor
    private func paletteCommandItems() -> [PaletteItem] {
        var items = PaletteCommands.specs.map { id, title in
            PaletteItem(id: id, title: title, trailingText: shortcutText(for: id))
        }
        let count = notificationInbox.count
        let viewTitle = count > 0 ? "View Notifications (\(count))" : "View Notifications"
        let notifItems = [
            PaletteItem(id: PaletteCommands.viewNotificationsID, title: viewTitle),
            PaletteItem(id: PaletteCommands.clearNotificationsID, title: "Clear All Notifications"),
        ]
        let insertAt = (items.firstIndex { $0.id == PaletteCommands.selectFontID }).map { $0 + 1 } ?? items.count
        items.insert(contentsOf: notifItems, at: insertAt)
        // Surface the custom palette (script-backed providers) as a
        // drill-in row, but only when at least one provider is configured.
        if !RoostConfig.load().providers.isEmpty {
            items.append(
                PaletteItem(
                    id: "custom_commands", title: "Custom Commands…",
                    trailingText: shortcutText(for: KeybindAction.customPalette)))
        }
        return items
    }

    /// Map the active theme list onto palette items, pre-highlighting
    /// the live theme. Names are shown verbatim (so `roost-dark` stays
    /// lowercase) to keep `id == bundled file name`.
    @MainActor
    private func themeFrame() -> PaletteFrame {
        let names = Theme.bundledNames()
        let items = names.map { PaletteItem(id: $0, title: $0) }
        let selection = names.firstIndex(of: activeThemeName) ?? 0
        return PaletteFrame(
            id: "themes",
            placeholder: "Select a theme…",
            items: items,
            selection: selection
        )
    }

    @MainActor
    private func themeBehavior() -> PaletteBehavior {
        PaletteBehavior(
            onHighlight: { [weak self] item in self?.previewTheme(name: item.id) },
            onConfirm: { [weak self] item in
                self?.commitTheme(name: item.id)
                return .close
            },
            onCancel: { [weak self] in self?.revertTheme() }
        )
    }

    /// Build the font sub-frame: curated programming fonts first
    /// (filtered to those NSFont reports installed), then every
    /// other monospace family alphabetically. Pre-selects the live
    /// family.
    @MainActor
    private func fontFrame() -> PaletteFrame {
        let families = availableFontFamilies()
        // Match against the PRIMARY entry of a comma list (e.g. the
        // value `"Fira Code, Monospace"` should pre-select "Fira
        // Code"). Mirrors `App::font_frame` on the Linux side.
        let active = activeFontFamily ?? activeFont.familyName ?? ""
        let primary =
            active
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first(where: { !$0.isEmpty }) ?? ""
        let selection =
            families.firstIndex(where: { $0.caseInsensitiveCompare(primary) == .orderedSame })
            ?? 0
        let items = families.map { PaletteItem(id: $0, title: $0) }
        return PaletteFrame(
            id: "fonts",
            placeholder: "Select a font…",
            items: items,
            selection: selection
        )
    }

    @MainActor
    private func fontBehavior() -> PaletteBehavior {
        PaletteBehavior(
            onHighlight: { [weak self] item in self?.previewFontFamily(name: item.id) },
            onConfirm: { [weak self] item in
                self?.commitFontFamily(name: item.id)
                return .close
            },
            onCancel: { [weak self] in self?.revertFontFamily() }
        )
    }

    /// Curated programming fonts that look great in a terminal, in a
    /// thoughtful order. The first entries that are actually
    /// installed lead the picker; uninstalled entries are skipped.
    /// Mirrors `App::CURATED_FONTS` on the Linux side.
    private static let curatedFonts: [String] = [
        "JetBrains Mono",
        "JetBrainsMono Nerd Font",
        "Fira Code",
        "Fira Mono",
        "Hack",
        "Source Code Pro",
        "Cascadia Code",
        "Cascadia Mono",
        "IBM Plex Mono",
        "Inconsolata",
        "Iosevka",
        "SF Mono",
        "Menlo",
        "Monaco",
        // Cross-platform additions; usually missing on macOS so the
        // installed-filter step elides them, but kept here so the two
        // platforms' curated lists stay close.
        "DejaVu Sans Mono",
        "Ubuntu Mono",
        "Liberation Mono",
        "Noto Sans Mono",
    ]

    /// Enumerate font families for the picker: curated first
    /// (filtered to installed), then every other monospace family
    /// alphabetically. Uses `NSFontManager` to enumerate the
    /// installed monospace set.
    @MainActor
    private func availableFontFamilies() -> [String] {
        let manager = NSFontManager.shared
        // `availableFontNames(with:[.fixedPitchFontMask])` returns
        // *typeface* names (e.g. "Menlo-Regular"), one row per
        // weight/style; we want family names. Cross-reference with
        // `availableFontFamilies` (which is family-deduped) and keep
        // the families that have at least one fixed-pitch face.
        let allFamilies = Set(manager.availableFontFamilies)
        let monoTypefaces = manager.availableFontNames(with: [.fixedPitchFontMask]) ?? []
        var monoFamilies = Set<String>()
        for face in monoTypefaces {
            // `NSFont(name:size:).familyName` is the most reliable
            // map from typeface → family; fall back to a hyphen-strip
            // heuristic if AppKit refuses to resolve a face.
            if let family = NSFont(name: face, size: 12)?.familyName,
               allFamilies.contains(family)
            {
                monoFamilies.insert(family)
            }
        }
        let installed = { (name: String) -> Bool in
            allFamilies.contains(where: { $0.caseInsensitiveCompare(name) == .orderedSame })
        }
        var seen = Set<String>()
        var out: [String] = []
        for entry in Self.curatedFonts {
            if installed(entry), !seen.contains(entry.lowercased()) {
                // Use the canonical name from `allFamilies` if it's a
                // case mismatch.
                let canonical =
                    allFamilies.first(where: { $0.caseInsensitiveCompare(entry) == .orderedSame })
                    ?? entry
                out.append(canonical)
                seen.insert(canonical.lowercased())
            }
        }
        let remaining = monoFamilies
            .sorted { $0.localizedCaseInsensitiveCompare($1) == .orderedAscending }
        for name in remaining where !seen.contains(name.lowercased()) {
            out.append(name)
            seen.insert(name.lowercased())
        }
        return out
    }

    // MARK: - Notification inbox

    /// Build the notifications sub-frame from the live inbox snapshot.
    /// Each row encodes its tab id as `"notif:<id>"` (parsed on confirm
    /// to jump). An empty inbox shows a single non-actionable row.
    @MainActor
    private func notificationsFrame() -> PaletteFrame {
        let records = notificationInbox.snapshot()
        let items: [PaletteItem]
        if records.isEmpty {
            items = [PaletteItem(id: "notif:none", title: "No notifications")]
        } else {
            items = records.map { r in
                PaletteItem(
                    id: "notif:\(r.tabID)",
                    title: r.title,
                    subtitle: r.body.isEmpty ? nil : r.body,
                    trailingText: relativeTimeLabel(from: r.at)
                )
            }
        }
        return PaletteFrame(
            id: "notifications",
            placeholder: "Jump to a notification…",
            items: items
        )
    }

    /// Confirm on a notification row → focus that project + tab. The
    /// "No notifications" sentinel is a no-op (stay open). Focusing
    /// clears the tab's `has_notification`, which drives the inbox
    /// false-edge → the row + badge clear (see `selectTab`).
    @MainActor
    private func notificationsBehavior() -> PaletteBehavior {
        PaletteBehavior(onConfirm: { [weak self] item in
            guard let self else { return .close }
            guard let tabID = Self.tabID(fromNotifItemID: item.id) else {
                return .none  // "No notifications" sentinel
            }
            // Update the *core* active tab (so identify / tab.focus report
            // the jump target), not just UI selection — the `.active`
            // event then drives the project switch + tab select. Routing
            // through the workspace, rather than the prior UI-only
            // `focusTab`, keeps the core the source of truth (matches the
            // GTK `focus_tab_by_id` fix). Raise the app (a jump wants
            // focus) and clear the tab's notification.
            //
            // User action: reveal the sidebar so the jump destination is
            // visible; the `.active` event arm uses pure `focusTab` and
            // can't be relied on to uncollapse for us.
            self.ensureSidebarVisible()
            _ = try? RoostBackend.shared.localClient?.focusTab(tabID)
            NSApp.activate(ignoringOtherApps: true)
            let socket = self.socketPath
            Task.detached { await clearTabNotification(socketPath: socket, tabID: tabID) }
            return .close
        })
    }

    /// Parse `"notif:<id>"` → the tab id, or nil for the sentinel /
    /// malformed ids.
    private static func tabID(fromNotifItemID id: String) -> Int64? {
        guard let raw = id.split(separator: ":", maxSplits: 1).last else { return nil }
        return Int64(raw)
    }

    /// "Clear All Notifications": clear each pending tab's notification
    /// through the workspace. Each clear emits the `tab.notification`
    /// false-edge, which is the single source of truth — its handler
    /// drops the inbox row, refreshes the Dock badge, and rebuilds the
    /// dot. Driving removal only off that edge (no parallel local clear)
    /// keeps list == dots == badge even if a clear fails: that tab
    /// simply stays pending rather than the UI desyncing from the
    /// workspace.
    @MainActor
    private func clearAllNotifications() {
        let socket = socketPath
        for tabID in notificationInbox.tabIDs {
            Task.detached {
                await clearTabNotification(socketPath: socket, tabID: tabID)
            }
        }
    }

    /// Bring `tabID`'s tab forward: switch project if needed, select the
    /// tab within it, and (when `activate`) raise the app. Shared by the
    /// OS-banner click handler and the inbox jump (which want the app
    /// raised) and the workspace `.active` arm for external IPC focus
    /// (`roostctl tab focus`), which switches the visible tab without
    /// stealing OS focus from the user's frontmost app — matching the
    /// GTK `ActiveChanged` arm, which switches the page but never calls
    /// `window.present()`.
    ///
    /// Pure data mutation: does not touch sidebar visibility. Inherits
    /// that contract from `selectProject(id:)`. Call sites that should
    /// reveal the sidebar (user-action paths — banner click, palette
    /// confirm, ⌘⇧U) must invoke `ensureSidebarVisible()` before this
    /// call (or before the workspace-routed `localClient?.focusTab`
    /// that re-enters here via the `.active` arm).
    @MainActor
    private func focusTab(tabID: Int64, activate: Bool = true) {
        guard let session = tabs.first(where: { $0.id == tabID }) else { return }
        // selectProject is idempotent when the id already matches.
        if session.projectID != activeProjectID {
            selectProject(id: session.projectID)
        }
        let projectTabs = tabs.filter { $0.projectID == session.projectID }
        if let idx = projectTabs.firstIndex(where: { $0 === session }) {
            selectTab(at: idx)
        }
        if activate {
            NSApp.activate(ignoringOtherApps: true)
        }
    }

    /// Read any tab's terminal viewport as text for the `tab.dump` IPC
    /// op (not just the active one). `nil` when no `TabSession` holds
    /// that id. Called on the main actor from `IPCHandlerImpl`.
    @MainActor
    func dumpTab(tabID: Int64) -> TerminalView.Dump? {
        guard let session = tabs.first(where: { $0.id == tabID }) else { return nil }
        return session.terminalView.dumpText()
    }

    /// Mirror the inbox count onto the Dock tile badge. `nil` at zero so
    /// the badge disappears entirely (AppKit shows nothing for an empty
    /// label).
    @MainActor
    private func refreshDockBadge() {
        let count = notificationInbox.count
        NSApp.dockTile.badgeLabel = count > 0 ? String(count) : nil
    }

    @MainActor
    private func previewTheme(name: String) {
        guard name != activeThemeName else { return }  // skip redundant re-apply
        setActiveTheme(Theme.loadBundled(name: name), name: name)
    }

    @MainActor
    private func revertTheme() {
        guard let name = themeNameAtOpen, name != activeThemeName else { return }
        setActiveTheme(Theme.loadBundled(name: name), name: name)
    }

    /// Commit the user's Enter on the theme sub-frame: make sure
    /// live state matches `name` (highlight normally does this; a
    /// fast-Enter without ever moving is a no-preview path), then
    /// persist to `~/.config/roost/config.conf` so the next launch
    /// picks the same theme. Preview + revert deliberately do NOT
    /// call this — they only mutate in-memory state.
    @MainActor
    private func commitTheme(name: String) {
        if name != activeThemeName {
            setActiveTheme(Theme.loadBundled(name: name), name: name)
        }
        if let err = writeBackTheme(name) {
            NSLog("roost-mac: failed to persist theme to config.conf: %@", "\(err)")
        }
    }

    @MainActor
    private func previewFontFamily(name: String) {
        if name == activeFontFamily { return }
        setActiveFontFamily(name)
    }

    /// Revert to the font family captured when the palette opened.
    /// The double-Optional snapshot disambiguates "palette never
    /// opened" (outer nil) from "user had no `font-family =` line"
    /// (inner nil).
    @MainActor
    private func revertFontFamily() {
        guard let target = fontFamilyAtOpen else { return }
        if target == activeFontFamily { return }
        setActiveFontFamily(target)
    }

    /// Commit the user's Enter on the font sub-frame: ensure live
    /// state matches `name`, then persist to config. Counterpart to
    /// `commitTheme`.
    ///
    /// Preserves a comma-separated fallback chain (e.g. `"JetBrains
    /// Mono, Monospace"`) when the user confirms the chain's primary
    /// — the picker only exposes individual family names but a user
    /// may have hand-edited a fallback into config. The check is
    /// against the **at-open snapshot**, not the live preview
    /// value: if the user previewed another font and arrowed back,
    /// the live state is already the stripped primary, so comparing
    /// against the live value would still drop the fallback.
    @MainActor
    private func commitFontFamily(name: String) {
        // `fontFamilyAtOpen` is `Optional<Optional<String>>` —
        // outer nil = palette never opened (defensive); inner nil =
        // user had no font-family line. Flatten to a plain
        // `String?`.
        let opened: String? = fontFamilyAtOpen.flatMap { $0 }
        let openedPrimary =
            opened?
            .split(separator: ",")
            .first
            .map { $0.trimmingCharacters(in: .whitespaces) }
        if openedPrimary?.caseInsensitiveCompare(name) == .orderedSame {
            // No-op confirm: restore the opened chain to live state
            // (an interim preview may have replaced it with the bare
            // primary) and DON'T rewrite the file — it already has
            // the chain the user opened with.
            if activeFontFamily != opened {
                setActiveFontFamily(opened)
            }
            return
        }
        setActiveFontFamily(name)
        if let err = writeBackFontFamily(name) {
            NSLog("roost-mac: failed to persist font-family to config.conf: %@", "\(err)")
        }
    }

    /// Apply `family` (nil = system monospace) at the current size.
    /// Used by both preview and commit; commit additionally calls
    /// `writeBackFontFamily`.
    @MainActor
    private func setActiveFontFamily(_ family: String?) {
        activeFontFamily = family
        let size = activeFont.pointSize
        let newFont = resolveFont(family: family, size: size)
        activeFont = newFont
        for session in tabs {
            session.terminalView.updateFont(newFont)
        }
    }

    /// Persist `theme = <name>` to the user's config file. Returns
    /// the error to the caller (which logs once at the user-action
    /// boundary), per the repo convention "return errors rather
    /// than logging-and-swallowing them; log at the boundary that
    /// handles the error". A failed write must not crash the UI;
    /// the in-memory selection still works for the rest of the
    /// session.
    @MainActor
    @discardableResult
    private func writeBackTheme(_ name: String) -> Error? {
        RoostConfig.setKey("theme", value: name)
    }

    /// Persist `font-family = "<name>"` to config. The value is
    /// wrapped in double quotes since family names commonly contain
    /// spaces ("JetBrains Mono"); the parser strips them on read.
    @MainActor
    @discardableResult
    private func writeBackFontFamily(_ name: String) -> Error? {
        RoostConfig.setKey("font-family", value: "\"\(name)\"")
    }

    /// Persist `font-size = <pt>` to config. Whole values render as
    /// integers ("14") rather than floats ("14.0").
    @MainActor
    @discardableResult
    private func writeBackFontSize(_ size: CGFloat) -> Error? {
        RoostConfig.setKey("font-size", value: formatFontSize(size))
    }

    /// Format a font size for the config file. Whole numbers render
    /// as integers; non-whole values keep up to two decimals (trailing
    /// zeros trimmed) so a `font-size = 14.5` round-trips cleanly.
    /// `nonisolated static` for unit-testing without an `App`.
    ///
    /// Locale-pinned to POSIX (`en_US_POSIX`) — `String(format:)`
    /// is locale-aware, so a French/German UI would otherwise write
    /// `font-size = 14,5` which the parser (which goes through
    /// `Double(_ value: String)` and accepts only `.` as the
    /// decimal separator) silently rejects on next launch.
    nonisolated static func formatFontSize(_ size: CGFloat) -> String {
        let rounded = size.rounded()
        if abs(rounded - size) < 0.001 {
            return String(Int(rounded))
        }
        let s = String(format: "%.2f", locale: Locale(identifier: "en_US_POSIX"), Double(size))
        var trimmed = s
        while trimmed.hasSuffix("0") { trimmed.removeLast() }
        if trimmed.hasSuffix(".") { trimmed.removeLast() }
        return trimmed
    }

    /// Non-static wrapper so call sites can stay terse.
    @MainActor
    private func formatFontSize(_ size: CGFloat) -> String {
        Self.formatFontSize(size)
    }

    /// Dispatch a palette command through the same handlers the menu
    /// uses, so there's a single code path. New tabs/projects etc. land
    /// exactly as if the shortcut was pressed.
    @MainActor
    private func runCommand(_ id: String) {
        switch id {
        case KeybindAction.newTab:        newTab(nil)
        case KeybindAction.closeTab:      closeActiveTab(nil)
        case KeybindAction.renameTab:     renameActiveTab(nil)
        case KeybindAction.cycleTabNext:  cycleTabNext(nil)
        case KeybindAction.cycleTabPrev:  cycleTabPrev(nil)
        case KeybindAction.newProject:    newProject(nil)
        case KeybindAction.renameProject: renameActiveProject(nil)
        case KeybindAction.closeProject:  closeActiveProject(nil)
        case KeybindAction.toggleSidebar: toggleSidebar(nil)
        case KeybindAction.jumpToUnread:  jumpToUnread(nil)
        case KeybindAction.fontIncrease:  fontIncrease(nil)
        case KeybindAction.fontDecrease:  fontDecrease(nil)
        case KeybindAction.fontReset:     fontReset(nil)
        default:
            NSLog("roost-mac: palette runCommand got unknown id %@", id)
        }
    }

    /// Render an action's keybind as a glyph string ("⌘⇧P") for the
    /// palette's right-hand hint. Nil when the action is unbound.
    @MainActor
    private func shortcutText(for action: String) -> String? {
        let (key, mods) = accel(for: action)
        if key.isEmpty { return nil }
        var s = ""
        if mods.contains(.control) { s += "⌃" }
        if mods.contains(.option) { s += "⌥" }
        if mods.contains(.shift) { s += "⇧" }
        if mods.contains(.command) { s += "⌘" }
        switch key {
        case "\r": s += "⏎"
        case "\u{1b}": s += "⎋"
        case "\t": s += "⇥"
        case " ": s += "␣"
        default: s += key.uppercased()
        }
        return s
    }

    /// Defense-in-depth shortcut gate: while the palette is open and
    /// capturing keystrokes, don't let Roost's own command shortcuts
    /// (⌘T, ⌘W, …) fire from the still-live main menu. Only items
    /// targeting `self` route here — clipboard (NSText) and Quit
    /// (NSApplication) are validated elsewhere and stay live. The
    /// palette toggle itself stays enabled (re-press is a no-op).
    @objc @MainActor
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        // The picker toggles (palette + launcher) stay live while a
        // palette is open; both `show*` guard on `palette == nil`, so a
        // re-press is a harmless no-op.
        if paletteOpen,
            menuItem.action != #selector(showCommandPalette(_:)),
            menuItem.action != #selector(showCommandLauncher(_:)),
            menuItem.action != #selector(showCustomPalette(_:))
        {
            return false
        }
        return true
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
            // Take the persisted tab layout (one-shot) to re-open each
            // project's prior tabs as fresh shells in their saved dirs.
            let restore = await MainActor.run { RoostBackend.shared.workspace?.takeRestoreLayout() }
            await MainActor.run { [weak self] in
                guard let self else { return }
                self.projects = fetched
                self.rebuildSidebar()

                // Determine the active project + tab index up-front so
                // the right tab can be focused in the workspace as it's
                // opened. Fall back to the first project (tab 0) when
                // the saved active id is gone/unset.
                let activeID: Int64?
                let activePos: Int
                if let r = restore, self.projects.contains(where: { $0.id == r.activeProjectID }) {
                    activeID = r.activeProjectID
                    activePos = Int(r.activeTabPosition)
                } else {
                    activeID = self.projects.first?.id
                    activePos = 0
                }

                // Re-open each project's saved tabs (position order,
                // saved cwd + title). A project with no saved tabs —
                // or a state.json predating tab persistence — seeds a
                // single tab. Eager (not lazy-on-select) so `tab list`
                // and screenshots reflect every project's tabs, and so
                // the GTK + Mac builds restore identically.
                for project in self.projects {
                    let saved = restore?.projects
                        .first(where: { $0.projectID == project.id })?.tabs ?? []
                    let specs: [(cwd: String, title: String, userTitled: Bool)] =
                        saved.isEmpty
                            ? [("", "", false)]
                            : saved.map { ($0.cwd, $0.title, $0.userTitled) }
                    for (idx, spec) in specs.enumerated() {
                        let isActive = project.id == activeID && idx == activePos
                        self.openTab(
                            inProject: project.id,
                            cwd: spec.cwd,
                            title: spec.title,
                            userTitled: spec.userTitled,
                            focusInWorkspaceWhenReady: isActive
                        )
                    }
                }

                // Restore the UI active project + tab selection. This
                // is a launch-time restore, not a user action — and
                // `selectProject` is now pure data mutation, so the
                // user's collapsed-sidebar preference is preserved
                // automatically.
                if let pid = activeID {
                    self.selectProject(id: pid)
                    self.selectTabByPosition(in: pid, position: Int32(activePos))
                }
                // The subscription emits a `.resync` as its first event,
                // which reconciles any tab opened during the boot gap —
                // so no synchronous reconcile here (it would race the
                // async restore opens above).
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
                    await MainActor.run { [weak self] in
                        self?.handleEvent(event)
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
                notificationInbox.remove(tabID)
            }
            session.terminalView.removeFromSuperview()
            session.close(socketPath: socketPath)
        }
        refreshDockBadge()
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

    /// Materialize a `TabSession` for a workspace tab the UI doesn't yet
    /// hold and attach it to its supervisor PTY (so it shows in the strip
    /// and OSC scanning runs against its output). Deduped by id — calling
    /// it for a tab already shown is a no-op. Shared by the `.tabOpened`
    /// event arm (cross-client `tab.open`) and the boot-gap reconcile.
    @MainActor
    private func attachExistingTab(id: Int64, projectID: Int64, title: String, cwd: String) {
        guard !tabs.contains(where: { $0.id == id }) else { return }
        let session = TabSession(
            projectID: projectID,
            cols: 80,
            rows: 24,
            theme: activeTheme,
            font: activeFont,
            copyOnSelect: config.copyOnSelect,
            clipboardWrite: config.clipboardWrite,
            wordBreakChars: config.wordBreakChars
        )
        session.liveTitle = title
        session.liveCwd = cwd.isEmpty ? nil : cwd
        tabs.append(session)
        session.attach(socketPath: socketPath, tabID: id)
        if projectID == activeProjectID {
            rebuildTabBar()
        }
        rebuildSidebar()
    }

    /// Heal the boot gap. The event subscription is callback-based with
    /// no replay, so a tab opened via IPC between the server binding and
    /// the subscription registering had its `tabOpened` dropped — the UI
    /// would never materialize it. Driven by the `.resync` event the
    /// subscription emits as its first event (so it can't race the async
    /// restore opens at bootstrap): attach any workspace tab the UI is
    /// missing. Mirrors GTK's resync-on-subscribe
    /// (`crates/roost-linux/src/events.rs`); idempotent — a tab opened in
    /// the sliver between subscribe and the snapshot is just deduped.
    @MainActor
    private func attachMissingWorkspaceTabs() {
        guard let workspace = RoostBackend.shared.workspace else { return }
        for project in workspace.snapshot() {
            for tab in workspace.tabs(in: project.id)
            where !tabs.contains(where: { $0.id == tab.id }) {
                attachExistingTab(
                    id: tab.id, projectID: tab.projectId, title: tab.title, cwd: tab.cwd
                )
            }
        }
    }

    /// Dispatch one event from the WatchEvents stream. Anything not
    /// surfaced visually in M1 is logged and dropped — later
    /// milestones (M3 tab strip, Phase 6b notifications) light up
    /// the remaining cases.
    @MainActor
    private func handleEvent(_ kind: RoostEvent) {
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
            // Headless `roostctl tab close` or any other `tab.close`
            // RPC kills the PTY in-process; the supervisor's drain
            // task emits `.tabExited` → workspace emits
            // `.tabClosed` → we land here. The Mac UI still holds
            // the TabSession reference in `tabs` until this event;
            // tear it down now so the tab strip converges.
            guard let session = tabs.first(where: { $0.id == e.tabID }) else { break }
            // A closed tab can't hold a pending notification — drop its
            // inbox row to preserve "row exists iff tab has pending".
            notificationInbox.remove(e.tabID)
            refreshDockBadge()
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
            // Project-level cascade lives in `Workspace.closeTab`
            // (it deletes the parent project when its last tab is
            // closed). The Mac UI's local `tabs` list omits any
            // tabs opened externally via `roostctl tab open`, so a
            // UI-side empty check could under-count and delete a
            // project the workspace thinks still has tabs — the
            // workspace is authoritative. We just handle local
            // cleanup here; the cascade's `.projectDeleted` event
            // lands in the arm below and closes the window if the
            // workspace is empty.
            if projectID == activeProjectID {
                rebuildTabBar()
                if wasActive {
                    let remaining = tabsForActiveProject()
                    if !remaining.isEmpty {
                        selectTab(at: 0)
                    }
                }
            }
        case .tabOpened(let e):
            // Cross-client `tab.open` (e.g. `roostctl tab open`) produces
            // a workspace tab we don't yet hold a TabSession for; attach
            // it (matches GTK's auto-attach). UI-driven `openNewTab`
            // already inserted the matching TabSession before this event
            // arrives — `attachExistingTab` dedupes by id, so that case
            // is a no-op here.
            let newTab = e.tab
            attachExistingTab(
                id: newTab.id,
                projectID: newTab.projectID,
                title: newTab.title,
                cwd: newTab.cwd
            )
        case .active(let e):
            // Workspace-driven active-selection change. The UI is
            // authoritative for focus it initiates itself (pill click,
            // new-tab open, restore, cascade-close fallback): those
            // paths update the local selection *before* this echo
            // arrives, so the guard below makes us a no-op and avoids a
            // selectTab → focusTab → `.active` feedback loop. When the
            // change originates *outside* the UI — `roostctl tab focus`,
            // or any future external client — the UI hasn't switched
            // yet, so bring the requested tab forward (without raising
            // the app), matching GTK's `ActiveChanged` arm and the
            // documented `tab focus` = "click the pill" behavior, which
            // also clears the tab's notification badge via `selectTab`.
            let alreadyShown = activeProjectID == e.projectID
                && activeSessionByProject[e.projectID]?.id == e.tabID
            if !alreadyShown, e.tabID != 0,
               tabs.contains(where: { $0.id == e.tabID })
            {
                focusTab(tabID: e.tabID, activate: false)
            }
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
                session.liveHasNotification = e.hasPending
                if session.projectID == activeProjectID {
                    rebuildTabBar()
                }
                rebuildSidebar()  // sidebar's per-project rollup
            }
            // Inbox false-edge: clearing a tab's notification (focus,
            // prompt-submit, session-end, explicit clear) drops its row
            // + updates the Dock badge, keeping list == dots == badge.
            if !e.hasPending {
                notificationInbox.remove(e.tabID)
                refreshDockBadge()
            }
        case .notification(let e):
            // Live-inbox upsert: compose a project-forward row from the
            // model (title = "<project> · <tab>", subtitle = message),
            // keyed by tab id for dedup + jump. The `.tabNotification`
            // true-edge fires alongside this and lights the dot; the
            // false-edge (clear/focus/close) removes the row.
            if let session = tabs.first(where: { $0.id == e.tabID }) {
                let projectName = projects.first(where: { $0.id == session.projectID })?.name ?? "Project"
                let projectTabs = tabs.filter { $0.projectID == session.projectID }
                let index = projectTabs.firstIndex(where: { $0 === session }) ?? 0
                notificationInbox.upsert(NotificationRecord(
                    tabID: e.tabID,
                    projectID: session.projectID,
                    title: NotificationInbox.composeTitle(
                        project: projectName,
                        tab: pillLabel(for: session, index: index)
                    ),
                    body: e.body
                ))
                refreshDockBadge()
            }
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
        case .resync:
            // First event after the subscription registers: attach any
            // tab opened before it went live (the boot gap).
            attachMissingWorkspaceTabs()
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

    /// Pure data mutation: swap the active project + reconcile tabs.
    /// Does NOT touch sidebar visibility — call sites that want the
    /// sidebar revealed (user-action paths like ⌘1-9, sidebar-row
    /// click, palette confirm) must invoke `ensureSidebarVisible()`
    /// themselves before this call. Mirrors GTK's
    /// `set_active_project` and the vision.md DL-11 principle: "the
    /// UI is a reaction to the core's events, not a parallel source
    /// of truth." Programmatic callers (bootstrap, event reconcile,
    /// `focusTab` for external IPC, project-delete next-pick) get
    /// silent mutation and preserve the user's collapse intent.
    @MainActor
    private func selectProject(id: Int64) {
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
        // follow-on `selectProject(id:)` is pure data mutation per
        // DL-11 — this is the only reveal in the success path too.
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
                notificationInbox.remove(tabID)
            }
            session.terminalView.removeFromSuperview()
            session.close(socketPath: socketPath)
        }
        refreshDockBadge()
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
        // Title numbering is 1-based within the active project.
        let title = "roost-mac \(tabsForActiveProject().count + 1)"
        // Cmd+T inherits the active tab's live (OSC 7-tracked) cwd, then
        // project cwd, then $HOME — the same resolution the command
        // launcher (Cmd+Shift+T) already uses. Without this, new tabs
        // opened in $HOME instead of where the user was working.
        let cwd = activeLaunchCwd(projectID: projectID)
        guard let session = openTab(inProject: projectID, cwd: cwd, title: title) else { return }
        let projectTabs = tabsForActiveProject()
        let insertedIndex = projectTabs.firstIndex(where: { $0 === session })
            ?? max(0, projectTabs.count - 1)
        rebuildTabBar()
        selectTab(at: insertedIndex)
    }

    /// Open a tab in `projectID` starting at `cwd` (empty → the
    /// project's cwd, then $HOME, so a Finder-launched app doesn't
    /// drop the shell at `/`) with placeholder `title`, append + start
    /// its session. Does NOT change the active selection or rebuild the
    /// tab bar — the caller decides whether to focus it. Returns the
    /// new session. Shared by `openNewTab` (active project) and session
    /// restore (which passes each saved tab's cwd + title).
    @discardableResult
    private func openTab(
        inProject projectID: Int64,
        cwd: String,
        title: String,
        argv: [String] = [],
        userTitled: Bool = false,
        focusInWorkspaceWhenReady: Bool = false
    ) -> TabSession? {
        guard daemonReachable else { return nil }
        let session = TabSession(
            projectID: projectID,
            cols: 80,
            rows: 24,
            theme: activeTheme,
            font: activeFont,
            copyOnSelect: config.copyOnSelect,
            clipboardWrite: config.clipboardWrite,
            wordBreakChars: config.wordBreakChars
        )
        let resolvedCwd: String
        if !cwd.isEmpty {
            resolvedCwd = cwd
        } else {
            let projectCwd = projects.first(where: { $0.id == projectID })?.cwd ?? ""
            resolvedCwd = projectCwd.isEmpty
                ? (ProcessInfo.processInfo.environment["HOME"] ?? "")
                : projectCwd
        }
        // Pre-seed `liveCwd` so the new pill renders the
        // tilde-abbreviated path on frame 1, instead of flashing
        // "Tab N" while waiting for the shell's OSC 7. OSC 7 will
        // refine if the shell starts in a different directory.
        session.liveCwd = resolvedCwd.isEmpty ? nil : resolvedCwd
        tabs.append(session)
        session.start(socketPath: socketPath, title: title, cwd: resolvedCwd, argv: argv) { [weak self] tabID in
            // The id is now known; keep the window menu in sync so its
            // tag-driven ⌘1..⌘9 routes to the current tab order.
            // Also rebuild the tab bar so the pill that was created
            // pre-id gets re-cached against its (now-known) tab id.
            // Without this rebuild, `tabPillViews[id]` never gets
            // populated for the newly opened tab, and ⌘R
            // (renameActiveTab) silently no-ops because
            // `tabPillViews[tabID]` returns nil.
            self?.rebuildWindowMenu()
            self?.rebuildTabBar()
            // Restore: sync the WORKSPACE's active selection to this
            // tab once its real id exists, so the next persist (and
            // IPC `identify`) record the restored active tab, not the
            // last-opened one. The UI selection is set separately by
            // `selectTabByPosition`. #95 review.
            if focusInWorkspaceWhenReady {
                _ = try? RoostBackend.shared.workspace?.focusTab(tabID)
            }
            // Restore: re-assert the manual-rename lock. `openTab`
            // always seeds `userTitled=false` (the supplied title is
            // treated as a placeholder); `setTabTitle` flips it back
            // to true and emits a tabTitleChanged. Without this, the
            // first post-relaunch `setTabCwd` would re-derive the
            // title (issue #196 model fix). Log on failure so a
            // silently-lost lock is diagnosable — mirrors the Rust
            // twin's `tracing::warn!` in `app.rs::bootstrap` restore.
            if userTitled && !title.isEmpty {
                do {
                    try RoostBackend.shared.workspace?.setTabTitle(tabID, title: title)
                } catch {
                    RoostLogger.shared.warn(
                        "restore: failed to re-lock manual title for tab \(tabID) (title=\(title)): \(error); next cd will re-derive"
                    )
                }
            }
        }
        return session
    }

    /// Select the tab at `position` (0-based; equals open order, so it
    /// matches the saved layout's position) within `projectID`, when
    /// it is the active project. Used at bootstrap to restore the
    /// saved active tab. Sessions are appended in open order, so the
    /// index lines up with the persisted position.
    private func selectTabByPosition(in projectID: Int64, position: Int32) {
        guard activeProjectID == projectID else { return }
        let projectTabs = tabsForActiveProject()
        guard !projectTabs.isEmpty else { return }
        let idx = min(Int(max(0, position)), projectTabs.count - 1)
        selectTab(at: idx)
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
        if let id = session.id {
            notificationInbox.remove(id)
            refreshDockBadge()
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
            // The closed tab was the project's last. `session.close`
            // routes through `Workspace.closeTab`, which now cascades:
            // it deletes the empty project and emits `.projectDeleted`,
            // handled in `handleEvent` (fall back to another project,
            // or close the window when the workspace is empty). Don't
            // respawn a tab here — that would resurrect the project the
            // cascade is removing.
            rebuildTabBar()
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
        if let id = session.id {
            notificationInbox.remove(id)
            refreshDockBadge()
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
            // Closed the project's last tab via the pill ×. Mirror
            // `closeActiveTabImpl`: `session.close` routes through
            // `Workspace.closeTab`, whose cascade deletes the empty
            // project and emits `.projectDeleted` (handled in
            // `handleEvent` — fall back to another project, or close
            // the window when the workspace empties). Don't respawn a
            // tab here; that would resurrect the project being removed.
            rebuildTabBar()
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
        // User action (⌘1-9): reveal the sidebar so the user can see
        // which project they landed on. Mirrors Go `cmd/roost/app.go:1487`.
        ensureSidebarVisible()
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
        // Sparkle "Check for Updates…" (issue #122). Targets the
        // retained updaterController; its `checkForUpdates(_:)` opens
        // Sparkle's standard update UI. Disabled gracefully if the
        // controller failed to start (target nil → AppKit greys it).
        let checkForUpdates = NSMenuItem(
            title: "Check for Updates…",
            action: #selector(SPUStandardUpdaterController.checkForUpdates(_:)),
            keyEquivalent: ""
        )
        checkForUpdates.target = updaterController
        appMenu.addItem(checkForUpdates)
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
        let paletteItem = NSMenuItem(
            title: "Command Palette…",
            action: #selector(showCommandPalette(_:)),
            keyEquivalent: ""
        )
        paletteItem.target = self
        bind(paletteItem, to: KeybindAction.commandPalette)
        viewMenu.addItem(paletteItem)
        let launcherItem = NSMenuItem(
            title: "Command Launcher…",
            action: #selector(showCommandLauncher(_:)),
            keyEquivalent: ""
        )
        launcherItem.target = self
        bind(launcherItem, to: KeybindAction.commandLauncher)
        viewMenu.addItem(launcherItem)
        let customPaletteItem = NSMenuItem(
            title: "Custom Commands…",
            action: #selector(showCustomPalette(_:)),
            keyEquivalent: ""
        )
        customPaletteItem.target = self
        bind(customPaletteItem, to: KeybindAction.customPalette)
        viewMenu.addItem(customPaletteItem)
        viewMenu.addItem(.separator())
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
            Self.uiDefaults.set(true, forKey: Self.sidebarVisibleDefaultsKey)
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
            Self.uiDefaults.set(false, forKey: Self.sidebarVisibleDefaultsKey)
        }
    }

    /// Force the sidebar visible without toggling. Called from the
    /// user-action paths where Go (`cmd/roost/app.go:1337,1487,1975`)
    /// auto-expands the sidebar so the user sees the affected row:
    /// `newProject` (freshly-created project), `selectProjectFromMenu`
    /// (⌘1-9 reveals the focused project), `renameActiveProject`
    /// (rename popover needs the row to anchor against), plus the
    /// notification jumps (banner click, palette confirm, ⌘⇧U). The
    /// underlying `selectProject` / `focusTab` are pure data mutators
    /// per DL-11; reveal is a per-call-site concern.
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
        guard let target = nextUnreadTab(), let tabID = target.id else { return }
        // Route through the core (like the inbox jump) so `workspace.active()`
        // — hence identify / tab.focus / the restored selection — tracks the
        // jump, not just UI selection. The `.active` event drives the project
        // switch + tab select; clear the tab's notification.
        //
        // User action (⌘⇧U): reveal the sidebar so the jump destination is
        // visible. `focusTab` is pure data mutation per DL-11.
        ensureSidebarVisible()
        _ = try? RoostBackend.shared.localClient?.focusTab(tabID)
        let socket = socketPath
        Task.detached { await clearTabNotification(socketPath: socket, tabID: tabID) }
    }

    /// The next tab with a pending notification: the active project first
    /// (from after the focused tab, wrapping), then other projects in
    /// order. `nil` when nothing is pending.
    @MainActor
    private func nextUnreadTab() -> TabSession? {
        if let activeID = activeProjectID {
            let activeTabs = tabs.filter { $0.projectID == activeID }
            let activeFocus = activeSessionByProject[activeID]
            let startIdx =
                activeFocus
                .flatMap { f in activeTabs.firstIndex(where: { $0 === f }) }
                .map { $0 + 1 } ?? 0
            for offset in 0..<activeTabs.count {
                let idx = (startIdx + offset) % activeTabs.count
                if activeTabs[idx].liveHasNotification {
                    return activeTabs[idx]
                }
            }
        }
        for project in projects where project.id != activeProjectID {
            if let first = tabs
                .filter({ $0.projectID == project.id })
                .first(where: { $0.liveHasNotification })
            {
                return first
            }
        }
        return nil
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
        // No-op when the live size already matches the baseline.
        // Skipping `applyFont` also skips its config write — otherwise
        // a stray Cmd+0 on an unconfigured user would materialize
        // `font-size = <default>` into a config that never had a
        // font-size line.
        if abs(activeFont.pointSize - defaultSize) < 0.01 { return }
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
        // Use the *live* font family (post-`Select Font…` selection),
        // not the boot-time config snapshot — otherwise font-size
        // zooming would silently revert the family selection.
        let newFont = resolveFont(family: activeFontFamily, size: size)
        activeFont = newFont
        for session in tabs {
            session.terminalView.updateFont(newFont)
        }
        // Font-size changes are commit-only (no preview / revert
        // distinction like theme + font-family have), so persist
        // unconditionally here. The atomic tmp+rename keeps repeated
        // Cmd+= presses cheap.
        if let err = writeBackFontSize(size) {
            NSLog("roost-mac: failed to persist font-size to config.conf: %@", "\(err)")
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

/// Holds the continuation for a `palette.present` so the confirm and
/// dismiss closures can race to resume it exactly once. `@unchecked
/// Sendable` (NSLock-guarded) so it crosses the (main-actor) closure
/// boundaries the checker can't prove.
private final class PresentResumeBox: @unchecked Sendable {
    private let lock = NSLock()
    private var cont: CheckedContinuation<String?, Never>?
    init(_ c: CheckedContinuation<String?, Never>) { cont = c }
    func resume(_ value: String?) {
        lock.lock()
        let c = cont
        cont = nil
        lock.unlock()
        c?.resume(returning: value)
    }
}

/// `@unchecked Sendable` box so the timeout watchdog (on a background
/// queue) can terminate the `Process` (which isn't `Sendable`).
private final class ProcBox: @unchecked Sendable {
    let p: Process
    init(_ p: Process) { self.p = p }
}

/// Drains one pipe to EOF on a background queue, so stdout and stderr can
/// be read concurrently (a child that fills one pipe while we block on the
/// other can't deadlock). `@unchecked Sendable` (NSLock-guarded) so it
/// crosses the dispatch closure boundary.
private final class PipeDrain: @unchecked Sendable {
    private let fh: FileHandle
    private let lock = NSLock()
    private var data = Data()
    init(_ fh: FileHandle) { self.fh = fh }
    func drain() {
        let d = fh.readDataToEndOfFile()
        lock.lock()
        data = d
        lock.unlock()
    }
    func result() -> Data {
        lock.lock()
        defer { lock.unlock() }
        return data
    }
}

/// Tiny thread-safe flag for "the watchdog fired".
private final class TimeoutFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    func set() {
        lock.lock()
        value = true
        lock.unlock()
    }
    func get() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

/// Run a provider phase as a subprocess (blocking — call off the main
/// actor). Writes the context JSON to stdin, reads stdout/stderr, and
/// enforces `timeoutSecs` via a watchdog that terminates the child.
/// Mirrors `run_provider_subprocess` on the GTK side.
private func runProviderProcess(
    argv: [String], env: [(String, String)], stdin: String, cwd: String, timeoutSecs: UInt64
) -> Result<ProviderOutput, ProviderError> {
    let proc = Process()
    proc.executableURL = URL(fileURLWithPath: argv[0])
    proc.arguments = Array(argv.dropFirst())
    var environment = ProcessInfo.processInfo.environment
    for (k, v) in env { environment[k] = v }
    proc.environment = environment
    // Only set the cwd if it still exists — the active tab's dir may have
    // been removed; don't let that fail the whole spawn.
    var cwdIsDir: ObjCBool = false
    if !cwd.isEmpty, FileManager.default.fileExists(atPath: cwd, isDirectory: &cwdIsDir),
        cwdIsDir.boolValue
    {
        proc.currentDirectoryURL = URL(fileURLWithPath: cwd)
    }

    let inPipe = Pipe()
    let outPipe = Pipe()
    let errPipe = Pipe()
    proc.standardInput = inPipe
    proc.standardOutput = outPipe
    proc.standardError = errPipe

    do {
        try proc.run()
    } catch {
        return .failure(ProviderError(message: "spawn provider: \(error.localizedDescription)"))
    }

    if let data = stdin.data(using: .utf8), !data.isEmpty {
        inPipe.fileHandleForWriting.write(data)
    }
    try? inPipe.fileHandleForWriting.close()

    let box = ProcBox(proc)
    let timedOut = TimeoutFlag()
    let watchdog = DispatchWorkItem {
        timedOut.set()
        box.p.terminate()  // SIGTERM
        // Escalate to SIGKILL shortly after, so a child that ignores
        // SIGTERM (or a descendant holding the pipes open) can't keep the
        // blocking reader stuck past the timeout — its death closes the
        // pipes, unblocking `readDataToEndOfFile`.
        let pid = box.p.processIdentifier
        DispatchQueue.global(qos: .userInitiated).asyncAfter(deadline: .now() + .milliseconds(500)) {
            if box.p.isRunning { kill(pid, SIGKILL) }
        }
    }
    DispatchQueue.global(qos: .userInitiated)
        .asyncAfter(deadline: .now() + .seconds(Int(timeoutSecs)), execute: watchdog)

    // Drain stdout and stderr concurrently so a child that fills one pipe
    // while we're blocked on the other can't deadlock.
    let outDrain = PipeDrain(outPipe.fileHandleForReading)
    let errDrain = PipeDrain(errPipe.fileHandleForReading)
    let group = DispatchGroup()
    let readQ = DispatchQueue.global(qos: .userInitiated)
    group.enter()
    readQ.async {
        outDrain.drain()
        group.leave()
    }
    group.enter()
    readQ.async {
        errDrain.drain()
        group.leave()
    }
    group.wait()
    proc.waitUntilExit()
    watchdog.cancel()
    let outData = outDrain.result()
    let errData = errDrain.result()

    if timedOut.get() {
        return .failure(ProviderError(message: "provider timed out after \(timeoutSecs)s"))
    }
    if proc.terminationStatus != 0 {
        let stderr = String(decoding: errData, as: UTF8.self)
        let tail = stderr.split(separator: "\n").last.map(String.init)?
            .trimmingCharacters(in: .whitespaces) ?? ""
        let code = proc.terminationStatus
        return .failure(ProviderError(message: tail.isEmpty ? "provider exited with status \(code)" : "provider exited \(code): \(tail)"))
    }
    let stdout = String(decoding: outData, as: UTF8.self)
    do {
        return .success(try parseProviderOutput(stdout))
    } catch {
        return .failure(ProviderError(message: "provider output: \(error.localizedDescription)"))
    }
}

extension RoostApp: UiBridge {
    /// Expose the (private) window to the IPC handler via the bridge.
    /// `dumpTab(tabID:)` (defined above) satisfies the rest of `UiBridge`.
    var mainWindow: NSWindow? { window }

    /// Sidebar pane width + collapsed state for `app.window_metrics`.
    /// Collapse is keyed off "frame width is effectively zero" — the
    /// same rule the toggle/divider-hide paths already use.
    func sidebarMetrics() -> (width: CGFloat, collapsed: Bool) {
        let w = sidebarPane?.frame.width ?? 0
        return (width: w, collapsed: w < 1)
    }

    // MARK: command palette — IPC drive surface (palette.* ops)
    //
    // The IPC handler reaches the live `PalettePanel` through these (same
    // file, so the private `showCommandPalette` / `palette` are in
    // scope). Each holds a local strong ref to the panel before driving
    // it, since a confirm's dismiss sets `self.palette = nil` mid-call.

    /// `palette.open`: present a root frame (kind pre-validated by the
    /// IPC layer: "" / "commands" → command palette, "launcher" → the
    /// custom-command launcher), then read back its state.
    func openPalette(kind: String) -> PaletteSnapshot {
        switch kind {
        case "launcher": showCommandLauncher(nil)
        case "custom": showCustomPalette(nil)
        default: showCommandPalette(nil)
        }
        return paletteSnapshot()
    }

    /// `palette.present`: open the palette on a caller-supplied list and
    /// resume with the chosen row id (or `nil` on dismissal). Blocking —
    /// the continuation is resumed from the confirm/dismiss closures.
    func presentPalette(title: String, placeholder: String, items: [PaletteSnapshot.Item]) async -> String? {
        await withCheckedContinuation { (cont: CheckedContinuation<String?, Never>) in
            // Close any open palette first, then present fresh.
            palette?.driveDismiss()
            guard let window else {
                cont.resume(returning: nil)
                return
            }
            let ph = !placeholder.isEmpty ? placeholder : (!title.isEmpty ? title : "Select…")
            let paletteItems = items.map { PaletteItem(id: $0.id, title: $0.title, subtitle: $0.subtitle) }
            let root = PaletteFrame(id: "present", placeholder: ph, items: paletteItems)
            // Shared between confirm + dismiss; whoever fires first wins.
            let resume = PresentResumeBox(cont)
            let behavior = PaletteBehavior(onConfirm: { item in
                resume.resume(item.id)
                return .close
            })
            let panel = PalettePanel(
                parent: window, contentRegion: terminalContainer, root: root, behavior: behavior
            ) { [weak self] in
                self?.dismissPalette()
                resume.resume(nil)
            }
            palette = panel
            paletteOpen = true
            panel.present()
        }
    }

    /// `palette.state`: snapshot the live palette, or the closed state.
    func paletteState() -> PaletteSnapshot {
        paletteSnapshot()
    }

    /// `palette.query`: set the filter on the open palette (no-op when
    /// closed), then read back.
    func paletteQuery(_ text: String) -> PaletteSnapshot {
        palette?.driveQuery(text)
        return paletteSnapshot()
    }

    /// `palette.activate`: confirm the visible row with `id` (the same
    /// dispatch as its keybind). `nil` (→ `not-found`) when no palette is
    /// open or no row matches.
    func paletteActivate(id: String) -> PaletteSnapshot? {
        guard let panel = palette else { return nil }
        guard panel.driveActivate(id: id) else { return nil }
        return paletteSnapshot()
    }

    /// `palette.dismiss`: close any open palette (no-op when closed),
    /// then read back the closed state.
    func dismissPaletteOverlay() -> PaletteSnapshot {
        palette?.driveDismiss()
        return paletteSnapshot()
    }

    // MARK: selection — IPC drive surface (selection.* ops)
    //
    // Resolve the tab id to its live `TerminalView`, then delegate.
    // Returning `false` / `nil` from the outer optional signals
    // "no live tab" so the IPC handler maps to `not-found`. Coords
    // pass through to the view as viewport (col, row).

    func setTabSelection(
        tabID: Int64,
        anchorCol: Int,
        anchorRow: Int,
        cursorCol: Int,
        cursorRow: Int
    ) -> Bool {
        guard let session = tabs.first(where: { $0.id == tabID }) else { return false }
        session.terminalView.setSelection(
            anchorCol: anchorCol,
            anchorRow: anchorRow,
            cursorCol: cursorCol,
            cursorRow: cursorRow
        )
        return true
    }

    func clearTabSelection(tabID: Int64) -> Bool {
        guard let session = tabs.first(where: { $0.id == tabID }) else { return false }
        session.terminalView.clearSelection()
        return true
    }

    func dumpTabSelection(tabID: Int64) -> TerminalView.SelectionDump?? {
        guard let session = tabs.first(where: { $0.id == tabID }) else {
            return Optional<TerminalView.SelectionDump?>.none
        }
        return .some(session.terminalView.dumpSelection())
    }

    // MARK: test ops — IPC drive surface (tab.feed_pty_bytes /
    // tab.dump_resolved). Capture-input is read straight off
    // RoostBackend.shared and never reaches this bridge.

    func feedTabPtyBytes(tabID: Int64, data: Data) -> Bool {
        guard let session = tabs.first(where: { $0.id == tabID }) else { return false }
        // Direct call to the same `appendBytes` the real PTY
        // output drain calls (`TabSession.outputDrainTask`,
        // TabSession.swift). Indistinguishable from real PTY
        // output to the OSC scanner + libghostty — no shadow
        // drain (vision DL-5: "No test-only backdoors that drift
        // from reality").
        session.terminalView.appendBytes(data)
        return true
    }

    func dumpResolvedCells(tabID: Int64) -> TerminalView.ResolvedDump? {
        guard let session = tabs.first(where: { $0.id == tabID }) else { return nil }
        return session.terminalView.dumpResolvedCells()
    }

    func expandTabSelectionAt(
        tabID: Int64,
        col: Int,
        row: Int,
        clickCount: Int
    ) -> ExpandSelectionOutcome? {
        guard let session = tabs.first(where: { $0.id == tabID }) else { return nil }
        // Same dispatch the real mouseDown runs. handleClickCount
        // both commits the selection and writes the pasteboard;
        // dumpSelection then reads the extracted text back.
        guard
            session.terminalView.handleClickCount(
                col: col,
                row: row,
                clickCount: clickCount
            )
        else { return nil }
        // dumpSelection returns text only; recompute the (col0, col1)
        // bounds from the same row text the production dispatch
        // walked. Cheap + deterministic — and avoids plumbing a new
        // return type all the way through the WordSelection helper.
        let rowText = session.terminalView.viewportRowTextForTest(row: row)
        let span: WordSpan?
        if clickCount == 2 {
            span = WordSelection.expandWord(
                in: rowText,
                at: col,
                extraWordChars: session.terminalView.wordBreakChars
            )
        } else {
            span = WordSelection.expandLine(in: rowText)
        }
        guard let s = span else { return nil }
        let text = session.terminalView.dumpSelection()?.text
        return ExpandSelectionOutcome(
            col0: UInt16(clamping: s.col0),
            col1: UInt16(clamping: s.col1),
            text: text
        )
    }

    // MARK: test ops — mouse/focus/cursor (PR A: mouse-tracking +
    // OSC 22 wiring). Each routes through the same TerminalView path
    // a real NSEvent would, so the e2e suite pins the production
    // mouse-encoder + focus-tracking + OSC 22 emit chain.

    func dispatchTabMouseEvent(
        tabID: Int64,
        kind: MouseRoutingAction,
        button: MouseRoutingButton?,
        cellCol: UInt32,
        cellRow: UInt32,
        mods: UInt32
    ) -> Bool {
        guard let session = tabs.first(where: { $0.id == tabID }) else { return false }
        let view = session.terminalView
        // Compute the surface-space pixel point at the cell's
        // upper-left so the encoder's pixel→cell math lands on the
        // exact requested cell. Mirrors the wheel-handler clamp:
        // a cell on the right/bottom edge maps to its lower bound.
        let cellW = view.cellSize.width
        let cellH = view.cellSize.height
        let px = NSPoint(
            x: (CGFloat(cellCol) + 0.5) * cellW,
            y: (CGFloat(cellRow) + 0.5) * cellH
        )
        let ghMods = GhosttyMods(UInt16(truncatingIfNeeded: mods))
        view.emitMouseTracking(
            action: kind,
            button: button,
            mods: ghMods,
            point: px
        )
        return true
    }

    func setSimulatedFocus(focused: Bool) -> Bool {
        // Apply to the active tab only — the e2e harness assumes
        // there's one focused tab at a time. Returns `false` when
        // there's no tab (relaunched cold). `requireFirstResponder:
        // false` because the bridge already targeted the active
        // session; the real OS focus may be elsewhere (the e2e
        // suite runs without taking the window key for itself).
        guard let pid = activeProjectID,
              let session = activeSessionByProject[pid]
        else { return false }
        session.terminalView.emitFocusEvent(
            focused: focused,
            requireFirstResponder: false
        )
        return true
    }

    func currentCursorShape() -> String {
        guard let pid = activeProjectID,
              let session = activeSessionByProject[pid]
        else { return "default" }
        return session.terminalView.currentCursorShapeName()
    }

    /// Map the live `PalettePanel` (if any) to a `PaletteSnapshot`.
    private func paletteSnapshot() -> PaletteSnapshot {
        guard let panel = palette else { return .closed }
        let s = panel.driveSnapshot()
        return PaletteSnapshot(
            open: true,
            frame: s.frame,
            query: s.query,
            selection: s.selection,
            items: s.items.map {
                PaletteSnapshot.Item(id: $0.id, title: $0.title, subtitle: $0.subtitle)
            }
        )
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

/// Result of the sidebar/content resize allocation. Pure value type
/// so the allocation math is unit-testable without a live NSSplitView.
struct SidebarResizeAllocation: Equatable {
    /// `nil` means "let NSSplitView's default `adjustSubviews()`
    /// handle it" — typically the user-drag and collapsed paths.
    let sidebar: NSRect?
    let content: NSRect?

    static let useDefault = SidebarResizeAllocation(sidebar: nil, content: nil)
}

/// Threshold under which a `splitView` width delta is treated as
/// rounding noise rather than a real window resize. AppKit can call
/// `resizeSubviewsWithOldSize` with a sub-pixel float diff on what
/// is effectively the same width (e.g. when tab-bar height changes
/// re-trigger a layout pass); 0.5pt is well below any real interactive
/// drag delta.
let sidebarResizeWidthTolerance: CGFloat = 0.5

/// Compute the resize allocation for `RoostApp`'s split view.
/// Returns `.useDefault` when NSSplitView's own `adjustSubviews()`
/// should run (user drag, no width change, or sidebar collapsed);
/// otherwise returns explicit frames for sidebar + content where
/// the sidebar holds its current width (clamped to the configured
/// band) and the content absorbs the entire window-resize delta.
///
/// This is the lever that fixes the sidebar-grows-on-resize bug.
/// PR #159 tried to fix it by mutating a constraint on every
/// `splitViewDidResizeSubviews` callback, which created a runaway
/// loop. The correct fix is to take ownership of the redistribution
/// here, where NSSplitView gives us a callback specifically for it.
func computeSidebarResizeAllocation(
    splitViewSize newSize: NSSize,
    oldSize: NSSize,
    currentSidebarFrame: NSRect,
    dividerThickness: CGFloat,
    minWidth: CGFloat,
    maxWidth: CGFloat
) -> SidebarResizeAllocation {
    let dx = newSize.width - oldSize.width
    let isWindowResize = abs(dx) > sidebarResizeWidthTolerance
    if !isWindowResize { return .useDefault }
    // ⌘B collapse path: sidebar is at width 0. Let `adjustSubviews`
    // honor the collapsed state; we don't want to hand the collapsed
    // sidebar any width back.
    if currentSidebarFrame.width < 1 { return .useDefault }
    let sidebarW = max(minWidth, min(maxWidth, currentSidebarFrame.width))
    let contentW = max(0, newSize.width - sidebarW - dividerThickness)
    let height = newSize.height
    return SidebarResizeAllocation(
        sidebar: NSRect(x: 0, y: 0, width: sidebarW, height: height),
        content: NSRect(
            x: sidebarW + dividerThickness, y: 0,
            width: contentW, height: height
        )
    )
}

extension RoostApp: NSSplitViewDelegate {
    /// Own the redistribution when the *split view itself* changes
    /// size (window resize). Sidebar holds its current width; content
    /// absorbs the entire delta. This bypasses NSSplitView's default
    /// holding-priority dance — which, paired with our `.defaultHigh`
    /// preferred-width constraint at the same priority, was non-
    /// deterministic and grew the sidebar proportionally with the
    /// window (the bug PR #159 misdiagnosed).
    ///
    /// User divider drag also routes through here (NSSplitView calls
    /// this on every layout pass), but `oldSize.width ==
    /// newWidth` then, so we DON'T treat it as a window resize:
    /// `adjustSubviews()` does its normal thing and the user's drag
    /// goes through. Detection happens in
    /// `computeSidebarResizeAllocation`.
    func splitView(
        _ splitView: NSSplitView,
        resizeSubviewsWithOldSize oldSize: NSSize
    ) {
        guard splitView.arrangedSubviews.count == 2,
              let sidebar = sidebarPane else {
            splitView.adjustSubviews()
            return
        }
        let alloc = computeSidebarResizeAllocation(
            splitViewSize: splitView.bounds.size,
            oldSize: oldSize,
            currentSidebarFrame: sidebar.frame,
            dividerThickness: splitView.dividerThickness,
            minWidth: Self.sidebarMinWidth,
            maxWidth: Self.sidebarMaxWidth
        )
        guard let sidebarFrame = alloc.sidebar,
              let contentFrame = alloc.content else {
            splitView.adjustSubviews()
            return
        }
        sidebar.frame = sidebarFrame
        splitView.arrangedSubviews[1].frame = contentFrame
    }

    /// Persist the sidebar pane's width on every layout change.
    /// `splitViewDidResizeSubviews` fires for both user drag and
    /// window resize; the band check below skips the ⌘B-collapsed
    /// state (frame.width = 0). Does NOT mutate
    /// `sidebarPreferredWidthConstraint` — that runaway-loop pattern
    /// was what PR #159 introduced. The constraint stays pinned at
    /// its launch-time value; user drag is preserved via NSSplitView's
    /// internal "user-set" position, and `resizeSubviewsWithOldSize`
    /// above is what holds the sidebar across window resize.
    func splitViewDidResizeSubviews(_ notification: Notification) {
        guard sidebarPersistenceActive else { return }
        guard let sidebar = sidebarPane else { return }
        let w = sidebar.frame.width
        guard w >= Self.sidebarMinWidth, w <= Self.sidebarMaxWidth else { return }
        Self.uiDefaults.set(Double(w), forKey: Self.sidebarWidthDefaultsKey)
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
