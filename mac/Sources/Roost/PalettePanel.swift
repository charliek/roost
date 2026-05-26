// Command palette — the AppKit overlay (Cmd+Shift+P).
//
// The pure model lives in Palette.swift; this is the @MainActor NSPanel
// that renders it: a centered dark card (text field + table) over a
// dimmed backdrop. The panel is added as a CHILD window of the main
// window so it travels on move/minimize, and tracks the parent's size
// via a resize observer. Dismissing (Esc at root, backdrop click, or
// losing key — e.g. Cmd+Tab) reverts any live preview and closes.
//
// We use a plain NSTextField, not NSSearchField: NSSearchField's cell
// swallows Escape to clear its text, which would steal our Esc-to-pop.

import AppKit

/// Readable medium-gray for secondary text (placeholder + unselected
/// shortcut hints), à la Zed. Light enough to read on the card; dim
/// enough to stay secondary to the white row titles.
private let paletteMutedColor = NSColor(white: 0.62, alpha: 1.0)

/// What confirming an item does. Built by the caller (RoostApp) as part
/// of each frame's behavior; kept out of the pure `PaletteState`.
enum PaletteOutcome {
    case close                                // action ran; dismiss the palette
    case push(PaletteFrame, PaletteBehavior)  // drill into a sub-list
    case none                                 // ignore (e.g. nothing selected)
}

/// Side effects for one frame, looked up by frame id.
struct PaletteBehavior {
    /// Live preview as the highlight moves (theme list applies a theme).
    var onHighlight: ((PaletteItem) -> Void)?
    var onConfirm: (PaletteItem) -> PaletteOutcome
    /// Fired exactly once when the frame is left without confirming
    /// (pop, or any dismissal) — the theme list reverts here.
    var onCancel: (() -> Void)?

    init(
        onHighlight: ((PaletteItem) -> Void)? = nil,
        onConfirm: @escaping (PaletteItem) -> PaletteOutcome,
        onCancel: (() -> Void)? = nil
    ) {
        self.onHighlight = onHighlight
        self.onConfirm = onConfirm
        self.onCancel = onCancel
    }
}

@MainActor
final class PalettePanel: NSPanel, NSWindowDelegate, NSTextFieldDelegate, NSTableViewDataSource, NSTableViewDelegate {
    private var state: PaletteState
    private var behaviors: [String: PaletteBehavior]
    private let onDismiss: () -> Void

    private weak var hostWindow: NSWindow?
    /// The terminal/content area (right of the sidebar). The card is
    /// centered over this and pinned just under the tab bar.
    private weak var contentRegion: NSView?
    private let field = NSTextField()
    private let table = NSTableView()
    private let card = NSView()
    private var cardCenterX: NSLayoutConstraint!
    private var cardTop: NSLayoutConstraint!
    private var isClosing = false

    private static let cardWidth: CGFloat = 660
    private static let cardHeight: CGFloat = 440
    private static let rowHeight: CGFloat = 32
    /// Taller row for two-line notification entries (title + body).
    private static let subtitleRowHeight: CGFloat = 48
    /// Gap below the tab bar (~1cm) so the card floats under the tabs.
    private static let topGap: CGFloat = 30

    init(
        parent: NSWindow,
        contentRegion: NSView?,
        root: PaletteFrame,
        behavior: PaletteBehavior,
        onDismiss: @escaping () -> Void
    ) {
        self.state = PaletteState(root: root)
        self.behaviors = [root.id: behavior]
        self.onDismiss = onDismiss
        self.hostWindow = parent
        self.contentRegion = contentRegion
        super.init(
            contentRect: parent.frame,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        isFloatingPanel = true
        hasShadow = false  // the card draws its own shadow; the backdrop is flat dim
        backgroundColor = .clear
        isOpaque = false
        delegate = self
        buildViews()
    }

    // Borderless panels can't become key by default; we need it so the
    // text field receives typing.
    override var canBecomeKey: Bool { true }

    // MARK: - Build

    private func buildViews() {
        let backdrop = BackdropView()
        backdrop.wantsLayer = true
        // Fully transparent: the terminal stays visible as-is behind the
        // palette (no dim). The card's shadow + border keep it distinct.
        // The view still hit-tests clicks (opacity doesn't affect that),
        // so click-outside-to-dismiss keeps working.
        backdrop.layer?.backgroundColor = NSColor.clear.cgColor
        backdrop.onClickOutsideCard = { [weak self] in self?.dismiss(confirmed: false) }
        backdrop.card = card
        contentView = backdrop

        card.translatesAutoresizingMaskIntoConstraints = false
        card.wantsLayer = true
        // Lighter than the terminal so white text reads crisply (Zed).
        card.layer?.backgroundColor = NSColor(srgbRed: 0.18, green: 0.18, blue: 0.20, alpha: 1).cgColor
        card.layer?.cornerRadius = 10
        card.layer?.borderWidth = 1
        card.layer?.borderColor = NSColor.white.withAlphaComponent(0.12).cgColor
        card.shadow = {
            let s = NSShadow()
            s.shadowColor = NSColor.black.withAlphaComponent(0.55)
            s.shadowBlurRadius = 34
            s.shadowOffset = NSSize(width: 0, height: -10)
            return s
        }()
        backdrop.addSubview(card)
        cardCenterX = card.centerXAnchor.constraint(equalTo: backdrop.centerXAnchor)
        cardTop = card.topAnchor.constraint(equalTo: backdrop.topAnchor, constant: 100)
        let cardHeight = card.heightAnchor.constraint(equalToConstant: Self.cardHeight)
        cardHeight.priority = .defaultHigh  // shrink on short windows
        NSLayoutConstraint.activate([
            cardCenterX,
            cardTop,
            card.widthAnchor.constraint(equalToConstant: Self.cardWidth),
            cardHeight,
            card.bottomAnchor.constraint(lessThanOrEqualTo: backdrop.bottomAnchor, constant: -16),
        ])

        field.translatesAutoresizingMaskIntoConstraints = false
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.font = .systemFont(ofSize: 17, weight: .regular)
        field.textColor = .white
        field.placeholderAttributedString = Self.placeholder(state.current.placeholder)
        field.delegate = self
        field.cell?.usesSingleLineMode = true
        field.cell?.wraps = false
        card.addSubview(field)

        let scroll = NSScrollView()
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        scroll.autohidesScrollers = true
        scroll.documentView = table

        table.headerView = nil
        table.backgroundColor = .clear
        table.rowHeight = Self.rowHeight
        // `.regular` (not `.none`) so selection state actually drives
        // `drawSelection` — `.none` suppressed it, which made arrow-key
        // moves invisible. PaletteRowView draws a custom neutral fill.
        table.selectionHighlightStyle = .regular
        table.intercellSpacing = NSSize(width: 0, height: 2)
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.action = #selector(rowClicked)
        let column = NSTableColumn(identifier: .init("row"))
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)

        let divider = NSBox()
        divider.translatesAutoresizingMaskIntoConstraints = false
        divider.boxType = .separator

        card.addSubview(divider)
        card.addSubview(scroll)
        NSLayoutConstraint.activate([
            field.leadingAnchor.constraint(equalTo: card.leadingAnchor, constant: 18),
            field.trailingAnchor.constraint(equalTo: card.trailingAnchor, constant: -18),
            field.topAnchor.constraint(equalTo: card.topAnchor, constant: 16),
            field.heightAnchor.constraint(equalToConstant: 26),

            divider.leadingAnchor.constraint(equalTo: card.leadingAnchor),
            divider.trailingAnchor.constraint(equalTo: card.trailingAnchor),
            divider.topAnchor.constraint(equalTo: field.bottomAnchor, constant: 12),

            scroll.leadingAnchor.constraint(equalTo: card.leadingAnchor, constant: 8),
            scroll.trailingAnchor.constraint(equalTo: card.trailingAnchor, constant: -8),
            scroll.topAnchor.constraint(equalTo: divider.bottomAnchor, constant: 6),
            scroll.bottomAnchor.constraint(equalTo: card.bottomAnchor, constant: -8),
        ])
    }

    // MARK: - Present / dismiss

    func present() {
        guard let parent = hostWindow else { return }
        setFrame(parent.frame, display: true)
        layoutCard()
        parent.addChildWindow(self, ordered: .above)
        makeKeyAndOrderFront(nil)
        makeFirstResponder(field)
        syncUI()

        let nc = NotificationCenter.default
        nc.addObserver(self, selector: #selector(parentFrameChanged), name: NSWindow.didResizeNotification, object: parent)
        nc.addObserver(self, selector: #selector(parentFrameChanged), name: NSWindow.didMoveNotification, object: parent)
    }

    @objc private func parentFrameChanged() {
        guard let parent = hostWindow else { return }
        setFrame(parent.frame, display: true)
        layoutCard()
    }

    /// Center the card over the whole window, with its top pinned just
    /// under the tab bar. The vertical anchor is derived from the
    /// content region (terminal area, whose top edge sits below the tab
    /// bar); horizontal stays window-centered. The panel is borderless
    /// with `frame == host.frame`, so the host view's window coords map
    /// straight into the backdrop's coordinate space.
    private func layoutCard() {
        guard let backdrop = contentView else { return }
        cardCenterX.constant = 0
        if let region = contentRegion, region.window != nil {
            let r = region.convert(region.bounds, to: nil)
            if r.height > 1 {
                cardTop.constant = max(backdrop.bounds.height - r.maxY + Self.topGap, 24)
                return
            }
        }
        cardTop.constant = 100
    }

    /// Tear down. When not confirmed, fire `onCancel` for every frame
    /// still on the stack (top-down) so an in-flight preview reverts.
    private func dismiss(confirmed: Bool) {
        guard !isClosing else { return }
        isClosing = true
        if !confirmed {
            for frame in state.stack.reversed() {
                behaviors[frame.id]?.onCancel?()
            }
        }
        NotificationCenter.default.removeObserver(self)
        hostWindow?.removeChildWindow(self)
        orderOut(nil)
        onDismiss()
    }

    // Losing key (Cmd+Tab away, app deactivate, programmatic re-key of
    // the parent) cancels — matches the inline-rename "focus loss ==
    // cancel" policy. `isClosing` guards the programmatic teardown.
    func windowDidResignKey(_ notification: Notification) {
        if !isClosing { dismiss(confirmed: false) }
    }

    // MARK: - Transitions

    private func confirm() {
        guard let item = state.selectedItem else { return }  // empty filter → no-op
        guard let outcome = behaviors[state.current.id]?.onConfirm(item) else { return }
        switch outcome {
        case .close:
            dismiss(confirmed: true)
        case .push(let frame, let behavior):
            behaviors[frame.id] = behavior
            state.push(frame)
            syncUI()
        case .none:
            break
        }
    }

    private func escape() {
        if let popped = state.pop() {
            behaviors[popped.id]?.onCancel?()
            behaviors[popped.id] = nil
            syncUI()
        } else {
            dismiss(confirmed: false)
        }
    }

    @objc private func rowClicked() {
        let clicked = table.clickedRow
        guard clicked >= 0, clicked < state.matches.count else { return }
        state.setSelection(clicked)
        confirm()
    }

    // MARK: - IPC drive surface (palette.* ops)
    //
    // The IPC bridge reaches the live panel through these so the same
    // navigation a user drives by keyboard/mouse is exercisable over the
    // socket. They go through the same `confirm` / `setQuery` / `dismiss`
    // a person hits, not a parallel path.

    /// Current frame id + filter + selection + visible rows (display
    /// order), for `palette.state`.
    func driveSnapshot() -> (
        frame: String, query: String, selection: Int,
        items: [(id: String, title: String, subtitle: String?)]
    ) {
        let items = state.matches.map {
            (id: $0.item.id, title: $0.item.title, subtitle: $0.item.subtitle)
        }
        return (state.current.id, state.current.query, state.current.selection, items)
    }

    /// Set the filter as if typed: re-filter, re-select the top match,
    /// fire the highlight. Also rewrites the field so the visible query
    /// matches.
    func driveQuery(_ text: String) {
        state.setQuery(text)
        field.stringValue = text
        table.reloadData()
        selectCurrentRow()
        fireHighlight()
    }

    /// Select the visible row whose item id matches, then confirm it —
    /// the same `confirm` a click/Enter runs (push a sub-frame or
    /// dispatch the command). False if no visible row has that id.
    func driveActivate(id: String) -> Bool {
        guard let index = state.matches.firstIndex(where: { $0.item.id == id }) else {
            return false
        }
        state.setSelection(index)
        confirm()
        return true
    }

    /// Dismiss (revert) — the same path Esc-at-root / backdrop click
    /// takes.
    func driveDismiss() {
        dismiss(confirmed: false)
    }

    /// Re-render the field + table for the current frame and fire the
    /// highlight preview for the selected row.
    private func syncUI() {
        field.placeholderAttributedString = Self.placeholder(state.current.placeholder)
        if field.stringValue != state.current.query {
            field.stringValue = state.current.query
        }
        table.reloadData()
        selectCurrentRow()
        fireHighlight()
    }

    private func selectCurrentRow() {
        let count = state.matches.count
        guard count > 0 else { return }
        let row = min(max(state.current.selection, 0), count - 1)
        table.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
        table.scrollRowToVisible(row)
    }

    private func fireHighlight() {
        guard let item = state.selectedItem else { return }
        behaviors[state.current.id]?.onHighlight?(item)
    }

    /// Colored placeholder (NSTextField's default placeholder color is
    /// too dim on the card).
    private static func placeholder(_ text: String) -> NSAttributedString {
        NSAttributedString(string: text, attributes: [
            .foregroundColor: paletteMutedColor,
            .font: NSFont.systemFont(ofSize: 17, weight: .regular),
        ])
    }

    // MARK: - NSTextFieldDelegate

    func controlTextDidChange(_ obj: Notification) {
        state.setQuery(field.stringValue)
        table.reloadData()
        selectCurrentRow()
        fireHighlight()
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        switch commandSelector {
        case #selector(NSResponder.insertNewline(_:)):
            confirm()
            return true
        case #selector(NSResponder.cancelOperation(_:)):
            escape()
            return true
        case #selector(NSResponder.moveUp(_:)):
            state.moveSelection(by: -1)
            selectCurrentRow()
            fireHighlight()
            return true
        case #selector(NSResponder.moveDown(_:)):
            state.moveSelection(by: 1)
            selectCurrentRow()
            fireHighlight()
            return true
        default:
            return false
        }
    }

    // MARK: - Table data / views

    func numberOfRows(in tableView: NSTableView) -> Int {
        state.matches.count
    }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        PaletteRowView()
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let matches = state.matches
        guard row < matches.count else { return nil }
        let cell = (tableView.makeView(withIdentifier: PaletteCellView.id, owner: self) as? PaletteCellView) ?? PaletteCellView()
        cell.configure(matches[row])
        return cell
    }

    func tableView(_ tableView: NSTableView, shouldSelectRow row: Int) -> Bool {
        true
    }

    /// Notification rows carry a `subtitle` (the message body) and need
    /// a second line; command/theme rows stay at the compact height.
    func tableView(_ tableView: NSTableView, heightOfRow row: Int) -> CGFloat {
        let matches = state.matches
        guard row < matches.count else { return Self.rowHeight }
        return matches[row].item.subtitle == nil ? Self.rowHeight : Self.subtitleRowHeight
    }
}

// MARK: - Backdrop

/// Dim layer behind the card. A click that lands outside the card frame
/// dismisses; clicks inside fall through to the card's controls.
private final class BackdropView: NSView {
    var onClickOutsideCard: (() -> Void)?
    weak var card: NSView?

    override func mouseDown(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        if let card, card.frame.contains(p) {
            super.mouseDown(with: event)
        } else {
            onClickOutsideCard?()
        }
    }
}

// MARK: - Row + cell

/// Neutral light-gray selection fill (Zed-style). Always drawn as
/// "emphasized" because the text field — not the table — is first
/// responder, so the default unemphasized (dim gray) look never applies.
private final class PaletteRowView: NSTableRowView {
    override var isEmphasized: Bool {
        get { true }
        set {}
    }

    override func drawSelection(in dirtyRect: NSRect) {
        guard isSelected else { return }
        NSColor.white.withAlphaComponent(0.13).setFill()
        let r = bounds.insetBy(dx: 6, dy: 1)
        NSBezierPath(roundedRect: r, xRadius: 6, yRadius: 6).fill()
    }
}

private final class PaletteCellView: NSView {
    static let id = NSUserInterfaceItemIdentifier("PaletteCell")
    private let title = NSTextField(labelWithString: "")
    private let subtitle = NSTextField(labelWithString: "")
    private let trailing = NSTextField(labelWithString: "")
    /// Title above subtitle. An NSStackView collapses to just the title
    /// when the subtitle is hidden, so single-line command/theme rows
    /// stay centered while notification rows show the two-line body.
    private let textColumn = NSStackView()

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        identifier = Self.id
        title.lineBreakMode = .byTruncatingTail
        subtitle.lineBreakMode = .byTruncatingTail
        subtitle.font = .systemFont(ofSize: 12)
        subtitle.textColor = paletteMutedColor
        trailing.translatesAutoresizingMaskIntoConstraints = false
        trailing.font = .systemFont(ofSize: 12)
        trailing.textColor = paletteMutedColor
        trailing.alignment = .right
        trailing.setContentHuggingPriority(.required, for: .horizontal)
        trailing.setContentCompressionResistancePriority(.required, for: .horizontal)

        textColumn.translatesAutoresizingMaskIntoConstraints = false
        textColumn.orientation = .vertical
        textColumn.alignment = .leading
        textColumn.spacing = 2
        textColumn.addArrangedSubview(title)
        textColumn.addArrangedSubview(subtitle)

        addSubview(textColumn)
        addSubview(trailing)
        NSLayoutConstraint.activate([
            textColumn.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 14),
            textColumn.centerYAnchor.constraint(equalTo: centerYAnchor),
            textColumn.trailingAnchor.constraint(lessThanOrEqualTo: trailing.leadingAnchor, constant: -8),
            trailing.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -14),
            trailing.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func configure(_ match: PaletteMatch) {
        title.attributedStringValue = highlighted(match.item.title, ranges: match.ranges)
        trailing.stringValue = match.item.trailingText ?? ""
        if let body = match.item.subtitle {
            subtitle.stringValue = body
            subtitle.isHidden = false
        } else {
            subtitle.stringValue = ""
            subtitle.isHidden = true
        }
    }

    /// White base text with the fuzzy-matched characters in blue
    /// (Zed-style) so the hit pops as you type.
    private func highlighted(_ text: String, ranges: [Range<Int>]) -> NSAttributedString {
        let base = NSMutableAttributedString(
            string: text,
            attributes: [
                .font: NSFont.systemFont(ofSize: 14),
                .foregroundColor: NSColor.white,
            ]
        )
        let ns = text as NSString
        for r in ranges where r.lowerBound >= 0 && r.upperBound <= ns.length {
            base.addAttributes(
                [
                    .font: NSFont.systemFont(ofSize: 14, weight: .semibold),
                    .foregroundColor: NSColor.controlAccentColor,
                ],
                range: NSRange(location: r.lowerBound, length: r.count)
            )
        }
        return base
    }
}
