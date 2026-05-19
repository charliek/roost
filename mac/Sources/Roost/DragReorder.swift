// Drag-and-drop reorder plumbing for the Mac UI.
//
// M2 + M3 of `goal-mac-parity-2026-05-18.md` brings tab + sidebar
// drag-to-reorder up to parity with Linux M10 / the Go GTK binary.
// The reorder math (`ReorderMath.swift`) is shared with both axes;
// this file owns the AppKit-specific pieces: the pasteboard types,
// and the `TabBarStackView` NSStackView subclass that accepts drops
// from `TabPillView` instances.

import AppKit

extension NSPasteboard.PasteboardType {
    /// Pasteboard payload carrying a tab id as a UTF-8 stringified
    /// `Int64`. Used by `TabPillView`'s drag source + `TabBarStackView`'s
    /// drop destination. Custom UTI rather than `.string` so we don't
    /// accidentally accept arbitrary text drops onto the tab strip.
    static let roostTabID = NSPasteboard.PasteboardType("com.roost.tabid")

    /// Same shape as `roostTabID`, but for sidebar project rows. The
    /// NSOutlineView drag-source + drop-target methods on `RoostApp`
    /// emit / accept this UTI.
    static let roostProjectID = NSPasteboard.PasteboardType("com.roost.projectid")
}

/// NSStackView host for the tab strip. AppKit's stack view is fine for
/// layout but exposes no drag-destination hooks of its own; subclassing
/// lets us own `draggingEntered`/`performDragOperation` for the drop
/// without leaking the choice into `RoostApp`'s widget-tree code. The
/// outline view (sidebar) handles drops via its data-source protocol
/// instead, so M3 doesn't need a matching subclass.
@MainActor
final class TabBarStackView: NSStackView {
    /// Set by the App when constructing the strip. Fired on a successful
    /// drop with the source tab's id (decoded from the pasteboard) and
    /// the raw target index (i.e. "user dropped the source between
    /// arranged-subview slots `rawTargetIdx - 1` and `rawTargetIdx`").
    /// `ReorderMath.computeInsertIdx` maps that to a final insert index
    /// + no-op flag.
    var onDropTab: (@MainActor (Int64, Int) -> Void)?

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        registerForDraggedTypes([.roostTabID])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    override func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation {
        sender.draggingPasteboard.types?.contains(.roostTabID) == true ? .move : []
    }

    override func draggingUpdated(_ sender: any NSDraggingInfo) -> NSDragOperation {
        sender.draggingPasteboard.types?.contains(.roostTabID) == true ? .move : []
    }

    override func performDragOperation(_ sender: any NSDraggingInfo) -> Bool {
        guard let idStr = sender.draggingPasteboard.string(forType: .roostTabID),
              let id = Int64(idStr)
        else { return false }
        let local = convert(sender.draggingLocation, from: nil)
        let rawTarget = hitTestRawTargetIdx(at: local)
        onDropTab?(id, rawTarget)
        return true
    }

    /// Walk the arranged-subview pills (left-to-right) and return the
    /// index of the slot the user dropped into. Mid-pill is the
    /// threshold — drops in the left half of pill `i` land at raw
    /// target `i`; drops in the right half land at `i + 1`. Tail
    /// drops past every pill return `pillCount`. The trailing `+`
    /// button is *not* a pill; it's filtered out by type.
    private func hitTestRawTargetIdx(at point: NSPoint) -> Int {
        let pills = arrangedSubviews.compactMap { $0 as? TabPillView }
        for (i, pill) in pills.enumerated() {
            if point.x < pill.frame.midX { return i }
        }
        return pills.count
    }
}

/// Round-3 R1: AppKit's drag delivery walks
/// `NSScrollView → NSClipView → documentView`. The clip view does not
/// forward `draggingEntered` / `performDragOperation` to its
/// documentView by default, so the underlying `TabBarStackView`'s
/// drag-destination overrides never fired for drops on the
/// scrolled-content region — drag visual worked but drop never landed.
/// This thin subclass registers the same pasteboard type at the
/// scroll-view layer and forwards drag-destination events down to the
/// document view (the `TabBarStackView`) so its existing handlers run.
@MainActor
final class TabBarScrollView: NSScrollView {
    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        registerForDraggedTypes([.roostTabID])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    override func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation {
        guard sender.draggingPasteboard.types?.contains(.roostTabID) == true
        else { return [] }
        _ = (documentView as? TabBarStackView)?.draggingEntered(sender)
        return .move
    }

    override func draggingUpdated(_ sender: any NSDraggingInfo) -> NSDragOperation {
        guard sender.draggingPasteboard.types?.contains(.roostTabID) == true
        else { return [] }
        _ = (documentView as? TabBarStackView)?.draggingUpdated(sender)
        return .move
    }

    override func performDragOperation(_ sender: any NSDraggingInfo) -> Bool {
        (documentView as? TabBarStackView)?.performDragOperation(sender) ?? false
    }

    override func draggingExited(_ sender: (any NSDraggingInfo)?) {
        (documentView as? TabBarStackView)?.draggingExited(sender)
    }

    override func concludeDragOperation(_ sender: (any NSDraggingInfo)?) {
        (documentView as? TabBarStackView)?.concludeDragOperation(sender)
    }
}
