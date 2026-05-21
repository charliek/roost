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

    // MARK: - Round-7 R7.B — drop indicator line

    /// Thin vertical accent bar shown at the would-land slot during
    /// an active drag. Pills don't move during the drag — the source
    /// stays visible and the floating drag snapshot AppKit provides
    /// in `beginDraggingSession` is the only "I'm being moved" cue.
    /// This replaces R6.C's live-shift placeholder; with stable pill
    /// frames, the hit-test stays consistent across pointer movement,
    /// avoiding the jitter that made R6.C effectively unusable.
    private var dropIndicator: NSView?

    /// Last-known left-edge X of the indicator in stack-local
    /// coordinates. `nil` when no indicator is shown. Tracked so
    /// `draggingUpdated` can skip frame writes when the slot hasn't
    /// crossed.
    private var dropIndicatorX: CGFloat?

    /// Indicator dimensions: 3pt wide × 24pt tall. Width matches
    /// R7.C's sidebar accent band for visual consistency across the
    /// two drop surfaces; height matches the 24pt pill so the
    /// indicator sits cell-aligned inside the 32pt strip.
    static let indicatorWidth: CGFloat = 3
    static let indicatorHeight: CGFloat = 24

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        registerForDraggedTypes([.roostTabID])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    override func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation {
        guard sender.draggingPasteboard.types?.contains(.roostTabID) == true
        else { return [] }
        let local = convert(sender.draggingLocation, from: nil)
        showIndicator(forCursorX: local.x)
        return .move
    }

    override func draggingUpdated(_ sender: any NSDraggingInfo) -> NSDragOperation {
        guard sender.draggingPasteboard.types?.contains(.roostTabID) == true
        else { return [] }
        let local = convert(sender.draggingLocation, from: nil)
        showIndicator(forCursorX: local.x)
        return .move
    }

    override func draggingExited(_ sender: (any NSDraggingInfo)?) {
        removeIndicator()
    }

    override func performDragOperation(_ sender: any NSDraggingInfo) -> Bool {
        guard let idStr = sender.draggingPasteboard.string(forType: .roostTabID),
              let id = Int64(idStr)
        else { return false }
        let local = convert(sender.draggingLocation, from: nil)
        let rawTarget = hitTestRawTargetIdx(at: local.x)
        onDropTab?(id, rawTarget)
        return true
    }

    override func concludeDragOperation(_ sender: (any NSDraggingInfo)?) {
        removeIndicator()
    }

    // MARK: - Indicator lifecycle

    /// Lazily create the indicator NSView (or move the existing one)
    /// and snap its frame to the would-land slot under the cursor.
    /// Called from both `draggingEntered` (first frame of the drag)
    /// and `draggingUpdated` (each subsequent frame). No animation —
    /// instant snap, intentional. Animations on a 3pt bar add no
    /// perceptual value and reintroduce the timing-overlap class of
    /// bugs that R6.C suffered from.
    ///
    /// Exposed at `internal` for `DragIndicatorTests` so the
    /// lifecycle can be exercised without mocking `NSDraggingInfo`.
    func showIndicator(forCursorX cursorX: CGFloat) {
        let x = dropIndicatorXForCursor(cursorX)
        if let existing = dropIndicator {
            if x == dropIndicatorX { return }
            existing.frame = indicatorRect(forX: x)
            dropIndicatorX = x
            return
        }
        let indicator = NSView()
        indicator.wantsLayer = true
        indicator.layer?.backgroundColor = NSColor.controlAccentColor.cgColor
        indicator.layer?.cornerRadius = Self.indicatorWidth / 2
        indicator.frame = indicatorRect(forX: x)
        addSubview(indicator)
        dropIndicator = indicator
        dropIndicatorX = x
    }

    /// Tear down the indicator. Called from `draggingExited` (cursor
    /// left the strip) and `concludeDragOperation` (drop committed).
    /// Idempotent so non-tab drag traffic forwarded through
    /// `TabBarScrollView` can call us safely.
    func removeIndicator() {
        dropIndicator?.removeFromSuperview()
        dropIndicator = nil
        dropIndicatorX = nil
    }

    /// Indicator frame for a given left-edge X. Vertically centers
    /// the 24pt indicator inside whatever height the strip is at
    /// (32pt today; `max(0, …)` keeps the math safe if a future
    /// layout shrinks below 24pt).
    private func indicatorRect(forX x: CGFloat) -> NSRect {
        let y = max(0, (bounds.height - Self.indicatorHeight) / 2)
        return NSRect(x: x, y: y, width: Self.indicatorWidth, height: Self.indicatorHeight)
    }

    // MARK: - Position math

    /// Compute the indicator's left-edge X in stack-local coords
    /// given the cursor X. Slot threshold mirrors
    /// `hitTestRawTargetIdx`: left half of pill[i] = slot i, right
    /// half = slot i+1.
    ///
    /// Anchoring:
    /// - Slot 0 (drop before the leading pill): flush against
    ///   pill 0's leading edge — the indicator's right edge sits
    ///   at `pill.frame.minX`.
    /// - Slot i where 0 < i < N (between two pills): centered in
    ///   the inter-pill spacing.
    /// - Slot N (past every pill, before the `+ Tab` button):
    ///   flush after the trailing pill's edge — the indicator's
    ///   left edge sits at `last.frame.maxX + indicatorWidth`.
    func dropIndicatorXForCursor(_ cursorX: CGFloat) -> CGFloat {
        let frames = arrangedSubviews.compactMap { ($0 as? TabPillView)?.frame }
        return Self.dropIndicatorX(
            forCursorX: cursorX,
            pillFrames: frames,
            indicatorWidth: Self.indicatorWidth
        )
    }

    /// Pure function form of `dropIndicatorXForCursor` used by the
    /// instance method and exposed at `internal` so the unit tests
    /// in `DragIndicatorTests` can drive the slot-threshold math
    /// without spinning up NSStackView layout. See the instance
    /// method's doc comment for the anchoring rules.
    static func dropIndicatorX(
        forCursorX cursorX: CGFloat,
        pillFrames: [CGRect],
        indicatorWidth: CGFloat
    ) -> CGFloat {
        for (i, frame) in pillFrames.enumerated() {
            if cursorX < frame.midX {
                if i == 0 { return frame.minX - indicatorWidth }
                let center = (pillFrames[i - 1].maxX + frame.minX) / 2
                return center - indicatorWidth / 2
            }
        }
        if let last = pillFrames.last { return last.maxX + indicatorWidth }
        return 0
    }

    /// True when an indicator NSView is currently installed. Used by
    /// tests to verify the lifecycle without exposing the NSView
    /// itself.
    var isDropIndicatorActive: Bool { dropIndicator != nil }

    // MARK: - Hit test

    /// Walk visible pill midpoints left-to-right and return the
    /// drop slot for the cursor. Pre-round-6 form — no placeholder
    /// or hidden-source bookkeeping because the source pill stays
    /// visible during the drag.
    private func hitTestRawTargetIdx(at cursorX: CGFloat) -> Int {
        let pills = arrangedSubviews.compactMap { $0 as? TabPillView }
        for (i, pill) in pills.enumerated() {
            if cursorX < pill.frame.midX { return i }
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
