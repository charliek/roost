// Inverse + bold-accent resolver tests, Swift companion to the Rust
// suite in `crates/roost-linux/src/terminal_view.rs::tests`. The two
// renderers MUST behave identically for inverse-marked TUI chrome
// (codex's `\e[7m` prompt row) and bold default-fg text — getting
// either case wrong produces a visible regression. The cases below
// are 1:1 with the Rust file; add Rust tests in lockstep when this
// file grows.
//
// Also exercises the libghostty STYLE readback end-to-end: feeds an
// SGR-inverse byte stream through a real `RenderState.walk` and
// asserts the `inverse` bit lands on the right cell. That half is
// the Swift mirror of `walk_reads_style_bits_for_inverse_cells` in
// `crates/roost-vt/src/render_state.rs`.

import AppKit
import CGhosttyVT
import Testing

@testable import Roost

// MARK: - Pure resolver tests (no libghostty needed)

private func cell(
    background: NSColor? = nil,
    foreground: NSColor? = nil,
    bold: Bool = false,
    italic: Bool = false,
    inverse: Bool = false
) -> RenderState.Cell {
    RenderState.Cell(
        row: 0,
        col: 0,
        background: background,
        foreground: foreground,
        glyph: nil,
        bold: bold,
        italic: italic,
        inverse: inverse
    )
}

private let defaultFg = NSColor(srgbRed: 229.0 / 255, green: 229.0 / 255, blue: 229.0 / 255, alpha: 1)
private let defaultBg = NSColor(srgbRed: 28.0 / 255, green: 28.0 / 255, blue: 28.0 / 255, alpha: 1)
private let explicitFg = NSColor(srgbRed: 128.0 / 255, green: 192.0 / 255, blue: 64.0 / 255, alpha: 1)
private let explicitBg = NSColor(srgbRed: 58.0 / 255, green: 58.0 / 255, blue: 58.0 / 255, alpha: 1)
private let boldAccent = NSColor.white

@Test
func resolver_plainDefaultCell_skipsBgFill() {
    let (fg, bg, hasBg) = TerminalView.resolveCellColors(
        cell: cell(),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: nil
    )
    #expect(fg === defaultFg)
    #expect(bg === defaultBg)
    #expect(!hasBg, "default cell must not trigger a per-cell bg fill")
}

@Test
func resolver_explicitBg_isFillable() {
    let (fg, bg, hasBg) = TerminalView.resolveCellColors(
        cell: cell(background: explicitBg),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: nil
    )
    #expect(fg === defaultFg)
    #expect(bg === explicitBg)
    #expect(hasBg)
}

/// The codex regression: `\e[7m` on an otherwise-default cell.
/// Pre-fix the resolver simply rendered the cell at default fg/bg
/// with no bg fill, so the gray prompt row stayed black against
/// the canvas.
@Test
func resolver_inverseDefaultCell_swapsColorsAndForcesBgFill() {
    let (fg, bg, hasBg) = TerminalView.resolveCellColors(
        cell: cell(inverse: true),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: nil
    )
    #expect(fg === defaultBg, "inverse swap: fg becomes default bg")
    #expect(bg === defaultFg, "inverse swap: bg becomes default fg")
    #expect(hasBg, "inverse must force hasExplicitBg=true so the swap paints")
}

@Test
func resolver_inverseWithExplicitColors_swapsThem() {
    let (fg, bg, hasBg) = TerminalView.resolveCellColors(
        cell: cell(background: explicitBg, foreground: explicitFg, inverse: true),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: nil
    )
    #expect(fg === explicitBg)
    #expect(bg === explicitFg)
    #expect(hasBg)
}

/// Boundary: inverse on a cell that has only an explicit fg (no
/// explicit bg). The default bg sits in the bg slot before the
/// swap, so after inverse the effective fg should be `defaultBg`
/// and the effective bg should be the originally-explicit fg.
/// Mirror of the Rust `inverse_with_only_explicit_fg_*` case.
@Test
func resolver_inverseWithOnlyExplicitFg_swapsDefaultBgIntoFg() {
    let (fg, bg, hasBg) = TerminalView.resolveCellColors(
        cell: cell(foreground: explicitFg, inverse: true),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: nil
    )
    #expect(fg === defaultBg)
    #expect(bg === explicitFg)
    #expect(hasBg)
}

/// Mirror of the above with the colors flipped: explicit bg only,
/// no explicit fg. After the swap effective fg = explicit bg,
/// effective bg = default fg.
@Test
func resolver_inverseWithOnlyExplicitBg_swapsDefaultFgIntoBg() {
    let (fg, bg, hasBg) = TerminalView.resolveCellColors(
        cell: cell(background: explicitBg, inverse: true),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: nil
    )
    #expect(fg === explicitBg)
    #expect(bg === defaultFg)
    #expect(hasBg)
}

@Test
func resolver_boldDefaultFg_usesBoldAccentWhenProvided() {
    let (fg, _, _) = TerminalView.resolveCellColors(
        cell: cell(bold: true),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: boldAccent
    )
    #expect(fg === boldAccent)
}

@Test
func resolver_boldWithExplicitFg_keepsTheExplicitFg() {
    let (fg, _, _) = TerminalView.resolveCellColors(
        cell: cell(foreground: explicitFg, bold: true),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: boldAccent
    )
    #expect(
        fg === explicitFg,
        "bold accent must not override explicit SGR fg (e.g. bold red stays red)"
    )
}

@Test
func resolver_boldWithInverse_doesNotApplyAccentToSwappedBg() {
    // After inverse, fg=default_bg. Applying boldAccent here would
    // land it in the bg position and produce the wrong visual. The
    // legacy guard `!inverse` prevents this.
    let (fg, bg, _) = TerminalView.resolveCellColors(
        cell: cell(bold: true, inverse: true),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: boldAccent
    )
    #expect(fg === defaultBg, "post-inverse fg must remain default_bg")
    #expect(bg === defaultFg, "post-inverse bg must remain default_fg")
}

@Test
func resolver_boldColorNil_disablesTheAccent() {
    let (fg, _, _) = TerminalView.resolveCellColors(
        cell: cell(bold: true),
        defaultFg: defaultFg,
        defaultBg: defaultBg,
        boldColor: nil
    )
    #expect(fg === defaultFg, "nil boldColor must leave default fg unchanged")
}

// MARK: - libghostty FFI: STYLE bit round-trips through RenderState.walk

/// Mirror of the Rust `walk_reads_style_bits_for_inverse_cells` test
/// in `crates/roost-vt/src/render_state.rs`. Pre-fix `RenderState.Cell`
/// had no `inverse`/`bold` field at all — the bits got silently
/// dropped on the way through the walk. This pins the round trip
/// libghostty → walk callback for the inverse-marked cell *and* the
/// post-reset cell that follows.
@Test @MainActor
func renderState_walkReadsStyleBitsForInverseCells() throws {
    var opts = GhosttyTerminalOptions()
    opts.cols = 80
    opts.rows = 24
    opts.max_scrollback = 0

    var maybeTerm: GhosttyTerminal?
    #expect(ghostty_terminal_new(nil, &maybeTerm, opts).rawValue == 0)
    let term = try #require(maybeTerm, "ghostty_terminal_new returned success but term is nil")
    defer { ghostty_terminal_free(term) }

    // CSI 1;7 m = bold + inverse on; X; CSI 0 m reset; Y.
    let bytes: [UInt8] = Array("\u{1b}[1;7mX\u{1b}[0mY".utf8)
    bytes.withUnsafeBufferPointer {
        ghostty_terminal_vt_write(term, $0.baseAddress, bytes.count)
    }

    let renderState = RenderState()
    renderState.update(terminal: term)

    var row0: [(col: Int, glyph: Character, bold: Bool, inverse: Bool)] = []
    renderState.walk { cell in
        if cell.row == 0, let g = cell.glyph, !g.isWhitespace {
            row0.append((cell.col, g, cell.bold, cell.inverse))
        }
    }

    let xCell = try #require(
        row0.first(where: { $0.glyph == "X" }),
        "X cell missing from row 0 walk: \(row0)"
    )
    #expect(
        xCell.inverse,
        "X cell should carry inverse=true after \\e[1;7m, got \(xCell)"
    )
    #expect(
        xCell.bold,
        "X cell should carry bold=true after \\e[1;7m, got \(xCell)"
    )

    let yCell = try #require(
        row0.first(where: { $0.glyph == "Y" }),
        "Y cell missing from row 0 walk: \(row0)"
    )
    #expect(!yCell.inverse, "Y cell (post-reset) must not carry inverse, got \(yCell)")
    #expect(!yCell.bold, "Y cell (post-reset) must not carry bold, got \(yCell)")
}
