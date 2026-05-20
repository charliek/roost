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

    /// Round-6 R6.C: notify the App when an interactive drag ends so
    /// it can flush a `rebuildPending` if a sibling-client event
    /// arrived mid-drag. Fires for BOTH accepted drops (after
    /// `performDragOperation`) and cancelled drops (after
    /// `draggingExited`). Cheap idempotent callback.
    var onDragEnded: (@MainActor () -> Void)?

    // MARK: - Round-6 R6.C — live-shift drop placeholder

    /// Round-6 R6.C: transparent NSView inserted into
    /// `arrangedSubviews` at the would-land slot during an active
    /// drag. Width matches the hidden source pill so the gap the
    /// user sees is the same size as the pill that will land there.
    /// Pills around the gap slide via NSStackView's natural layout,
    /// animated through `NSAnimationContext.runAnimationGroup`.
    private var dropPlaceholder: NSView?

    /// The placeholder's last-known slot inside `arrangedSubviews`.
    /// Used by `draggingUpdated` to short-circuit when the cursor
    /// hasn't crossed a slot boundary — without this we'd remove +
    /// re-insert on every mouse-move event, which both wastes work
    /// and double-triggers the animation timing.
    private var dropPlaceholderIndex: Int?

    /// The source pill we hid at drag-start. Held weakly so a
    /// `rebuildTabBar` that swaps the pill cache out from under us
    /// (shouldn't happen with R6.C's rebuild postponement, but
    /// defensively) doesn't leave a dangling reference. Cleared on
    /// `concludeDragOperation` / `draggingExited`.
    private weak var hiddenSource: TabPillView?

    /// Round-6 R6.C: true between `draggingEntered` and the matching
    /// `concludeDragOperation` / `draggingExited`. The App's
    /// `rebuildTabBar` checks this flag and postpones rebuilds while
    /// a drag is in flight — otherwise a sibling-client
    /// `TabsReorderedEvent` arriving mid-drag would yank pills out
    /// of `arrangedSubviews` while the placeholder is still in
    /// place, and the user's drag would visually break.
    private(set) var isDragInProgress = false

    /// Set by `rebuildTabBar` when it skips a rebuild during a drag.
    /// `onDragEnded` reads it and asks the App to flush.
    var rebuildPending = false

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        registerForDraggedTypes([.roostTabID])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    override func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation {
        guard sender.draggingPasteboard.types?.contains(.roostTabID) == true
        else { return [] }
        installPlaceholder(for: sender)
        return .move
    }

    override func draggingUpdated(_ sender: any NSDraggingInfo) -> NSDragOperation {
        guard sender.draggingPasteboard.types?.contains(.roostTabID) == true
        else { return [] }
        movePlaceholder(for: sender)
        return .move
    }

    override func draggingExited(_ sender: (any NSDraggingInfo)?) {
        // Drag left the strip entirely (cursor moved away, or
        // dropped past the scroll view's bounds). Roll back the
        // placeholder; no reorder fires.
        teardownPlaceholder(commit: false)
    }

    override func performDragOperation(_ sender: any NSDraggingInfo) -> Bool {
        guard let idStr = sender.draggingPasteboard.string(forType: .roostTabID),
              let id = Int64(idStr)
        else { return false }
        let local = convert(sender.draggingLocation, from: nil)
        // Same hit-test the visual placeholder follows — the daemon's
        // canonical drop slot must agree with what the user saw on
        // screen. `slotsForHitTest` skips the hidden source pill so
        // the indices line up with what `computeInsertIdx` expects.
        let rawTarget = hitTestRawTargetIdx(at: local)
        onDropTab?(id, rawTarget)
        return true
    }

    override func concludeDragOperation(_ sender: (any NSDraggingInfo)?) {
        // `performDragOperation` fired the RPC asynchronously; the
        // daemon's `TabsReorderedEvent` arm will trigger
        // `rebuildTabBar` and re-seat the (no-longer-hidden) pill at
        // its new slot. Either way, clean up the placeholder and
        // restore the source pill's visibility here.
        teardownPlaceholder(commit: true)
    }

    // MARK: - Placeholder lifecycle

    /// Find the dragged pill in `arrangedSubviews`, hide it, and
    /// insert a transparent placeholder at the would-land slot.
    /// Called from `draggingEntered`. No-op if a placeholder is
    /// already present (re-entry during a single drag).
    private func installPlaceholder(for sender: any NSDraggingInfo) {
        guard dropPlaceholder == nil else { return }
        guard let idStr = sender.draggingPasteboard.string(forType: .roostTabID),
              let sourceID = Int64(idStr)
        else { return }
        let pills = arrangedSubviews.compactMap { $0 as? TabPillView }
        let source = pills.first(where: { $0.tabID == sourceID })

        // Hide the source pill — the floating drag-snapshot
        // already shows the user what they're moving. The
        // placeholder takes its place in the layout.
        if let source {
            source.isHidden = true
            hiddenSource = source
        }

        // Build the placeholder. Width matches the hidden source's
        // current frame so the gap is exactly pill-sized; height
        // matches the pill's 24pt intrinsic height. Transparent
        // background — we don't want any visual chrome inside the
        // gap.
        let placeholder = NSView()
        placeholder.translatesAutoresizingMaskIntoConstraints = false
        let pillWidth = source?.frame.width ?? 120
        NSLayoutConstraint.activate([
            placeholder.widthAnchor.constraint(equalToConstant: pillWidth),
            placeholder.heightAnchor.constraint(equalToConstant: 24),
        ])
        dropPlaceholder = placeholder

        // Compute the initial slot. `hitTestRawTargetIdx` operates
        // in the "slots-for-drop" coordinate system: the hidden
        // source is excluded so indices reflect what the user
        // actually sees.
        let local = convert(sender.draggingLocation, from: nil)
        let slot = hitTestRawTargetIdx(at: local)
        let insertIndex = clampedInsertIndex(slot)
        insertArrangedSubview(placeholder, at: insertIndex)
        dropPlaceholderIndex = insertIndex

        isDragInProgress = true
    }

    /// Move the placeholder to the slot under the cursor. No-op if
    /// the slot hasn't actually changed since last update.
    private func movePlaceholder(for sender: any NSDraggingInfo) {
        guard let placeholder = dropPlaceholder else { return }
        let local = convert(sender.draggingLocation, from: nil)
        let slot = hitTestRawTargetIdx(at: local)
        let nextIndex = clampedInsertIndex(slot)
        guard nextIndex != dropPlaceholderIndex else { return }
        dropPlaceholderIndex = nextIndex

        // Animate the reposition. `allowsImplicitAnimation = true`
        // is what lets NSStackView's frame updates ride a CABasic
        // position transition without per-pill explicit declarations.
        // 0.15s ease-out matches macOS sheet/popover timing — quick
        // enough that fast drags don't feel sluggish, slow enough
        // that the eye can track the gap.
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration = 0.15
            ctx.timingFunction = CAMediaTimingFunction(name: .easeOut)
            ctx.allowsImplicitAnimation = true
            removeArrangedSubview(placeholder)
            insertArrangedSubview(placeholder, at: nextIndex)
        }
    }

    /// Remove the placeholder, restore the source pill (if still
    /// hidden), reset drag state, and fire `onDragEnded` so the App
    /// can flush a pending rebuild. `commit` is purely informational
    /// today; if a future revision needs to know "did this end at a
    /// drop or a cancel", that's the signal.
    ///
    /// CR on PR #75: only fire `onDragEnded` when an actual tab drag
    /// was in flight. `TabBarScrollView` forwards drag-exit / conclude
    /// events to us indiscriminately; if some non-tab drag drove
    /// `draggingExited` we'd otherwise trigger a spurious
    /// `rebuildTabBar()` via the callback.
    private func teardownPlaceholder(commit: Bool) {
        let hadActiveDrag = isDragInProgress
            || dropPlaceholder != nil
            || hiddenSource != nil

        if let placeholder = dropPlaceholder {
            removeArrangedSubview(placeholder)
            placeholder.removeFromSuperview()
        }
        dropPlaceholder = nil
        dropPlaceholderIndex = nil

        // Restore the source pill's visibility. After
        // `performDragOperation` returns true the daemon's reorder
        // event arm will fire `rebuildTabBar` which `configure`s
        // the cached pill back to `isHidden = false` anyway — but
        // doing it here too means there's no flicker frame between
        // teardown and rebuild.
        if let source = hiddenSource {
            source.isHidden = false
        }
        hiddenSource = nil
        isDragInProgress = false

        // Drain a deferred rebuild that arrived mid-drag. Also fire
        // `onDragEnded` on the happy-path end-of-drag — but skip it
        // entirely when no drag was active, so non-tab forwarded
        // events don't spam `rebuildTabBar()`.
        if rebuildPending {
            rebuildPending = false
            onDragEnded?()
        } else if hadActiveDrag {
            onDragEnded?()
        }
        _ = commit
    }

    // MARK: - Hit test

    /// Walk the visible slots (pills + placeholder, excluding the
    /// hidden source) left-to-right and return the index the user's
    /// drop point lands at. Mid-slot is the threshold — drops in
    /// the left half of slot `i` land at raw target `i`; drops in
    /// the right half land at `i + 1`. Tail drops past every slot
    /// return `slots.count`.
    ///
    /// During a drag, the placeholder counts as a slot (it
    /// represents where the source will land) and the hidden source
    /// pill is skipped (the user no longer sees it). Outside a
    /// drag, this collapses to "iterate pills" — same as the
    /// pre-R6.C implementation.
    private func hitTestRawTargetIdx(at point: NSPoint) -> Int {
        let slots: [NSView] = arrangedSubviews.compactMap { view in
            if let pill = view as? TabPillView {
                return pill.isHidden ? nil : pill
            }
            if view === dropPlaceholder { return view }
            return nil
        }
        for (i, slot) in slots.enumerated() {
            if point.x < slot.frame.midX { return i }
        }
        return slots.count
    }

    /// Translate a "slot among visible slots" index into an
    /// `insertArrangedSubview(at:)` index that walks the full
    /// `arrangedSubviews` array (which includes the trailing `+ Tab`
    /// button + potentially the hidden source pill). Without this,
    /// inserting at slot N from a hit-test would skip past the
    /// trailing affordance and land in the wrong spot.
    private func clampedInsertIndex(_ slot: Int) -> Int {
        // Walk arrangedSubviews; count visible pills until we've
        // passed `slot` of them. Insertion happens BEFORE the
        // (slot)-th visible pill.
        var visibleSeen = 0
        for (i, view) in arrangedSubviews.enumerated() {
            if let pill = view as? TabPillView, !pill.isHidden {
                if visibleSeen == slot { return i }
                visibleSeen += 1
            } else if view === dropPlaceholder {
                // Skip the placeholder itself when counting (we're
                // about to re-insert it, so it doesn't occupy a slot
                // for the purpose of this calculation).
                continue
            }
        }
        // Past every visible pill — insert before the trailing
        // affordance (`+ Tab` button is the last arranged subview).
        return max(0, arrangedSubviews.count - 1)
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
