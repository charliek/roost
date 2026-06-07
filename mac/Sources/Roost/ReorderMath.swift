// Drag-drop reorder math for sidebar projects + tab pills.
//
// Pure, sync, no AppKit dependency so it can be unit-tested without
// driving NSDraggingSession headlessly. Tested with a 20-case table
// in ReorderMathTests. The Linux UI's
// `crates/roost-linux/src/app.rs::compute_insert_idx` mirrors this —
// keep both byte-equivalent so dragging behaves identically on
// either UI.

import Foundation

/// Result of mapping a "user dropped between rows i and i-1" intent
/// onto the actual `insert(at:)` index a list view's
/// remove-then-insert reorder primitive needs.
///
/// `isNoop` is true when the raw drop target sits on top of, or
/// immediately after, the source row's current position — both cases
/// mean "leave order alone." Callers should short-circuit before
/// firing an RPC.
struct InsertIdx: Equatable {
    let index: Int
    let isNoop: Bool
}

/// Compute the `insert(at:)` index for a drag-drop where the source
/// row currently sits at `sourceIdx` and the desired insertion point
/// in the present (with-source) order is `rawTargetIdx`.
///
/// When the drop is forward of the source, removing the source first
/// shifts every later index down by one — so the insert position is
/// `rawTargetIdx - 1`. When the drop is backward of the source, the
/// raw target is unchanged. Drops on or immediately after the source
/// row are no-ops (`isNoop: true`, `index` reported as 0 — callers
/// should ignore index when `isNoop`).
func computeInsertIdx(sourceIdx: Int, rawTargetIdx: Int) -> InsertIdx {
    if rawTargetIdx == sourceIdx || rawTargetIdx == sourceIdx + 1 {
        return InsertIdx(index: 0, isNoop: true)
    }
    if rawTargetIdx > sourceIdx {
        return InsertIdx(index: rawTargetIdx - 1, isNoop: false)
    }
    return InsertIdx(index: rawTargetIdx, isNoop: false)
}
