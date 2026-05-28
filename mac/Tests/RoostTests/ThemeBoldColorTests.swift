// Swift companion to the Rust theme parser tests in
// `crates/roost-linux/src/theme.rs::tests`. Both UIs MUST agree on
// whether a theme opts into the Ghostty `bold-color` accent — same
// theme file, same parsed result — so the resolver's bold-default-fg
// branch fires symmetrically on Mac and Linux.

import AppKit
import CGhosttyVT
import Testing

@testable import Roost

@Test @MainActor
func bundled_roost_dark_now_has_bold_color() {
    let theme = Theme.loadBundled(name: "roost-dark")
    let bold = theme.boldColor
    // Bundled theme writes `bold-color = #ffffff` — pure white.
    let srgb = bold?.usingColorSpace(.sRGB)
    let r = srgb.map { UInt8(round($0.redComponent * 255)) }
    let g = srgb.map { UInt8(round($0.greenComponent * 255)) }
    let b = srgb.map { UInt8(round($0.blueComponent * 255)) }
    #expect(
        r == 0xff && g == 0xff && b == 0xff,
        "roost-dark must parse bold-color = #ffffff (got r=\(r as Any) g=\(g as Any) b=\(b as Any))"
    )
}

@Test @MainActor
func theme_without_bold_color_has_none() {
    // Dracula+ doesn't ship a `bold-color` line (verified by reading the
    // bundled file); its bold accent should stay nil.
    let theme = Theme.loadBundled(name: "Dracula+")
    #expect(
        theme.boldColor == nil,
        "theme without bold-color line must leave boldColor nil"
    )
}

/// End-to-end: feed `\e[1mX` through libghostty, walk via
/// `RenderState`, and confirm the resolver picks up the supplied bold
/// accent for the bold default-fg `X` cell. Pins the chain
/// (libghostty → walk → resolver) so a regression in any link fails
/// this test rather than waiting on a manual smoke. Companion to the
/// Rust `bold_default_fg_through_libghostty_uses_theme_bold_color`.
@Test @MainActor
func boldDefaultFgThroughLibghosttyUsesThemeBoldColor() throws {
    var opts = GhosttyTerminalOptions()
    opts.cols = 80
    opts.rows = 24
    opts.max_scrollback = 0

    var maybeTerm: GhosttyTerminal?
    #expect(ghostty_terminal_new(nil, &maybeTerm, opts).rawValue == 0)
    let term = try #require(maybeTerm, "ghostty_terminal_new returned success but term is nil")
    defer { ghostty_terminal_free(term) }

    // CSI 1 m = bold on; X.
    let bytes: [UInt8] = Array("\u{1b}[1mX".utf8)
    bytes.withUnsafeBufferPointer {
        ghostty_terminal_vt_write(term, $0.baseAddress, bytes.count)
    }

    let renderState = RenderState()
    renderState.update(terminal: term)

    let defaultFg = NSColor(srgbRed: 229.0 / 255, green: 229.0 / 255, blue: 229.0 / 255, alpha: 1)
    let defaultBg = NSColor(srgbRed: 28.0 / 255, green: 28.0 / 255, blue: 28.0 / 255, alpha: 1)
    let boldAccent = NSColor(srgbRed: 0xaa / 255.0, green: 0xbb / 255.0, blue: 0xcc / 255.0, alpha: 1)

    var resolvedFg: NSColor?
    renderState.walk { cell in
        if cell.row == 0, let g = cell.glyph, g == "X" {
            let (fg, _, _) = TerminalView.resolveCellColors(
                cell: cell,
                defaultFg: defaultFg,
                defaultBg: defaultBg,
                boldColor: boldAccent
            )
            resolvedFg = fg
        }
    }

    let fg = try #require(resolvedFg, "X cell missing from walk")
    #expect(
        fg === boldAccent,
        "bold default-fg X must resolve to the supplied bold accent, got \(fg)"
    )
}
