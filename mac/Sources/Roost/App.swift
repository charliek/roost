// Roost Mac client — Phase 6a: multi-tab AppKit shell.
//
// The window now hosts a stack of TabSession objects; each owns its
// own libghostty-vt-backed TerminalView and a long-running StreamPty
// gRPC session against `roost-core`. A horizontal tab bar above the
// terminal area lets the user switch between them, "+" opens a new
// tab, and the file menu wires ⌘T / ⌘W / ⌘1..⌘9 keyboard shortcuts.
//
// Project sidebar + WatchEvents subscription land in the next slice.
// All tabs in this commit live under the daemon's auto-created
// default project (the daemon does that itself when OpenTab arrives
// with project_id = 0).
//
// To run from the repo root:
//   1. Start the daemon in another terminal:
//        cargo run -p roost-core
//   2. Then:
//        cd mac && swift run Roost
// Once the window comes up the status panel shows the daemon's
// pid + version; ⌘T opens additional shells.

import AppKit
import Foundation

@main
@MainActor
final class RoostApp: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var statusLabel: NSTextField?

    /// Horizontal NSStackView holding one button per tab plus a
    /// trailing "+" button. The "+" button is allocated once and
    /// kept as the last view; tab buttons get inserted before it.
    private var tabBar: NSStackView?
    private var addTabButton: NSButton?

    /// Container view that holds whichever TabSession's terminalView
    /// is currently in front. We use addSubview / removeFromSuperview
    /// rather than isHidden so the inactive views can't accidentally
    /// pick up keystrokes via the responder chain.
    private var terminalContainer: NSView?

    /// Window menu — populated on launch with a placeholder per-tab
    /// list, rebuilt every time the tab set changes so ⌘1..⌘9 stay
    /// in sync with the visible tab order.
    private var windowMenu: NSMenu?

    /// All open tabs in window order. The first tab corresponds to
    /// ⌘1, the second to ⌘2, etc., capped at 9.
    private var tabs: [TabSession] = []
    private var activeIndex: Int?

    /// Cached after the Identify handshake. Menu actions read it to
    /// dial the daemon for new tabs / explicit CloseTab calls. Empty
    /// until launch finishes.
    private var socketPath: String = ""

    /// Set once Identify succeeds. New-tab actions are a no-op while
    /// false to avoid spawning sessions that will only fail; the
    /// status label tells the user the daemon is unreachable.
    private var daemonReachable: Bool = false

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

        installMainMenu()

        // A throwaway TerminalView gives us the cell-grid intrinsic
        // size used to fix the window's minimum content size. The
        // first real tab is created later, after Identify succeeds.
        let metricsProbe = TerminalView(cols: 80, rows: 24)
        let terminalSize = metricsProbe.intrinsicContentSize
        let headerSliceHeight: CGFloat = 112
        let tabBarHeight: CGFloat = 32
        let windowWidth = max(720, terminalSize.width + 48)
        let windowHeight = terminalSize.height + headerSliceHeight + tabBarHeight + 32

        let window = NSWindow(
            contentRect: NSRect(x: 200, y: 200, width: windowWidth, height: windowHeight),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Roost"
        window.minSize = NSSize(
            width: terminalSize.width + 48,
            height: terminalSize.height + headerSliceHeight + tabBarHeight + 32
        )

        let content = NSView(frame: window.contentRect(forFrameRect: window.frame))
        content.translatesAutoresizingMaskIntoConstraints = false
        window.contentView = content

        let socketLabel = NSTextField(labelWithString: "socket: \(socketPath)")
        socketLabel.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        socketLabel.textColor = .secondaryLabelColor
        socketLabel.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(socketLabel)

        let statusLabel = NSTextField(labelWithString: "daemon: connecting…")
        statusLabel.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        statusLabel.textColor = .secondaryLabelColor
        statusLabel.translatesAutoresizingMaskIntoConstraints = false
        statusLabel.lineBreakMode = .byWordWrapping
        statusLabel.maximumNumberOfLines = 0
        content.addSubview(statusLabel)

        let tabBar = NSStackView()
        tabBar.orientation = .horizontal
        tabBar.alignment = .centerY
        tabBar.spacing = 4
        tabBar.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(tabBar)

        let addTabButton = NSButton(title: "+", target: self, action: #selector(newTab(_:)))
        addTabButton.bezelStyle = .rounded
        addTabButton.toolTip = "New tab (⌘T)"
        tabBar.addArrangedSubview(addTabButton)

        let terminalContainer = NSView()
        terminalContainer.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(terminalContainer)

        NSLayoutConstraint.activate([
            socketLabel.topAnchor.constraint(equalTo: content.topAnchor, constant: 12),
            socketLabel.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 16),
            socketLabel.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -16),

            statusLabel.topAnchor.constraint(equalTo: socketLabel.bottomAnchor, constant: 4),
            statusLabel.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 16),
            statusLabel.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -16),

            tabBar.topAnchor.constraint(equalTo: statusLabel.bottomAnchor, constant: 12),
            tabBar.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 16),
            tabBar.trailingAnchor.constraint(lessThanOrEqualTo: content.trailingAnchor, constant: -16),
            // Tab bar height is intrinsic — tallest arranged subview
            // (a rounded NSButton, ~22pt) wins. We reserve `tabBarHeight`
            // in the window's min-size calculation as a worst-case
            // upper bound so the layout never clips the buttons.

            terminalContainer.topAnchor.constraint(equalTo: tabBar.bottomAnchor, constant: 8),
            terminalContainer.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 16),
            terminalContainer.widthAnchor.constraint(greaterThanOrEqualToConstant: terminalSize.width),
            terminalContainer.heightAnchor.constraint(greaterThanOrEqualToConstant: terminalSize.height),
            terminalContainer.bottomAnchor.constraint(lessThanOrEqualTo: content.bottomAnchor, constant: -16),
        ])

        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        self.window = window
        self.statusLabel = statusLabel
        self.tabBar = tabBar
        self.addTabButton = addTabButton
        self.terminalContainer = terminalContainer

        Task { [weak self] in
            let outcome = await runIdentify(socketPath: socketPath)
            await MainActor.run { [weak self] in
                self?.applyIdentifyOutcome(outcome)
                if case .ok = outcome {
                    self?.daemonReachable = true
                    self?.openNewTab()
                }
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func applicationWillTerminate(_ notification: Notification) {
        // Tear down every tab so each StreamPty stream closes cleanly
        // and the daemon issues a CloseTab for tabs that have an id.
        for tab in tabs {
            tab.close(socketPath: socketPath)
        }
        tabs.removeAll()
        activeIndex = nil
    }

    // MARK: - Tab management

    /// Spin up a new tab, append it to the tab bar, and switch to it.
    /// No-op if Identify hasn't completed successfully yet — opening
    /// shells against an unreachable daemon would just stack stderr
    /// noise.
    @MainActor
    private func openNewTab() {
        guard daemonReachable else { return }
        let session = TabSession(cols: 80, rows: 24)
        tabs.append(session)
        let insertedIndex = tabs.count - 1

        // Mount immediately and switch focus, even before the daemon
        // has confirmed the tab id. The terminalView renders empty
        // until the first PtyOutput chunk lands; this still feels
        // responsive vs. waiting for the round-trip.
        rebuildTabBar()
        selectTab(at: insertedIndex)

        let title = "roost-mac \(insertedIndex + 1)"
        session.start(socketPath: socketPath, title: title) { [weak self] _ in
            // ID assigned. Nothing to do here yet — the per-tab
            // button title doesn't depend on the daemon id, and the
            // window menu rebuild already happened. WatchEvents in a
            // later slice will use the id for badge updates.
            self?.rebuildWindowMenu()
        }
    }

    /// Close the currently active tab. If it was the last one and
    /// the daemon is still reachable, immediately open a fresh
    /// replacement so the window is never blank.
    @MainActor
    private func closeActiveTabImpl() {
        guard let index = activeIndex, tabs.indices.contains(index) else { return }
        let session = tabs.remove(at: index)
        session.terminalView.removeFromSuperview()
        session.close(socketPath: socketPath)

        if tabs.isEmpty {
            activeIndex = nil
            rebuildTabBar()
            if daemonReachable {
                openNewTab()
            }
            return
        }

        // Pick the next-most-natural tab to focus: the one to the
        // left of the closed slot, or the new last tab if we just
        // closed the rightmost.
        let nextIndex = min(index, tabs.count - 1)
        rebuildTabBar()
        selectTab(at: nextIndex)
    }

    @MainActor
    private func selectTab(at index: Int) {
        guard tabs.indices.contains(index) else { return }
        guard let container = terminalContainer else { return }

        // Detach the previously-visible terminalView. Constraints
        // pinned to the container are released by removeFromSuperview.
        for subview in container.subviews {
            subview.removeFromSuperview()
        }

        let session = tabs[index]
        let view = session.terminalView
        view.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(view)
        NSLayoutConstraint.activate([
            view.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            view.topAnchor.constraint(equalTo: container.topAnchor),
            view.widthAnchor.constraint(equalToConstant: view.intrinsicContentSize.width),
            view.heightAnchor.constraint(equalToConstant: view.intrinsicContentSize.height),
        ])

        activeIndex = index
        window?.makeFirstResponder(view)
        rebuildTabBar()
    }

    /// Rebuild the tab bar's button list from `tabs`. The "+" button
    /// is preserved; tab buttons are recreated each time so the
    /// active-state indicator and title indices stay in sync after
    /// inserts / removes.
    @MainActor
    private func rebuildTabBar() {
        guard let tabBar = tabBar, let addTabButton = addTabButton else { return }

        // Remove every arranged view that isn't the "+" button.
        for view in tabBar.arrangedSubviews where view !== addTabButton {
            tabBar.removeArrangedSubview(view)
            view.removeFromSuperview()
        }

        for (index, _) in tabs.enumerated() {
            let isActive = (index == activeIndex)
            let marker = isActive ? "● " : "  "
            let title = "\(marker)Tab \(index + 1)"
            let button = NSButton(title: title, target: self, action: #selector(tabButtonClicked(_:)))
            button.tag = index
            button.bezelStyle = .rounded
            button.toolTip = isActive ? "Active tab" : "Switch to Tab \(index + 1)"
            tabBar.insertArrangedSubview(button, at: tabBar.arrangedSubviews.count - 1)
        }

        rebuildWindowMenu()
    }

    @MainActor
    private func rebuildWindowMenu() {
        guard let windowMenu = windowMenu else { return }
        windowMenu.removeAllItems()

        for (index, _) in tabs.enumerated() {
            let title = "Tab \(index + 1)"
            let item = NSMenuItem(
                title: title,
                action: #selector(selectTabFromMenu(_:)),
                keyEquivalent: index < 9 ? "\(index + 1)" : ""
            )
            item.target = self
            item.tag = index
            if index == activeIndex {
                item.state = .on
            }
            windowMenu.addItem(item)
        }

        if !tabs.isEmpty {
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
    private func tabButtonClicked(_ sender: NSButton) {
        selectTab(at: sender.tag)
    }

    @objc @MainActor
    private func selectTabFromMenu(_ sender: NSMenuItem) {
        selectTab(at: sender.tag)
    }

    // MARK: - Menu installation

    @MainActor
    private func installMainMenu() {
        let mainMenu = NSMenu()

        // App menu (the bold one named after the binary). The OS
        // pulls the title from the first menu's first submenu's
        // parent; convention is to leave the title empty and let
        // AppKit fill in the process name.
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

        // File menu — tab lifecycle lives here, matching the macOS
        // convention used by Terminal.app and iTerm.
        let fileItem = NSMenuItem()
        let fileMenu = NSMenu(title: "File")
        let newTabItem = NSMenuItem(
            title: "New Tab",
            action: #selector(newTab(_:)),
            keyEquivalent: "t"
        )
        newTabItem.target = self
        fileMenu.addItem(newTabItem)
        let closeTabItem = NSMenuItem(
            title: "Close Tab",
            action: #selector(closeActiveTab(_:)),
            keyEquivalent: "w"
        )
        closeTabItem.target = self
        fileMenu.addItem(closeTabItem)
        fileItem.submenu = fileMenu
        mainMenu.addItem(fileItem)

        // Edit menu — minimal so Cocoa text-edit shortcuts (cut /
        // copy / paste) work in the about-panel and any future text
        // input fields. The TerminalView intercepts keyDown directly
        // and doesn't go through these.
        let editItem = NSMenuItem()
        let editMenu = NSMenu(title: "Edit")
        editMenu.addItem(
            withTitle: "Cut",
            action: #selector(NSText.cut(_:)),
            keyEquivalent: "x"
        )
        editMenu.addItem(
            withTitle: "Copy",
            action: #selector(NSText.copy(_:)),
            keyEquivalent: "c"
        )
        editMenu.addItem(
            withTitle: "Paste",
            action: #selector(NSText.paste(_:)),
            keyEquivalent: "v"
        )
        editMenu.addItem(
            withTitle: "Select All",
            action: #selector(NSText.selectAll(_:)),
            keyEquivalent: "a"
        )
        editItem.submenu = editMenu
        mainMenu.addItem(editItem)

        // Window menu — populated dynamically by rebuildWindowMenu()
        // every time the tab set changes. The empty submenu here is
        // a placeholder; rebuildWindowMenu fills it on first call.
        //
        // Deliberately NOT set as NSApp.windowsMenu: AppKit would then
        // auto-append per-window entries that wipe + reappear on each
        // rebuildWindowMenu() pass. We're managing this menu manually
        // for the tab list, so we skip the OS auto-management.
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
        guard let label = statusLabel else { return }
        switch outcome {
        case .ok(let id):
            label.textColor = .labelColor
            label.stringValue = """
                daemon: connected
                  pid: \(id.pid)
                  version: \(id.daemonVersion)  (proto v\(id.protocolVersion))
                  active project: \(id.activeProjectID)  active tab: \(id.activeTabID)
                """
        case .failed(let reason):
            label.textColor = .systemRed
            label.stringValue = """
                daemon: not reachable
                  reason: \(reason)
                  hint: start it with \"cargo run -p roost-core\"
                """
        }
    }

    /// Resolve the same default socket path as `roost-core`'s
    /// `default_socket_path` for macOS — always
    /// `~/Library/Caches/roost/roost.sock` when `HOME` is set;
    /// `/tmp/roost.sock` only as a last resort.
    ///
    /// We deliberately do NOT consult `XDG_RUNTIME_DIR` here even
    /// though the daemon does on Linux. The Roost Mac client is
    /// macOS-only (Package.swift gates `.macOS(.v15)`); the daemon's
    /// macOS path is unconditionally HOME-derived. A shell that
    /// happens to export `XDG_RUNTIME_DIR` (some dev setups do)
    /// would otherwise make the UI dial a different socket than the
    /// daemon created. Both sides agreeing on the macOS default
    /// matters more than mirroring the Linux ladder.
    ///
    /// The `environment` parameter defaults to the process's
    /// environment but is injectable so unit tests can pin behavior.
    /// Empty / non-absolute `HOME` falls through to `/tmp` —
    /// matching the daemon's robustness to malformed env vars in
    /// sandboxed launchd setups.
    ///
    /// Marked `nonisolated` because it's a pure function — no
    /// instance state — and `RoostApp` is `@MainActor`, which would
    /// otherwise force test callers (run on Swift Testing's task
    /// pool, not the main actor) to also be `@MainActor`.
    nonisolated static func defaultSocketPath(
        environment env: [String: String] = ProcessInfo.processInfo.environment
    ) -> String {
        if let home = env["HOME"], !home.isEmpty, home.hasPrefix("/") {
            return "\(home)/Library/Caches/roost/roost.sock"
        }
        return "/tmp/roost.sock"
    }
}
