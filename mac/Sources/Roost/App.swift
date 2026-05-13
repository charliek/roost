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
//   * the File menu gains "New Project" (⌘⇧N).
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
    private var statusLabel: NSTextField?

    /// Sidebar widgets. `sidebarStack` arranges project buttons +
    /// a trailing "+ New Project" row; `sidebarButtons` indexes them
    /// by project id so we can update active highlighting in place
    /// instead of rebuilding the whole stack on every selection
    /// change.
    private var sidebarStack: NSStackView?
    private var newProjectButton: NSButton?
    private var sidebarButtons: [Int64: NSButton] = [:]

    private var tabBar: NSStackView?
    private var addTabButton: NSButton?
    private var terminalContainer: NSView?
    private var windowMenu: NSMenu?

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

        // Cell-grid intrinsic size of an 80x24 terminal fixes the
        // window's minimum content height + the right-pane width.
        let metricsProbe = TerminalView(cols: 80, rows: 24)
        let terminalSize = metricsProbe.intrinsicContentSize
        let sidebarWidth: CGFloat = 200
        let headerSliceHeight: CGFloat = 112
        let tabBarHeight: CGFloat = 32
        let windowWidth = sidebarWidth + max(720, terminalSize.width + 48)
        let windowHeight = terminalSize.height + headerSliceHeight + tabBarHeight + 32

        let window = NSWindow(
            contentRect: NSRect(x: 200, y: 200, width: windowWidth, height: windowHeight),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Roost"
        window.minSize = NSSize(width: windowWidth, height: windowHeight)

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

        let header = NSTextField(labelWithString: "Projects")
        header.font = .systemFont(ofSize: 13, weight: .semibold)
        header.textColor = .secondaryLabelColor
        header.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(header)

        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 4
        stack.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(stack)

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
            header.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 12),
            header.trailingAnchor.constraint(equalTo: pane.trailingAnchor, constant: -12),

            stack.topAnchor.constraint(equalTo: header.bottomAnchor, constant: 8),
            stack.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 8),
            stack.trailingAnchor.constraint(equalTo: pane.trailingAnchor, constant: -8),

            addProject.topAnchor.constraint(
                greaterThanOrEqualTo: stack.bottomAnchor,
                constant: 8
            ),
            addProject.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 12),
            addProject.bottomAnchor.constraint(equalTo: pane.bottomAnchor, constant: -12),
        ])

        self.sidebarStack = stack
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

        let socketLabel = NSTextField(labelWithString: "socket: \(socketPath)")
        socketLabel.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        socketLabel.textColor = .secondaryLabelColor
        socketLabel.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(socketLabel)

        let statusLabel = NSTextField(labelWithString: "daemon: connecting…")
        statusLabel.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        statusLabel.textColor = .secondaryLabelColor
        statusLabel.translatesAutoresizingMaskIntoConstraints = false
        statusLabel.lineBreakMode = .byWordWrapping
        statusLabel.maximumNumberOfLines = 0
        pane.addSubview(statusLabel)

        let tabBar = NSStackView()
        tabBar.orientation = .horizontal
        tabBar.alignment = .centerY
        tabBar.spacing = 4
        tabBar.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(tabBar)

        let addTabButton = NSButton(title: "+", target: self, action: #selector(newTab(_:)))
        addTabButton.bezelStyle = .rounded
        addTabButton.toolTip = "New tab (⌘T)"
        tabBar.addArrangedSubview(addTabButton)

        let terminalContainer = NSView()
        terminalContainer.translatesAutoresizingMaskIntoConstraints = false
        pane.addSubview(terminalContainer)

        NSLayoutConstraint.activate([
            socketLabel.topAnchor.constraint(equalTo: pane.topAnchor, constant: 12),
            socketLabel.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 16),
            socketLabel.trailingAnchor.constraint(equalTo: pane.trailingAnchor, constant: -16),

            statusLabel.topAnchor.constraint(equalTo: socketLabel.bottomAnchor, constant: 4),
            statusLabel.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 16),
            statusLabel.trailingAnchor.constraint(equalTo: pane.trailingAnchor, constant: -16),

            tabBar.topAnchor.constraint(equalTo: statusLabel.bottomAnchor, constant: 12),
            tabBar.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 16),
            tabBar.trailingAnchor.constraint(lessThanOrEqualTo: pane.trailingAnchor, constant: -16),
            // Tab bar height stays intrinsic to its tallest button —
            // worst-case `tabBarHeight` is reserved in the window's
            // min-size calculation up in the parent layout.

            terminalContainer.topAnchor.constraint(equalTo: tabBar.bottomAnchor, constant: 8),
            terminalContainer.leadingAnchor.constraint(equalTo: pane.leadingAnchor, constant: 16),
            terminalContainer.widthAnchor.constraint(
                greaterThanOrEqualToConstant: terminalSize.width
            ),
            terminalContainer.heightAnchor.constraint(
                greaterThanOrEqualToConstant: terminalSize.height
            ),
            terminalContainer.bottomAnchor.constraint(
                lessThanOrEqualTo: pane.bottomAnchor,
                constant: -16
            ),
        ])

        _ = tabBarHeight  // referenced for window-min-size math; not constrained directly

        self.statusLabel = statusLabel
        self.tabBar = tabBar
        self.addTabButton = addTabButton
        self.terminalContainer = terminalContainer
        return pane
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func applicationWillTerminate(_ notification: Notification) {
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
                    self.selectProject(id: first.id, openTabIfEmpty: true)
                }
            }
        }
    }

    // MARK: - Project management

    @MainActor
    private func rebuildSidebar() {
        guard let stack = sidebarStack else { return }
        for view in stack.arrangedSubviews {
            stack.removeArrangedSubview(view)
            view.removeFromSuperview()
        }
        sidebarButtons.removeAll()

        for project in projects {
            let button = makeSidebarButton(for: project)
            stack.addArrangedSubview(button)
            sidebarButtons[project.id] = button
        }
        applySidebarHighlight()
        // Window menu's Project section is driven off `projects`; keep
        // it in sync so ⌘1..⌘9 always reflects the current sidebar.
        rebuildWindowMenu()
    }

    @MainActor
    private func makeSidebarButton(for project: ProjectSnapshot) -> NSButton {
        let button = NSButton(
            title: sidebarTitle(for: project, active: project.id == activeProjectID),
            target: self,
            action: #selector(sidebarProjectClicked(_:))
        )
        button.bezelStyle = .rounded
        button.tag = Int(project.id)
        button.alignment = .left
        // Right-click → Rename / Delete. Setting `menu` makes AppKit
        // route a right-click to it without needing a custom
        // NSResponder override.
        let menu = NSMenu()
        let rename = NSMenuItem(
            title: "Rename…",
            action: #selector(renameProjectFromMenu(_:)),
            keyEquivalent: ""
        )
        rename.target = self
        rename.tag = Int(project.id)
        menu.addItem(rename)
        let delete = NSMenuItem(
            title: "Delete",
            action: #selector(deleteProjectFromMenu(_:)),
            keyEquivalent: ""
        )
        delete.target = self
        delete.tag = Int(project.id)
        menu.addItem(delete)
        button.menu = menu
        return button
    }

    private func sidebarTitle(for project: ProjectSnapshot, active: Bool) -> String {
        let marker = active ? "● " : "  "
        return marker + project.name
    }

    @MainActor
    private func applySidebarHighlight() {
        for (id, button) in sidebarButtons {
            if let project = projects.first(where: { $0.id == id }) {
                button.title = sidebarTitle(for: project, active: id == activeProjectID)
            }
        }
    }

    @MainActor
    private func selectProject(id: Int64, openTabIfEmpty: Bool) {
        activeProjectID = id
        applySidebarHighlight()
        rebuildTabBar()

        let projectTabs = tabsForActiveProject()
        if projectTabs.isEmpty {
            if openTabIfEmpty && daemonReachable {
                openNewTab()
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
    private func sidebarProjectClicked(_ sender: NSButton) {
        let id = Int64(sender.tag)
        guard id != activeProjectID else { return }
        selectProject(id: id, openTabIfEmpty: false)
    }

    @objc @MainActor
    private func newProject(_ sender: Any?) {
        guard daemonReachable else { return }
        let socketPath = self.socketPath
        Task { [weak self] in
            let created = await createProject(socketPath: socketPath, name: "", cwd: "")
            await MainActor.run { [weak self] in
                guard let self, let created else { return }
                self.projects.append(created)
                self.rebuildSidebar()
                self.selectProject(id: created.id, openTabIfEmpty: true)
            }
        }
    }

    @objc @MainActor
    private func renameProjectFromMenu(_ sender: NSMenuItem) {
        let id = Int64(sender.tag)
        guard let project = projects.first(where: { $0.id == id }) else { return }

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
        let id = Int64(sender.tag)
        guard let project = projects.first(where: { $0.id == id }) else { return }

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
                        self.selectProject(id: next.id, openTabIfEmpty: true)
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
        let session = TabSession(projectID: projectID, cols: 80, rows: 24)
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
        container.addSubview(view)
        NSLayoutConstraint.activate([
            view.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            view.topAnchor.constraint(equalTo: container.topAnchor),
            view.widthAnchor.constraint(equalToConstant: view.intrinsicContentSize.width),
            view.heightAnchor.constraint(equalToConstant: view.intrinsicContentSize.height),
        ])

        activeSessionByProject[activeProjectID] = session
        window?.makeFirstResponder(view)
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
            let marker = isActive ? "● " : "  "
            let title = "\(marker)Tab \(index + 1)"
            let button = NSButton(
                title: title,
                target: self,
                action: #selector(tabButtonClicked(_:))
            )
            button.tag = index
            button.bezelStyle = .rounded
            tabBar.insertArrangedSubview(button, at: tabBar.arrangedSubviews.count - 1)
        }

        rebuildWindowMenu()
    }

    @MainActor
    private func rebuildWindowMenu() {
        guard let windowMenu = windowMenu else { return }
        windowMenu.removeAllItems()

        // Project switching first — ⌘1..⌘9, matching the Go binary's
        // `switch_project_N` defaults.
        for (index, project) in projects.enumerated() {
            let item = NSMenuItem(
                title: project.name,
                action: #selector(selectProjectFromMenu(_:)),
                keyEquivalent: index < 9 ? "\(index + 1)" : ""
            )
            item.target = self
            item.tag = Int(project.id)
            if project.id == activeProjectID {
                item.state = .on
            }
            windowMenu.addItem(item)
        }
        if !projects.isEmpty {
            windowMenu.addItem(.separator())
        }

        // Tab switching — ⌃1..⌃9, matching the Go binary's
        // `switch_tab_N` defaults (control-digit, not command-digit).
        let projectTabs = tabsForActiveProject()
        let activeSession = activeProjectID.flatMap { activeSessionByProject[$0] }
        for (index, session) in projectTabs.enumerated() {
            let item = NSMenuItem(
                title: "Tab \(index + 1)",
                action: #selector(selectTabFromMenu(_:)),
                keyEquivalent: index < 9 ? "\(index + 1)" : ""
            )
            item.keyEquivalentModifierMask = [.control]
            item.target = self
            item.tag = index
            if session === activeSession {
                item.state = .on
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
    private func tabButtonClicked(_ sender: NSButton) {
        selectTab(at: sender.tag)
    }

    @objc @MainActor
    private func selectTabFromMenu(_ sender: NSMenuItem) {
        selectTab(at: sender.tag)
    }

    @objc @MainActor
    private func selectProjectFromMenu(_ sender: NSMenuItem) {
        let id = Int64(sender.tag)
        guard id != activeProjectID else { return }
        selectProject(id: id, openTabIfEmpty: false)
    }

    /// ⌘⇧R entry point — pulls up the same rename dialog as the
    /// right-click sidebar action, targeted at whichever project is
    /// currently shown.
    @objc @MainActor
    private func renameActiveProject(_ sender: Any?) {
        guard let id = activeProjectID else { return }
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

        let fileItem = NSMenuItem()
        let fileMenu = NSMenu(title: "File")
        // ⌘N for New Project — Roost has no multi-window concept, so
        // ⌘N is free, and reaching for it is the natural first guess.
        // ⌘T then opens a new tab in the current project, mirroring
        // Terminal.app / iTerm.
        let newProjectItem = NSMenuItem(
            title: "New Project",
            action: #selector(newProject(_:)),
            keyEquivalent: "n"
        )
        newProjectItem.target = self
        fileMenu.addItem(newProjectItem)
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
        fileMenu.addItem(.separator())
        // ⌘⇧R = rename_project from the Go binary's defaults.
        let renameProjectItem = NSMenuItem(
            title: "Rename Project…",
            action: #selector(renameActiveProject(_:)),
            keyEquivalent: "r"
        )
        renameProjectItem.keyEquivalentModifierMask = [.command, .shift]
        renameProjectItem.target = self
        fileMenu.addItem(renameProjectItem)
        fileItem.submenu = fileMenu
        mainMenu.addItem(fileItem)

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
    nonisolated static func defaultSocketPath(
        environment env: [String: String] = ProcessInfo.processInfo.environment
    ) -> String {
        if let home = env["HOME"], !home.isEmpty, home.hasPrefix("/") {
            return "\(home)/Library/Caches/roost/roost.sock"
        }
        return "/tmp/roost.sock"
    }
}
