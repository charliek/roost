// LayoutRestore.swift — the launch's re-open of the saved tab layout.
//
// The Swift twin of `roost_engine::application::hydrate` (plan 072 §D6).
// Every saved tab comes back as a fresh shell in its directory. The
// difference is timing: the Mac's opens are asynchronous (each tab's id
// arrives through its session's own task), and each one selects the tab
// it opens as it lands. So each project's remembered tab and the saved
// selection are put back once, after every open has settled, through
// which saved position became which tab — except where the user chose a
// tab meanwhile, whose choice stands.

import Foundation

@MainActor
final class LayoutRestore {
    /// Open one saved tab. `settled` must be called exactly once, with
    /// the id the tab opened as, or `nil` when the open failed.
    typealias Open = @MainActor (
        _ projectID: Int64,
        _ tab: Workspace.RestoreTab,
        _ settled: @escaping @MainActor (Int64?) -> Void
    ) -> Void

    private enum Outcome {
        case pending
        case opened(Int64)
        case failed
    }

    private let workspace: Workspace
    private let layout: Workspace.RestoreLayout?
    /// Per project, what each saved position became. A failed one maps
    /// to nothing, so nothing lands on the tab after it instead.
    private var outcomes: [Int64: [Outcome]] = [:]
    private var inFlight = 0
    private var done: (@MainActor (Int64?) -> Void)?
    /// Per project, the tab the user last selected there while restoring.
    private var picks: [Int64: Int64] = [:]
    private var lastPick: (projectID: Int64, tabID: Int64)?

    init(workspace: Workspace, layout: Workspace.RestoreLayout?) {
        self.workspace = workspace
        self.layout = layout
    }

    /// From `start` until the selection has been put back.
    var isRestoring: Bool { done != nil }

    /// Open each of `projects`' saved tabs — one tab at the project's
    /// cwd for a project that saved none — and, once every open has
    /// settled, make each project remember its saved tab when that
    /// position opened and is still open, re-select the saved pair (else
    /// the active project's preferred tab), and write it all once. What
    /// the user selected meanwhile wins over both (`userSelected`).
    /// `done` gets the tab the selection landed on.
    func start(
        projects: [Int64],
        open: Open,
        done: @escaping @MainActor (Int64?) -> Void
    ) {
        let plan = projects.map { ($0, savedTabs($0)) }
        for (projectID, tabs) in plan {
            outcomes[projectID] = Array(repeating: .pending, count: tabs.count)
        }
        inFlight = plan.reduce(0) { $0 + $1.1.count }
        self.done = done
        for (projectID, tabs) in plan {
            for (position, tab) in tabs.enumerated() {
                open(projectID, tab) { [weak self] tabID in
                    self?.settle(projectID, position, tabID)
                }
            }
        }
        if projects.isEmpty { finish() }
    }

    /// The user selected `tabID` while the restore is in flight. Their
    /// choice wins: finishing leaves the selection on the last tab they
    /// picked, and each project they picked in on the tab they picked
    /// there rather than its saved one. After the restore, a no-op.
    func userSelected(_ tabID: Int64) {
        guard isRestoring, let projectID = workspace.tab(tabID)?.projectId else { return }
        picks[projectID] = tabID
        lastPick = (projectID, tabID)
    }

    private func savedTabs(_ projectID: Int64) -> [Workspace.RestoreTab] {
        let saved = layout?.projects.first { $0.projectID == projectID }?.tabs ?? []
        return saved.isEmpty ? [Workspace.RestoreTab(cwd: "", title: "", userTitled: false)] : saved
    }

    private func settle(_ projectID: Int64, _ position: Int, _ tabID: Int64?) {
        guard case .pending? = outcomes[projectID]?[position] else { return }
        outcomes[projectID]?[position] = tabID.map(Outcome.opened) ?? .failed
        inFlight -= 1
        if inFlight == 0 { finish() }
    }

    /// The tab `projectID`'s saved `position` opened as, while it is
    /// still open — a restored tab can exit again (its shell quits at
    /// once) before the rest have landed, and its dead id must give way
    /// to the fallback, as Rust's `hydrate_with` does.
    private func liveTab(_ projectID: Int64, _ position: Int32) -> Int64? {
        guard let outcomes = outcomes[projectID], outcomes.indices.contains(Int(position)),
              case .opened(let tabID) = outcomes[Int(position)], isOpen(tabID)
        else { return nil }
        return tabID
    }

    private func isOpen(_ tabID: Int64) -> Bool {
        workspace.tab(tabID) != nil
    }

    private func finish() {
        guard let done else { return }
        self.done = nil
        for saved in layout?.projects ?? [] where picks[saved.projectID] == nil {
            guard let tabID = saved.lastTabPosition.flatMap({ liveTab(saved.projectID, $0) })
            else { continue }
            try? workspace.restorePreferredTab(tabID)
        }
        // Each restore open selects the tab it opens, so one that landed
        // after the user's pick in the same project has moved its memory.
        for tabID in picks.values where isOpen(tabID) {
            try? workspace.restorePreferredTab(tabID)
        }
        let active = userSelection() ?? savedSelection()
        if let active {
            do {
                _ = try workspace.focusTab(active)
            } catch {
                RoostLogger.shared.warn("restore: re-selecting tab \(active) failed: \(error)")
            }
        }
        workspace.writeThrough()
        done(active)
    }

    /// The last tab the user picked, else — it closed since — the tab
    /// its project now shows.
    private func userSelection() -> Int64? {
        guard let lastPick else { return nil }
        return isOpen(lastPick.tabID) ? lastPick.tabID : workspace.preferredTab(lastPick.projectID)
    }

    /// The saved active tab, else the active project's preferred tab.
    private func savedSelection() -> Int64? {
        layout.flatMap { liveTab($0.activeProjectID, $0.activeTabPosition) }
            ?? activeProjectFallback().flatMap { workspace.preferredTab($0) }
    }

    /// The saved active project while it still exists, else the first.
    private func activeProjectFallback() -> Int64? {
        let projects = workspace.snapshot()
        if let saved = layout?.activeProjectID, projects.contains(where: { $0.id == saved }) {
            return saved
        }
        return projects.first?.id
    }
}
