import CGhosttyVT
import Foundation

/// Apply a scrollback line limit to a raw libghostty-vt terminal.
///
/// `ghostty_terminal_new` no longer carries a `GhosttyTerminalOptions`
/// struct, so the scrollback limit is a post-construction
/// `ghostty_terminal_set` — the same order `TerminalView` uses. Tests
/// that want the old `max_scrollback = 0` (no scrollback retained) call
/// this right after construction. Traps on failure: a terminal whose
/// limit did not apply is not the terminal the test asked for.
func setScrollbackLines(_ terminal: GhosttyTerminal, _ lines: size_t) {
    var value = lines
    let rc = ghostty_terminal_set(
        terminal,
        GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
        &value
    )
    precondition(
        rc.rawValue == 0,
        "ghostty_terminal_set(SCROLLBACK_MAX_LINES) failed (rc=\(rc.rawValue))"
    )
}
