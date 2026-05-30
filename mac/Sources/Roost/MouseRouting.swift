// Mouse-tracking routing decisions — pure helpers shared by
// `TerminalView` and `MouseRoutingTests`.
//
// libghostty-vt's `MouseEncoder` already gates internally on the
// negotiated mode + format (1000 vs 1002 vs 1003 vs 1006 vs 1015 vs
// 1016): an `encode` call returns empty bytes when the mode doesn't
// permit this kind of report. The Mac UI only needs to decide
// whether to invoke the encoder at all, vs. routing the event to
// selection / URL hover / paste.
//
// The decision is a pure function of:
//   * the event kind + button,
//   * whether mouse-tracking is active at all (`mouse_tracking`
//     in roost-vt; `GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING` from C),
//   * whether the click intersects a URL with the Cmd modifier
//     held (URL precedence wins over mouse forwarding — matches
//     ghostty so users can still ⌘-click links in TUIs that ask
//     for mouse tracking).
//
// Modes 1002 (drag) and 1003 (motion-no-button) need NO extra
// branch in the routing helper — the encoder decides. We only need
// motion-no-button gating at the TerminalView call site so we don't
// spend cycles in the throttle/encoder when the app didn't ask for
// any-motion. That gate lives in `mouseMoved(with:)` (mode 1003
// check via `terminal.mode_get(1003)`).
//
// This file keeps NO @MainActor isolation and depends ONLY on
// Swift stdlib types so swift-testing can drive it without an
// `NSView` (mirrors the `WordSelection` / `UrlDetection` test
// seam).

import AppKit
import Foundation

// MARK: - OSC 22 cursor shape mapping

/// Map a W3C CSS cursor name (as carried by OSC 22) to the matching
/// `NSCursor`. Unknown names + the empty string both fall back to
/// `.arrow` — ghostty silently ignores unknowns and the empty form
/// is the documented reset shape.
///
/// Why the empty fallback: strix avoids `\x1b]22;\x1b\\` because
/// ghostty/macOS rejects it, but other TUIs use it as the reset
/// form. Roost accepts both: empty body and `"default"` both map
/// to the platform default arrow.
@MainActor
func nsCursorForW3CName(_ name: String) -> NSCursor {
    switch name {
    case "", "default":
        return .arrow
    case "pointer":
        return .pointingHand
    case "text":
        return .iBeam
    case "crosshair":
        return .crosshair
    case "grab":
        return .openHand
    case "grabbing":
        return .closedHand
    case "not-allowed":
        return .operationNotAllowed
    case "col-resize":
        return .resizeLeftRight
    case "row-resize":
        return .resizeUpDown
    case "e-resize":
        return .resizeRight
    case "w-resize":
        return .resizeLeft
    case "n-resize":
        return .resizeUp
    case "s-resize":
        return .resizeDown
    default:
        return .arrow
    }
}

/// Canonical form of an OSC 22 W3C name for the `app.cursor_shape`
/// IPC op. Maps the empty reset form to `"default"` so test clients
/// can always assert against a non-empty name. Unknown names pass
/// through verbatim — the renderer falls back to `.arrow` but the
/// canonical name is still the raw payload, so tests can pin "I
/// asked for X; got X back" without depending on the mapping.
func canonicalCursorShape(_ name: String) -> String {
    if name.isEmpty { return "default" }
    return name
}

/// One of the three mouse actions libghostty-vt's encoder accepts.
/// Re-declared without the `CGhosttyVT` import so the test target
/// doesn't need to link against libghostty-vt.
enum MouseRoutingAction: Equatable {
    case press
    case release
    case motion
}

/// One of the five buttons the encoder reports on. Wheel up = `four`,
/// wheel down = `five` — the X11 convention every mouse-tracking app
/// reads. `nil` is used at call sites that mean "no button" (motion
/// under mode 1003).
enum MouseRoutingButton: Equatable {
    case left
    case right
    case middle
    case four
    case five
}

/// What the call site should do with this mouse event.
enum MouseRoutingDispatch: Equatable {
    /// Forward the event to the encoder with these parameters. A
    /// `nil` button means "clear the button on the event" — the
    /// motion-no-button path for mode 1003.
    case forward(action: MouseRoutingAction, button: MouseRoutingButton?)
    /// Don't forward to the encoder. Fall through to selection /
    /// paste / URL hover / whatever the legacy path was.
    case passThrough
}

/// Pure throttle for mode 1003 motion-no-button reports. Apps that
/// enable `\x1b[?1003h` get every pointer movement; without a cap
/// the encoder + PTY drain can spend a real chunk of a frame budget
/// on motion bytes when the user sweeps the pointer fast.
///
/// Rules:
///   * Suppress when the cell didn't change since the last emit
///     (per-cell already covers the encoder's own deduplication
///     budget, but doing it up front saves a `set_position` + the
///     scratch-buffer round-trip).
///   * Cap to 60 Hz — 16 ms minimum gap between emits. Strix and
///     similar TUIs only consume motion to update hover state on a
///     small set of UI rects; 60 Hz is plenty for a smooth highlight.
///
/// `nowSeconds` is a monotonic-clock-style timestamp in seconds.
/// Tests inject a hand-rolled clock instead of `CFAbsoluteTime`.
struct MotionEmitter: Equatable {
    /// Minimum gap between emitted reports in seconds. 60 Hz cap.
    static let minIntervalSeconds: Double = 1.0 / 60.0

    /// Last cell we emitted a report for (column, row). `nil` until
    /// the first emit.
    var lastCell: (col: Int, row: Int)?
    /// Monotonic timestamp of the last emit. `nil` until the first
    /// emit.
    var lastEmit: Double?

    init() {
        lastCell = nil
        lastEmit = nil
    }

    static func == (lhs: MotionEmitter, rhs: MotionEmitter) -> Bool {
        lhs.lastCell?.col == rhs.lastCell?.col
            && lhs.lastCell?.row == rhs.lastCell?.row
            && lhs.lastEmit == rhs.lastEmit
    }

    /// Pure read-only check: would `commit(col, row, nowSeconds)`
    /// actually advance the state and emit? Used by the production
    /// path to skip the encoder + scratch alloc when the answer is
    /// "no". The caller must follow up with `commit` after a
    /// successful encode so the throttle doesn't lock state on
    /// events that the encoder declined.
    func wouldEmit(col: Int, row: Int, nowSeconds: Double) -> Bool {
        if let lc = lastCell, lc.col == col, lc.row == row {
            return false
        }
        if let last = lastEmit, nowSeconds - last < Self.minIntervalSeconds {
            return false
        }
        return true
    }

    /// Advance the throttle state. Call this only after the encoder
    /// successfully produced bytes — committing on a declined encode
    /// would silently suppress the next event the encoder would
    /// accept (e.g. mode 1003 toggling on between two same-cell
    /// motions). See `pytest test_mouse_tracking.py::
    /// test_motion_no_button_emits_only_in_mode_1003`.
    mutating func commit(col: Int, row: Int, nowSeconds: Double) {
        lastCell = (col: col, row: row)
        lastEmit = nowSeconds
    }

}

/// Decide whether a mouse event should be forwarded to the PTY via
/// the mouse encoder, or passed through to the existing
/// selection/paste/URL paths.
///
/// Precedence (highest → lowest):
/// 1. URL takes priority over mouse forwarding (`urlInterceptsClick`
///    is true when Cmd is held over a URL hit). Matches ghostty.
/// 2. Mouse tracking off → pass through (selection, paste,
///    copy-on-select, regular click-count logic).
/// 3. Otherwise → forward to encoder. libghostty's encoder gates on
///    the negotiated mode/format internally; an empty `encode` return
///    when the mode declines is the encoder's job.
func computeMouseTrackingDispatch(
    eventKind: MouseRoutingAction,
    button: MouseRoutingButton?,
    isMouseTrackingActive: Bool,
    urlInterceptsClick: Bool
) -> MouseRoutingDispatch {
    if urlInterceptsClick { return .passThrough }
    if !isMouseTrackingActive { return .passThrough }
    return .forward(action: eventKind, button: button)
}
