// Per-project sidebar rollup state machine.
//
// Aggregates each tab's agent state + `hook_active` flag into a single
// `RollupState` for the project's sidebar row. The 3px colored stripe
// on `ProjectRowCellView` picks its color from this enum.
//
// Mirrors the Linux gtk4-rs port (`crates/roost-linux/src/rollup.rs`)
// 1:1 so a user on either UI sees identical precedence — needs-input
// wins, hook-active suppresses noise. The Go GTK binary lacks the
// hook-active suppression (Linux M6 added it); this is a deliberate
// extension past Go-parity.

import AppKit

/// Per-tab agent state. Mirrors `proto/roost.proto`'s `TabState` enum:
/// proto 0/1 collapse to `.none`; 2/3/4 → running/needsInput/idle.
/// Unknown values defensively collapse to `.none`.
enum TabAgentState {
    case none
    case running
    case needsInput
    case idle

    /// Map a proto `TabState` raw value to this UI enum.
    static func fromProto(_ value: Int) -> TabAgentState {
        switch value {
        case 2: return .running
        case 3: return .needsInput
        case 4: return .idle
        default: return .none
        }
    }
}

/// Project-level rollup. Drives both the sidebar stripe color and (via
/// `nsColor`) whether the stripe is rendered at all.
enum RollupState {
    case none
    case running
    case needsInput
    case idle

    /// Stripe color for this rollup, or `nil` when no stripe should
    /// render. Matches Linux M6's CSS palette (`#5fa3f0` / `#f0a040`
    /// / `#7a7a7a`) verbatim so the two UIs agree visually.
    var nsColor: NSColor? {
        switch self {
        case .none: return nil
        case .running:    return NSColor(red: 0x5f/255.0, green: 0xa3/255.0, blue: 0xf0/255.0, alpha: 1.0)
        case .needsInput: return NSColor(red: 0xf0/255.0, green: 0xa0/255.0, blue: 0x40/255.0, alpha: 1.0)
        case .idle:       return NSColor(red: 0x7a/255.0, green: 0x7a/255.0, blue: 0x7a/255.0, alpha: 1.0)
        }
    }

    /// Map a proto `TabState` raw value to the visually-equivalent
    /// rollup. Lets the per-tab pill dot share the same color palette
    /// as the sidebar stripe without re-declaring colors. Unknown /
    /// none → `.none` (no color).
    init(matchingProto value: Int) {
        switch TabAgentState.fromProto(value) {
        case .none:       self = .none
        case .running:    self = .running
        case .needsInput: self = .needsInput
        case .idle:       self = .idle
        }
    }
}

/// Compute the project rollup from a list of `(state, hookActive)`
/// pairs. Priority: `needsInput > running > idle > none`. When a tab
/// has `hookActive = true` its state is suppressed — the Claude hook
/// owns the urgency surface, and promoting the stripe color would
/// double-count it. Empty list → `.none`.
///
/// Pure function; no AppKit state. Unit-tested in `ProjectRollupTests`.
func projectRollup(tabs: [(state: TabAgentState, hookActive: Bool)]) -> RollupState {
    var needsInput = false
    var running = false
    var idle = false
    for (state, hookActive) in tabs {
        if hookActive { continue }
        switch state {
        case .needsInput: needsInput = true
        case .running:    running = true
        case .idle:       idle = true
        case .none:       break
        }
    }
    if needsInput { return .needsInput }
    if running    { return .running }
    if idle       { return .idle }
    return .none
}
