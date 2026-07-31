// Sidebar agent rows, the Mac half of the feature GTK ships in
// `crates/roost-linux/src/app.rs` (`refresh_agent_rows` /
// `build_agent_row`) plus `resources/style.css`. Rows are a lifecycle
// dot, the agent name, and a right-aligned elapsed time, with
// `status_text` as the tooltip.

import AppKit

/// One agent row as actually rendered, plus whether it is the active
/// tab. Mirrors GTK's `RenderedAgentRow`. Both UIs keep this per
/// project so a missed refresh is observable over IPC rather than
/// invisible: rendering and the dump read the same cache.
struct RenderedAgentRow: Equatable {
    let row: AgentPalette.SidebarAgentRow
    let isActive: Bool
}

/// Elapsed-time colour: the lifecycle colour for the two states that
/// want the user's attention, muted otherwise, so the time column reads
/// as chrome until something needs doing.
func sidebarAgentTimeColor(for lifecycle: AgentLifecycle) -> NSColor {
    switch lifecycle {
    case .waiting, .failed: return rollupColor(for: lifecycle) ?? AgentPalette.metricsColor(.muted)
    case .working, .finished, .inactive: return AgentPalette.metricsColor(.muted)
    }
}

/// Every project's rendered agent rows, in sidebar order, marking the
/// row whose tab is active. Projects with no agents are kept with an
/// empty list — the IPC contract reports all projects.
///
/// Pure; unit-tested in `SidebarAgentRowTests`.
func renderedSidebarAgents(
    projects: [Workspace.Project],
    tabs: [Workspace.Tab],
    activeTabID: Int64?,
    now: Int64
) -> [(projectID: Int64, agents: [RenderedAgentRow])] {
    projects.map { project in
        let rows = AgentPalette.sidebarAgents(project: project, tabs: tabs, now: now)
            .map { RenderedAgentRow(row: $0, isActive: $0.tabID == activeTabID) }
        return (projectID: project.id, agents: rows)
    }
}

/// How many child rows the sidebar outline reports for a project.
///
/// `agentCount` is already 0 when the toggle is off (the model drops the
/// items). Mid-drag the sidebar flattens to one row per project so
/// AppKit only ever proposes top-level drops — see
/// `outlineView(_:pasteboardWriterForItem:)`.
///
/// Pure; unit-tested in `SidebarAgentRowTests`.
func sidebarChildCount(agentCount: Int, isDraggingProjects: Bool) -> Int {
    isDraggingProjects ? 0 : agentCount
}

/// `NSOutlineView` with the disclosure triangle suppressed: agent rows
/// are auto-expanded and have no per-project collapse this pass, so the
/// triangle would be a control that does nothing but steal the leading
/// gutter every project row's name is aligned to.
final class SidebarOutlineView: NSOutlineView {
    override func frameOfOutlineCell(atRow row: Int) -> NSRect { .zero }
}

/// Rounded raised background for the active tab's agent row.
///
/// An explicit white wash rather than `quaternaryLabelColor`: the
/// semantic colour resolves against the *light* variant here (the
/// sidebar draws its own dark palette behind window vibrancy rather
/// than inheriting the window's dark appearance), which darkens the row
/// instead of lifting it. This is the same weight, and the same
/// direction, as the GTK rule `alpha(currentColor, 0.08)`.
private final class AgentActiveHighlightView: NSView {
    override func draw(_ dirtyRect: NSRect) {
        NSColor(white: 1.0, alpha: 0.09).setFill()
        NSBezierPath(roundedRect: bounds, xRadius: 5, yRadius: 5).fill()
    }
}

/// One agent row under a project. Indentation lives here (a leading
/// constraint) rather than on `indentationPerLevel`, so project rows
/// keep the geometry they had before agents existed.
final class AgentRowCellView: NSTableCellView {
    /// Fixed row height, shorter than a project row. Read by
    /// `outlineView(_:heightOfRowByItem:)`.
    static let rowHeight: CGFloat = 20

    private let dot = NSView()
    private let name = NSTextField(labelWithString: "")
    private let time = NSTextField(labelWithString: "")
    /// Deliberately not the blue accent pill `SidebarRowView` draws for
    /// selection: the selected project and the active agent must be
    /// legible at the same time.
    private let highlight = AgentActiveHighlightView()
    private let hit = NSButton()

    private var onActivate: (@MainActor () -> Void)?

    init() {
        super.init(frame: .zero)

        highlight.translatesAutoresizingMaskIntoConstraints = false
        highlight.isHidden = true

        dot.translatesAutoresizingMaskIntoConstraints = false
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 4

        name.translatesAutoresizingMaskIntoConstraints = false
        name.lineBreakMode = .byTruncatingTail
        name.usesSingleLineMode = true
        name.maximumNumberOfLines = 1
        name.font = .systemFont(ofSize: 11)
        name.textColor = ProjectRowCellView.inactiveLabel
        name.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)

        time.translatesAutoresizingMaskIntoConstraints = false
        time.alignment = .right
        time.usesSingleLineMode = true
        time.font = .systemFont(ofSize: 10)
        time.setContentCompressionResistancePriority(.required, for: .horizontal)
        time.setContentHuggingPriority(.required, for: .horizontal)

        // A transparent button, not a `mouseDown` override:
        // NSOutlineView runs its own mouse-tracking loop, so a cell
        // view doesn't reliably get first crack at the event, and
        // swallowing `mouseDown` would break drag initiation (drags
        // start from that same loop feeding `pasteboardWriterForItem`).
        hit.translatesAutoresizingMaskIntoConstraints = false
        hit.title = ""
        hit.isBordered = false
        hit.isTransparent = true
        hit.target = self
        hit.action = #selector(activate(_:))

        addSubview(highlight)
        addSubview(dot)
        addSubview(name)
        addSubview(time)
        addSubview(hit)
        textField = name

        NSLayoutConstraint.activate([
            highlight.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 14),
            highlight.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -6),
            highlight.topAnchor.constraint(equalTo: topAnchor),
            highlight.bottomAnchor.constraint(equalTo: bottomAnchor),

            dot.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 20),
            dot.centerYAnchor.constraint(equalTo: centerYAnchor),
            dot.widthAnchor.constraint(equalToConstant: 8),
            dot.heightAnchor.constraint(equalToConstant: 8),

            name.leadingAnchor.constraint(equalTo: dot.trailingAnchor, constant: 6),
            name.centerYAnchor.constraint(equalTo: centerYAnchor),
            name.trailingAnchor.constraint(lessThanOrEqualTo: time.leadingAnchor, constant: -6),

            time.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -12),
            time.centerYAnchor.constraint(equalTo: centerYAnchor),

            hit.leadingAnchor.constraint(equalTo: leadingAnchor),
            hit.trailingAnchor.constraint(equalTo: trailingAnchor),
            hit.topAnchor.constraint(equalTo: topAnchor),
            hit.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    func configure(with row: AgentPalette.SidebarAgentRow, isActive: Bool,
                   onActivate: @escaping @MainActor () -> Void) {
        dot.layer?.backgroundColor = AgentPalette.statusColor(for: row.lifecycle).cgColor
        name.stringValue = row.name
        time.stringValue = row.timeText
        time.textColor = sidebarAgentTimeColor(for: row.lifecycle)
        toolTip = row.statusText
        // The hit button is transparent and titleless, so VoiceOver has
        // nothing to announce without this — name plus status is what a
        // sighted user reads off the row.
        hit.setAccessibilityLabel("\(row.name), \(row.statusText)")
        self.onActivate = onActivate
        setActive(isActive)
    }

    func setActive(_ isActive: Bool) {
        highlight.isHidden = !isActive
        name.font = .systemFont(ofSize: 11, weight: isActive ? .semibold : .regular)
    }

    @objc private func activate(_ sender: Any?) {
        onActivate?()
    }
}
