// Path display helpers for window chrome.
//
// Keeps the Mac subtitle string identical to what the GTK headerbar
// shows for the same cwd. Pure functions so they can be unit-tested
// without AppKit / a window context.

import Foundation

/// Collapse a `$HOME` prefix to `~` and left-truncate with an ellipsis
/// when the result exceeds `max` runes. Trailing path segments are what
/// users recognize at a glance, so we keep the tail. Counts grapheme
/// scalars (i.e. `Character`s) rather than bytes, so CJK / emoji /
/// accented characters don't slice mid-codepoint.
///
/// Covered case-for-case by `PathDisplayTests`.
func pathDisplay(_ path: String, home: String, max: Int) -> String {
    // Defensive guard for non-positive `max`: `Collection.suffix(_:)`
    // documents a runtime trap on negative arguments, and the
    // truncation branch below passes `max - 1` straight through. The
    // function is exported for unit testing, so it should handle
    // edge inputs even though current callers pass 48 (subtitle
    // budget) and `Int.max` (no cap). Empty-string fallback is
    // semantically "render zero characters." Flagged by CodeRabbit
    // on PR #67.
    guard max > 0 else { return "" }
    var p = path
    if !home.isEmpty {
        if p == home {
            p = "~"
        } else if p.hasPrefix(home + "/") {
            p = "~" + p.dropFirst(home.count)
        }
    }
    let chars = Array(p)
    if chars.count <= max { return p }
    return "…" + String(chars.suffix(max - 1))
}
