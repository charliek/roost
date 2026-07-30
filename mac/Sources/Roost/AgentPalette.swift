// Agent palette — the pure frame builder for the `agents` frame
// (plan 005 §3.2–§3.6).
//
// Mirror of `crates/roost-linux/src/agent_palette.rs`: one row per tab
// an agent owns, carrying a status dot + project + name + status text +
// elapsed time (+ git metrics, filled asynchronously). Every rendered
// value is derived here from the core workspace snapshot, so
// `PalettePanel` stays a dumb renderer and the whole mapping —
// population filter, status vocabulary, name fallback, time buckets,
// ordering — is unit-testable without a window or a live panel.
//
// The lifecycle input is `Agent.effectiveLifecycle`, the same value the
// tab pill, the sidebar rollup, and the GTK palette render, so the two
// UIs and the three surfaces can never disagree. Ordering reuses
// `Agent.rank`, the shipped definition of "most urgent".

import AppKit

enum AgentPalette {
    /// Frame id — also the `palette.open` kind and what `palette.state`
    /// reports.
    static let frameID = "agents"
    static let placeholder = "Go to agent…"
    /// The empty-state row. Deliberately not parseable as `agent:<id>`.
    static let emptyRowID = "agents:empty"
    static let emptyRowTitle = "No agent sessions"

    private static let rowIDPrefix = "agent:"
    /// `detail` is an open string from an arbitrary adapter; cap what we
    /// render so one long line can't blow out the row.
    private static let detailMaxChars = 40
    /// The repo's two non-agent internal ownership sources
    /// (`tab.set_state` claims as `manual`, the deprecated
    /// `tab.set_hook_active` as `legacy`). Any *other* source is presumed
    /// an agent, so a third-party adapter shows up without a whitelist
    /// (AD-8). Taken from `Workspace` rather than re-spelled, so renaming
    /// a source can't silently un-filter the palette.
    private static let nonAgentSources: Set<String> = [
        Workspace.manualSource, Workspace.legacySource,
    ]
    /// Key under `ownership.metadata` carrying the agent's own session
    /// name.
    static let sessionTitleKey = "session_title"

    /// Wall-clock seconds — the same scale `ownership.lastEventAt` is
    /// stamped in (server receipt time).
    static func nowUnix() -> Int64 {
        Int64(Date().timeIntervalSince1970)
    }

    /// The root/sub frame for the agents palette.
    static func agentFrame(
        projects: [Workspace.Project], tabs: [Workspace.Tab], now: Int64
    ) -> PaletteFrame {
        PaletteFrame(
            id: frameID,
            placeholder: placeholder,
            items: agentItems(projects: projects, tabs: tabs, now: now)
        )
    }

    /// Rows for every agent-owned tab in the snapshot, in `rank` order.
    /// Empty populations yield the single non-actionable sentinel row.
    ///
    /// Takes the flat tab list plus the project list (the Mac workspace
    /// keeps them apart, where the GTK snapshot nests tabs under their
    /// project); a tab whose project is missing is skipped.
    static func agentItems(
        projects: [Workspace.Project], tabs: [Workspace.Tab], now: Int64
    ) -> [PaletteItem] {
        let byID = Dictionary(projects.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        var rows: [Row] = []
        for tab in tabs {
            guard Agent.isLive(tab.agent), let owner = tab.agent.ownership else { continue }
            if nonAgentSources.contains(owner.source) { continue }
            guard let project = byID[tab.projectId] else { continue }
            rows.append(
                rowFor(
                    project: project,
                    tab: tab,
                    owner: owner,
                    effective: Agent.effectiveLifecycle(tab.agent),
                    now: now
                )
            )
        }
        if rows.isEmpty {
            return [PaletteItem(id: emptyRowID, title: emptyRowTitle, actionable: false)]
        }
        // Total order (§3.5): urgency, then recency, then the workspace's
        // own deterministic layout order. Whole-second `lastEventAt` ties
        // are common, so the positional tiebreaks are load-bearing (and
        // make the comparison total, which `sort` needs to be
        // deterministic).
        rows.sort { a, b in
            if a.rank != b.rank { return a.rank > b.rank }
            if a.lastEventAt != b.lastEventAt { return a.lastEventAt > b.lastEventAt }
            if a.projectPosition != b.projectPosition { return a.projectPosition < b.projectPosition }
            if a.tabPosition != b.tabPosition { return a.tabPosition < b.tabPosition }
            return a.tabID < b.tabID
        }
        return rows.map(\.item)
    }

    /// Working directory of every tab that gets a row, by tab id — the
    /// input to the git-metrics probe (plan 005 §3.7). Built from the
    /// same population filter as `agentItems`, so a row and its probe can
    /// never disagree about which tabs are in play. A tab with no cwd
    /// maps to `""`, which the probe resolves to `—` (never skipped:
    /// skipping would leave the row pending forever).
    static func agentTabCwds(
        projects: [Workspace.Project], tabs: [Workspace.Tab]
    ) -> [Int64: String] {
        let byID = Dictionary(projects.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        var cwds: [Int64: String] = [:]
        for tab in tabs {
            guard Agent.isLive(tab.agent), let owner = tab.agent.ownership else { continue }
            if nonAgentSources.contains(owner.source) { continue }
            guard byID[tab.projectId] != nil else { continue }
            cwds[tab.id] = tab.cwd
        }
        return cwds
    }

    /// The tab id an agent row activates, or nil for the empty sentinel
    /// (and any other row id).
    static func tabID(fromRowID rowID: String) -> Int64? {
        guard rowID.hasPrefix(rowIDPrefix) else { return nil }
        return Int64(rowID.dropFirst(rowIDPrefix.count))
    }

    /// Dot + status-text colour for a lifecycle. The four live colours
    /// come from `rollupColor` (one hex source shared with the sidebar
    /// stripe and the GTK CSS); `inactive` — which has no stripe — is
    /// that gray at 50% alpha, distinct from `finished`'s full-alpha one.
    static func statusColor(for lifecycle: AgentLifecycle) -> NSColor {
        rollupColor(for: lifecycle) ?? inactiveColor
    }

    private static let inactiveColor = NSColor(
        red: 0x7a / 255.0, green: 0x7a / 255.0, blue: 0x7a / 255.0, alpha: 0.5)

    /// The row's status column (§3.3). `effective` picks the vocabulary;
    /// `raw` gates the background-tasks exception so only a genuinely
    /// working *agent* (not a shell-derived "Working") can report it.
    static func statusText(
        effective: AgentLifecycle, raw: AgentLifecycle, detail: String
    ) -> String {
        switch effective {
        case .working:
            guard let n = backgroundTasks(raw: raw, detail: detail) else { return "Working" }
            return n == 1 ? "Working · 1 bg task" : "Working · \(n) bg tasks"
        case .waiting:
            return "Waiting for input"
        case .finished:
            return "Finished"
        case .failed:
            let shown = displayDetail(detail)
            return shown.isEmpty ? "Failed" : "Failed · \(shown)"
        case .inactive:
            return "Idle"
        }
    }

    /// Elapsed since `lastEventAt` (§3.6). Clock skew — a stamp in the
    /// future — clamps to `"0s"` rather than rendering a negative age.
    ///
    /// Only the sub-minute bucket differs from the notification inbox's
    /// label (`"Ns"` here, `"just now"` there); the m/h/d edges are
    /// shared so the two lists can't drift apart.
    static func elapsedText(now: Int64, lastEventAt: Int64) -> String {
        let (delta, overflow) = now.subtractingReportingOverflow(lastEventAt)
        // Overflow only happens for absurd stamps; saturate the way
        // Rust's `saturating_sub` does rather than trapping.
        let secs = overflow ? (lastEventAt < 0 ? Int64.max : 0) : max(0, delta)
        if secs < 60 { return "\(secs)s" }
        return relativeTimeLabel(seconds: secs)
    }

    /// The fuzzy-match input and the generic-client fallback title. One
    /// composition, used everywhere — the filter matches exactly what
    /// the row shows.
    static func composeTitle(project: String, name: String) -> String {
        if project.isEmpty { return name }
        return NotificationInbox.composeTitle(project: project, tab: name)
    }

    // MARK: - Internals

    /// A row plus its sort keys.
    private struct Row {
        let rank: Int
        let lastEventAt: Int64
        let projectPosition: Int32
        let tabPosition: Int32
        let tabID: Int64
        let item: PaletteItem
    }

    private static func rowFor(
        project: Workspace.Project,
        tab: Workspace.Tab,
        owner: Ownership,
        effective: AgentLifecycle,
        now: Int64
    ) -> Row {
        let projectName = normalizeLine(project.name)
        let name = rowName(tab: tab, owner: owner)
        let item = PaletteItem(
            id: "\(rowIDPrefix)\(tab.id)",
            title: composeTitle(project: projectName, name: name),
            agent: AgentRowData(
                effectiveLifecycle: effective,
                project: projectName,
                name: name,
                statusText: statusText(
                    effective: effective, raw: tab.agent.lifecycle, detail: owner.detail),
                timeText: elapsedText(now: now, lastEventAt: owner.lastEventAt),
                // Filled by the git-metrics probe; nil means pending.
                metricsText: nil
            )
        )
        return Row(
            rank: Agent.rank(effective),
            lastEventAt: owner.lastEventAt,
            projectPosition: project.position,
            tabPosition: tab.position,
            tabID: tab.id,
            item: item
        )
    }

    /// The agent's own session name when it published one, else the tab
    /// title, else a stable placeholder.
    private static func rowName(tab: Workspace.Tab, owner: Ownership) -> String {
        let sessionTitle = normalizeLine(owner.metadata[sessionTitleKey] ?? "")
        if !sessionTitle.isEmpty { return sessionTitle }
        let title = normalizeLine(tab.title)
        if !title.isEmpty { return title }
        return "Tab \(tab.id)"
    }

    /// Non-nil for a working agent reporting `background_tasks:N` with
    /// `N >= 1`. Anything else — a shell-derived "Working", a foreign
    /// detail an adapter left behind, a malformed / zero / negative
    /// count — renders plain "Working".
    private static func backgroundTasks(raw: AgentLifecycle, detail: String) -> Int64? {
        guard raw == .working else { return nil }
        let normalized = normalizeLine(detail)
        let prefix = "background_tasks:"
        guard normalized.hasPrefix(prefix), let n = Int64(normalized.dropFirst(prefix.count)),
            n >= 1
        else { return nil }
        return n
    }

    private static func displayDetail(_ detail: String) -> String {
        truncate(normalizeLine(detail), max: detailMaxChars)
    }

    /// Collapse an open string to one printable line. `metadata` and
    /// `detail` come from arbitrary adapters, so a newline or an escape
    /// sequence must not be able to reshape a palette row.
    static func normalizeLine(_ text: String) -> String {
        let kept = text.unicodeScalars.filter { $0.properties.generalCategory != .control }
        return String(String.UnicodeScalarView(kept))
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Tail-ellipsize to at most `max` characters *including* the
    /// ellipsis.
    static func truncate(_ text: String, max: Int) -> String {
        if text.count <= max { return text }
        return String(text.prefix(Swift.max(max - 1, 0))) + "…"
    }
}
