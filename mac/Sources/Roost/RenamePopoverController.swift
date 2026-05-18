// Popover-over-pill tab rename UI.
//
// M5 of `goal-mac-parity-2026-05-18.md`: the Mac UI's previous tab
// rename (`renameActiveTab`) used a modal `NSAlert` — disruptive,
// blocks the entire window, doesn't match the Go binary's UX. This
// controller backs an `NSPopover` anchored to the target pill so the
// user can edit the title in place. Mirrors the Linux M9 tab popover
// pattern at `crates/roost-linux/src/app.rs::begin_rename_tab` and the
// Go binary's renameActiveTab popover.
//
// The popover is `behavior = .transient` (closes on focus-loss) and
// owns its content view controller's lifetime. Commits via Enter,
// cancels via Escape or click-away.

import AppKit
import Foundation

@MainActor
final class RenamePopoverController: NSViewController {
    private let field: NSTextField
    private let onCommit: @MainActor (String) -> Void
    private let onCancel: @MainActor () -> Void
    private var didFinish = false
    private var keyMonitor: Any?

    init(
        initial: String,
        onCommit: @escaping @MainActor (String) -> Void,
        onCancel: @escaping @MainActor () -> Void
    ) {
        let f = NSTextField(string: initial)
        f.isEditable = true
        f.isSelectable = true
        f.isBezeled = true
        f.bezelStyle = .roundedBezel
        f.usesSingleLineMode = true
        f.lineBreakMode = .byTruncatingTail
        f.translatesAutoresizingMaskIntoConstraints = false
        self.field = f
        self.onCommit = onCommit
        self.onCancel = onCancel
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    override func loadView() {
        let host = NSView()
        host.translatesAutoresizingMaskIntoConstraints = false
        host.addSubview(field)
        NSLayoutConstraint.activate([
            field.leadingAnchor.constraint(equalTo: host.leadingAnchor, constant: 8),
            field.trailingAnchor.constraint(equalTo: host.trailingAnchor, constant: -8),
            field.topAnchor.constraint(equalTo: host.topAnchor, constant: 8),
            field.bottomAnchor.constraint(equalTo: host.bottomAnchor, constant: -8),
            field.widthAnchor.constraint(greaterThanOrEqualToConstant: 240),
        ])
        view = host
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        view.window?.makeFirstResponder(field)
        if let editor = field.currentEditor() as? NSTextView {
            editor.selectAll(nil)
        }
        installKeyMonitor()
    }

    override func viewWillDisappear() {
        super.viewWillDisappear()
        removeKeyMonitor()
    }

    /// Install a local NSEvent monitor so Return / Escape are
    /// intercepted reliably. NSTextField + NSPopover's transient
    /// behavior turns out to swallow Enter via both the delegate's
    /// `doCommandBy:` callback and the `target / action` pair on some
    /// macOS revisions — the popover's window handler eats the key
    /// before the control sees it. A local key monitor sits at the
    /// app layer above the popover and always sees raw keyDown
    /// events while the popover's view is on screen, returning `nil`
    /// for handled keys so AppKit doesn't double-dispatch.
    private func installKeyMonitor() {
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            // Escape always cancels.
            if event.keyCode == 53 {  // kVK_Escape
                self.cancel()
                return nil
            }
            // Plain Return / Enter (no modifier) commits. Shift+Return
            // and friends fall through to the field's default handling
            // (no-op in single-line mode).
            if (event.keyCode == 36 || event.keyCode == 76),
               !event.modifierFlags.contains(.shift),
               !event.modifierFlags.contains(.option),
               !event.modifierFlags.contains(.command),
               !event.modifierFlags.contains(.control)
            {
                self.commit()
                return nil
            }
            return event
        }
    }

    private func removeKeyMonitor() {
        if let monitor = keyMonitor {
            NSEvent.removeMonitor(monitor)
            keyMonitor = nil
        }
    }

    private func commit() {
        guard !didFinish else { return }
        didFinish = true
        removeKeyMonitor()
        onCommit(field.stringValue)
    }

    private func cancel() {
        guard !didFinish else { return }
        didFinish = true
        removeKeyMonitor()
        onCancel()
    }
}
