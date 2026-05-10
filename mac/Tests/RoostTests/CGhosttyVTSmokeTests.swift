// Mac-side companion to the Rust `roost-vt::vt_smoke` test. Validates the
// libghostty-vt FFI wiring end-to-end: SwiftPM systemLibrary target
// resolves the headers, the static archive is on the link line, and the
// C symbols (ghostty_terminal_new, ghostty_terminal_vt_write,
// ghostty_terminal_free) are reachable from Swift.
//
// The point isn't to test libghostty-vt itself — Ghostty has its own
// suite — but to fail fast if our build pipeline ever drifts: header
// path, archive path, or platform symbol visibility breaks. Same
// invariant the Rust crate's smoke pins on the daemon side.

import CGhosttyVT
import Testing

@Test
func libghosttyVtRoundTrip() {
    var opts = GhosttyTerminalOptions()
    opts.cols = 80
    opts.rows = 24
    opts.max_scrollback = 0

    var term: GhosttyTerminal?
    let rc = ghostty_terminal_new(nil, &term, opts)
    // libghostty-vt returns GhosttyResult (typedef enum). Swift's C
    // importer wraps it as a struct, so we compare on the underlying
    // integer (`rawValue`) rather than against an Int32 literal.
    // GHOSTTY_SUCCESS is 0 by C convention; the Rust roost-vt smoke
    // pins the same invariant on the daemon side.
    #expect(
        rc.rawValue == 0,
        "ghostty_terminal_new should succeed (got rc.rawValue=\(rc.rawValue))"
    )
    #expect(term != nil, "ghostty_terminal_new should populate the out-handle")

    let bytes: [UInt8] = [0x68, 0x69, 0x0d, 0x0a]  // "hi\r\n"
    bytes.withUnsafeBufferPointer { ptr in
        ghostty_terminal_vt_write(term, ptr.baseAddress, bytes.count)
    }

    ghostty_terminal_free(term)
}
