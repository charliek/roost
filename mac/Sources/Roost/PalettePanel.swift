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
    private let field = NSTextField()
    private let table = NSTableView()
    private let card = NSView()
    private var isClosing = false

    private static let cardWidth: CGFloat = 640
    private static let cardHeight: CGFloat = 420
    private static let rowHeight: CGFloat = 34

    init(
        parent: NSWindow,
        root: PaletteFrame,
        behavior: PaletteBehavior,
        onDismiss: @escaping () -> Void
    ) {
        self.state = PaletteState(root: root)
        self.behaviors = [root.id: behavior]
        self.onDismiss = onDismiss
        self.hostWindow = parent
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
        backdrop.layer?.backgroundColor = NSColor.black.withAlphaComponent(0.35).cgColor
        backdrop.onClickOutsideCard = { [weak self] in self?.dismiss(confirmed: false) }
        backdrop.card = card
        contentView = backdrop

        card.translatesAutoresizingMaskIntoConstraints = false
        card.wantsLayer = true
        card.layer?.backgroundColor = NSColor(srgbRed: 0.14, green: 0.14, blue: 0.155, alpha: 1).cgColor
        card.layer?.cornerRadius = 10
        card.layer?.borderWidth = 1
        card.layer?.borderColor = NSColor.white.withAlphaComponent(0.08).cgColor
        card.shadow = {
            let s = NSShadow()
            s.shadowColor = NSColor.black.withAlphaComponent(0.5)
            s.shadowBlurRadius = 30
            s.shadowOffset = NSSize(width: 0, height: -10)
            return s
        }()
        backdrop.addSubview(card)
        NSLayoutConstraint.activate([
            card.centerXAnchor.constraint(equalTo: backdrop.centerXAnchor),
            // Slightly above center, Spotlight-style.
            card.centerYAnchor.constraint(equalTo: backdrop.centerYAnchor, constant: 60),
            card.widthAnchor.constraint(equalToConstant: Self.cardWidth),
            card.heightAnchor.constraint(equalToConstant: Self.cardHeight),
        ])

        field.translatesAutoresizingMaskIntoConstraints = false
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.font = .systemFont(ofSize: 18, weight: .regular)
        field.textColor = .labelColor
        field.placeholderString = state.current.placeholder
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
        table.selectionHighlightStyle = .none  // custom row highlight
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

    /// Re-render the field + table for the current frame and fire the
    /// highlight preview for the selected row.
    private func syncUI() {
        field.placeholderString = state.current.placeholder
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

/// Accent-tinted selection fill (system blue is too loud over the dark
/// card; matches the active tab pill's subtle accent wash).
private final class PaletteRowView: NSTableRowView {
    override func drawSelection(in dirtyRect: NSRect) {
        guard isSelected else { return }
        NSColor.controlAccentColor.withAlphaComponent(0.28).setFill()
        let r = bounds.insetBy(dx: 4, dy: 0)
        NSBezierPath(roundedRect: r, xRadius: 6, yRadius: 6).fill()
    }
}

private final class PaletteCellView: NSView {
    static let id = NSUserInterfaceItemIdentifier("PaletteCell")
    private let title = NSTextField(labelWithString: "")
    private let trailing = NSTextField(labelWithString: "")

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        identifier = Self.id
        title.translatesAutoresizingMaskIntoConstraints = false
        title.lineBreakMode = .byTruncatingTail
        trailing.translatesAutoresizingMaskIntoConstraints = false
        trailing.font = .systemFont(ofSize: 12)
        trailing.textColor = .secondaryLabelColor
        trailing.alignment = .right
        trailing.setContentHuggingPriority(.required, for: .horizontal)
        addSubview(title)
        addSubview(trailing)
        NSLayoutConstraint.activate([
            title.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 14),
            title.centerYAnchor.constraint(equalTo: centerYAnchor),
            trailing.leadingAnchor.constraint(greaterThanOrEqualTo: title.trailingAnchor, constant: 8),
            trailing.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -14),
            trailing.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func configure(_ match: PaletteMatch) {
        title.attributedStringValue = highlighted(match.item.title, ranges: match.ranges)
        trailing.stringValue = match.item.trailingText ?? ""
    }

    /// Bold the matched characters so the fuzzy hit is visible.
    private func highlighted(_ text: String, ranges: [Range<Int>]) -> NSAttributedString {
        let base = NSMutableAttributedString(
            string: text,
            attributes: [
                .font: NSFont.systemFont(ofSize: 14),
                .foregroundColor: NSColor.labelColor,
            ]
        )
        let ns = text as NSString
        for r in ranges where r.lowerBound >= 0 && r.upperBound <= ns.length {
            base.addAttributes(
                [
                    .font: NSFont.systemFont(ofSize: 14, weight: .bold),
                    .foregroundColor: NSColor.controlAccentColor,
                ],
                range: NSRange(location: r.lowerBound, length: r.count)
            )
        }
        return base
    }
}
