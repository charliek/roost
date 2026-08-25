// Per-project sidebar rollup.
//
// Reduces a project's tabs to the one colour its sidebar row's 3px
// leading stripe wears (`ProjectRowCellView`); this module picks which.
//
// Both halves are shared, not local: the per-tab value is
// `Agent.effectiveLifecycle` (the same value the tab pill's status dot
// renders) and the ordering is `Agent.rank` (the same function the
// future agent overview sorts by). So the sidebar can't disagree with
// the rest of the UI about which tab is loudest.
//
// Hook-driven state **participates**. Until plan 002 this module
// skipped every tab with `hookActive` set, so a project whose only
// blocked tab was a Claude session showed no stripe at all — the
// differentiating case, hidden. Kept at parity with the Linux port
// (`crates/roost-ui-model/src/rollup.rs`, shared by iced).

import AppKit

/// Compute the project rollup: the highest-`Agent.rank` tab wins.
/// No tabs → `.inactive`.
///
/// Pure function; no AppKit state. Unit-tested in `ProjectRollupTests`.
func projectRollup(_ lifecycles: [AgentLifecycle]) -> AgentLifecycle {
    lifecycles.max { Agent.rank($0) < Agent.rank($1) } ?? .inactive
}

/// Stripe colour for a rollup, or `nil` for `.inactive` (no colour =
/// no stripe). Matches the Linux CSS palette (`#5fa3f0` / `#f0a040` /
/// `#7a7a7a` / `#e05252`) verbatim so the two UIs agree visually.
///
/// `failed` is its own colour, not a louder needs-input: the legacy
/// `tab.state` field collapses the two (it stays a closed four-value
/// enum, see `Agent.effective`), so the stripe and the pill dot are
/// the only places a user can tell "the agent wants you" from "the
/// agent died".
func rollupColor(for lifecycle: AgentLifecycle) -> NSColor? {
    switch lifecycle {
    case .inactive: return nil
    case .working:  return NSColor(red: 0x5f/255.0, green: 0xa3/255.0, blue: 0xf0/255.0, alpha: 1.0)
    case .waiting:  return NSColor(red: 0xf0/255.0, green: 0xa0/255.0, blue: 0x40/255.0, alpha: 1.0)
    case .finished: return NSColor(red: 0x7a/255.0, green: 0x7a/255.0, blue: 0x7a/255.0, alpha: 1.0)
    case .failed:   return NSColor(red: 0xe0/255.0, green: 0x52/255.0, blue: 0x52/255.0, alpha: 1.0)
    }
}
