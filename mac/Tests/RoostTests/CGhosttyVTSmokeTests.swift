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

import AppKit
import CGhosttyVT
import Testing

@testable import Roost

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

/// Hard gate for the command-palette live-theme feature: prove that
/// `Theme.apply` re-applies the default background + 256-color palette
/// to a terminal that has ALREADY processed `vt_write` data. The
/// feature's live preview reapplies the palette mid-session; the header
/// (`COLOR_PALETTE` is a setter, `DATA_COLOR_*` are getters) says this
/// is supported, but `Theme.apply`'s own doc-comment historically said
/// colors "MUST" be set before the first write. This test pins the
/// real behavior so the rest of the feature can rely on it — and would
/// fail loudly if a future Ghostty bump made palette set write-once.
@Test @MainActor
func themeAppliesAfterVtWrite() {
    var opts = GhosttyTerminalOptions()
    opts.cols = 80
    opts.rows = 24
    opts.max_scrollback = 0

    var term: GhosttyTerminal?
    #expect(ghostty_terminal_new(nil, &term, opts).rawValue == 0)
    guard let term else { return }
    defer { ghostty_terminal_free(term) }

    // Simulate a live session: SGR red text + a newline, so the
    // terminal has parsed VT data and advanced its screen state
    // before we touch the palette.
    let written: [UInt8] = Array("\u{1b}[31mred\u{1b}[0m\r\n".utf8)
    written.withUnsafeBufferPointer {
        ghostty_terminal_vt_write(term, $0.baseAddress, written.count)
    }

    // A theme with byte-exact sRGB colors so the round-trip through
    // NSColor → GhosttyColorRgb stays exact (i/255 * 255 rounds to i).
    let theme = Theme(
        foreground: NSColor(srgbRed: 250.0 / 255, green: 251.0 / 255, blue: 252.0 / 255, alpha: 1),
        background: NSColor(srgbRed: 1.0 / 255, green: 2.0 / 255, blue: 3.0 / 255, alpha: 1),
        cursor: NSColor(srgbRed: 10.0 / 255, green: 11.0 / 255, blue: 12.0 / 255, alpha: 1),
        selectionBackground: .gray,
        selectionForeground: .white,
        palette: (0..<256).map {
            NSColor(srgbRed: CGFloat($0) / 255, green: 0, blue: 0, alpha: 1)
        }
    )
    Theme.apply(theme, to: term)

    var bg = GhosttyColorRgb(r: 0, g: 0, b: 0)
    #expect(ghostty_terminal_get(term, GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND, &bg).rawValue == 0)
    #expect(
        bg.r == 1 && bg.g == 2 && bg.b == 3,
        "background should reflect the post-write theme (got \(bg.r),\(bg.g),\(bg.b))"
    )

    var fg = GhosttyColorRgb(r: 0, g: 0, b: 0)
    #expect(ghostty_terminal_get(term, GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND, &fg).rawValue == 0)
    #expect(fg.r == 250 && fg.g == 251 && fg.b == 252)

    var palette = [GhosttyColorRgb](repeating: GhosttyColorRgb(r: 0, g: 0, b: 0), count: 256)
    palette.withUnsafeMutableBufferPointer {
        #expect(ghostty_terminal_get(term, GHOSTTY_TERMINAL_DATA_COLOR_PALETTE, $0.baseAddress).rawValue == 0)
    }
    #expect(
        palette[5].r == 5 && palette[5].g == 0 && palette[5].b == 0,
        "palette entry 5 should reflect the post-write theme (got \(palette[5].r),\(palette[5].g),\(palette[5].b))"
    )
    #expect(palette[200].r == 200)
}
