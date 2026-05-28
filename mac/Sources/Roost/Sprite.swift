// Geometric sprite renderer for Unicode box-drawing (U+2500–U+257F)
// and block-element (U+2580–U+259F) glyphs.
//
// Core Text font glyphs for these ranges don't tile pixel-perfectly
// across adjacent cells — you get visible hairline seams in TUI
// chrome (most obvious in the opencode wordmark logo). Ghostty
// solves this with a custom sprite renderer in
// `ghostty/src/font/sprite/draw/{block,box}.zig`; the legacy Go
// port mirrored it in `cmd/roost/sprite.go`. This module is the
// Swift + Core Graphics equivalent — every dispatch arm and helper
// is a direct port of the Go file (which itself is a port of the
// Zig original). When tweaking pixel math, cross-reference all
// three. The Rust sibling lives at
// `crates/roost-linux/src/sprite.rs` and stays in lockstep with
// this file per the CLAUDE.md parity rule.
//
// Public entry point: `Sprite.draw` — returns `true` when the
// codepoint is handled (caller skips the font glyph), `false`
// otherwise (caller falls back to NSAttributedString).

import AppKit
import CoreGraphics

enum Sprite {
    /// Draw the codepoint geometrically into the cell at
    /// `(x, y)..(x+w, y+h)` using the foreground color `fg`. Returns
    /// `true` if `cp` is in a supported range; `false` if the caller
    /// should fall back to a font glyph.
    static func draw(
        in ctx: CGContext,
        x: CGFloat,
        y: CGFloat,
        w: CGFloat,
        h: CGFloat,
        fg: NSColor,
        codepoint cp: UInt32
    ) -> Bool {
        switch cp {
        case 0x2580...0x259F:
            return drawBlockElement(in: ctx, x: x, y: y, w: w, h: h, fg: fg, cp: cp)
        case 0x2500...0x257F:
            return drawBoxGlyph(in: ctx, x: x, y: y, w: w, h: h, fg: fg, cp: cp)
        default:
            return false
        }
    }

    // MARK: - Color helpers

    private static func setFill(_ ctx: CGContext, _ color: NSColor) {
        guard let srgb = color.usingColorSpace(.sRGB) else {
            color.setFill()
            return
        }
        ctx.setFillColor(
            red: srgb.redComponent,
            green: srgb.greenComponent,
            blue: srgb.blueComponent,
            alpha: srgb.alphaComponent
        )
    }

    private static func setStroke(_ ctx: CGContext, _ color: NSColor) {
        guard let srgb = color.usingColorSpace(.sRGB) else {
            color.setStroke()
            return
        }
        ctx.setStrokeColor(
            red: srgb.redComponent,
            green: srgb.greenComponent,
            blue: srgb.blueComponent,
            alpha: srgb.alphaComponent
        )
    }

    // MARK: - Layer 1: Block elements (U+2580–U+259F)

    private enum HAlign { case left, center, right }
    private enum VAlign { case top, middle, bottom }

    private static let fEighth: CGFloat    = 1.0 / 8.0
    private static let fQuarter: CGFloat   = 1.0 / 4.0
    private static let f3Eighths: CGFloat  = 3.0 / 8.0
    private static let fHalf: CGFloat      = 1.0 / 2.0
    private static let f5Eighths: CGFloat  = 5.0 / 8.0
    private static let f3Quarters: CGFloat = 3.0 / 4.0
    private static let f7Eighths: CGFloat  = 7.0 / 8.0

    private static func drawBlockElement(
        in ctx: CGContext, x: CGFloat, y: CGFloat, w: CGFloat, h: CGFloat,
        fg: NSColor, cp: UInt32
    ) -> Bool {
        // Block elements are pure axis-aligned rect fills. Cairo's
        // default antialiasing softened block edges by a fraction of
        // a pixel — Core Graphics has the same issue under some
        // transforms; turning AA off ensures adjacent cells (the
        // opencode wordmark in particular) abut with no visible
        // seam. Box-drawing curves and diagonals keep the default AA
        // so they don't go jaggy.
        ctx.saveGState()
        ctx.setShouldAntialias(false)
        setFill(ctx, fg)
        defer { ctx.restoreGState() }

        switch cp {
        case 0x2580:
            alignedBlock(ctx, x, y, w, h, .center, .top, 1.0, fHalf)
        case 0x2581:
            alignedBlock(ctx, x, y, w, h, .center, .bottom, 1.0, fEighth)
        case 0x2582:
            alignedBlock(ctx, x, y, w, h, .center, .bottom, 1.0, fQuarter)
        case 0x2583:
            alignedBlock(ctx, x, y, w, h, .center, .bottom, 1.0, f3Eighths)
        case 0x2584:
            alignedBlock(ctx, x, y, w, h, .center, .bottom, 1.0, fHalf)
        case 0x2585:
            alignedBlock(ctx, x, y, w, h, .center, .bottom, 1.0, f5Eighths)
        case 0x2586:
            alignedBlock(ctx, x, y, w, h, .center, .bottom, 1.0, f3Quarters)
        case 0x2587:
            alignedBlock(ctx, x, y, w, h, .center, .bottom, 1.0, f7Eighths)
        case 0x2588:
            fillRect(ctx, x, y, w, h)
        case 0x2589:
            alignedBlock(ctx, x, y, w, h, .left, .middle, f7Eighths, 1.0)
        case 0x258A:
            alignedBlock(ctx, x, y, w, h, .left, .middle, f3Quarters, 1.0)
        case 0x258B:
            alignedBlock(ctx, x, y, w, h, .left, .middle, f5Eighths, 1.0)
        case 0x258C:
            alignedBlock(ctx, x, y, w, h, .left, .middle, fHalf, 1.0)
        case 0x258D:
            alignedBlock(ctx, x, y, w, h, .left, .middle, f3Eighths, 1.0)
        case 0x258E:
            alignedBlock(ctx, x, y, w, h, .left, .middle, fQuarter, 1.0)
        case 0x258F:
            alignedBlock(ctx, x, y, w, h, .left, .middle, fEighth, 1.0)
        case 0x2590:
            alignedBlock(ctx, x, y, w, h, .right, .middle, fHalf, 1.0)
        case 0x2591, 0x2592, 0x2593:
            let alphas: [CGFloat] = [0.25, 0.5, 0.75]
            let alpha = alphas[Int(cp - 0x2591)]
            if let srgb = fg.usingColorSpace(.sRGB) {
                ctx.setFillColor(
                    red: srgb.redComponent,
                    green: srgb.greenComponent,
                    blue: srgb.blueComponent,
                    alpha: alpha
                )
            }
            fillRect(ctx, x, y, w, h)
        case 0x2594:
            alignedBlock(ctx, x, y, w, h, .center, .top, 1.0, fEighth)
        case 0x2595:
            alignedBlock(ctx, x, y, w, h, .right, .middle, fEighth, 1.0)
        case 0x2596:
            drawQuads(ctx, x, y, w, h, tl: false, tr: false, bl: true, br: false)
        case 0x2597:
            drawQuads(ctx, x, y, w, h, tl: false, tr: false, bl: false, br: true)
        case 0x2598:
            drawQuads(ctx, x, y, w, h, tl: true, tr: false, bl: false, br: false)
        case 0x2599:
            drawQuads(ctx, x, y, w, h, tl: true, tr: false, bl: true, br: true)
        case 0x259A:
            drawQuads(ctx, x, y, w, h, tl: true, tr: false, bl: false, br: true)
        case 0x259B:
            drawQuads(ctx, x, y, w, h, tl: true, tr: true, bl: true, br: false)
        case 0x259C:
            drawQuads(ctx, x, y, w, h, tl: true, tr: true, bl: false, br: true)
        case 0x259D:
            drawQuads(ctx, x, y, w, h, tl: false, tr: true, bl: false, br: false)
        case 0x259E:
            drawQuads(ctx, x, y, w, h, tl: false, tr: true, bl: true, br: false)
        case 0x259F:
            drawQuads(ctx, x, y, w, h, tl: false, tr: true, bl: true, br: true)
        default:
            return false
        }
        return true
    }

    private static func alignedBlock(
        _ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat,
        _ ha: HAlign, _ va: VAlign, _ fw: CGFloat, _ fh: CGFloat
    ) {
        let rw = (w * fw).rounded()
        let rh = (h * fh).rounded()
        let ox: CGFloat
        switch ha {
        case .left:   ox = 0
        case .center: ox = floor((w - rw) / 2)
        case .right:  ox = w - rw
        }
        let oy: CGFloat
        switch va {
        case .top:    oy = 0
        case .middle: oy = floor((h - rh) / 2)
        case .bottom: oy = h - rh
        }
        fillRect(ctx, x + ox, y + oy, rw, rh)
    }

    private static func drawQuads(
        _ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat,
        tl: Bool, tr: Bool, bl: Bool, br: Bool
    ) {
        let halfW = (w / 2).rounded()
        let halfH = (h / 2).rounded()
        if tl { fillRect(ctx, x, y, halfW, halfH) }
        if tr { fillRect(ctx, x + halfW, y, w - halfW, halfH) }
        if bl { fillRect(ctx, x, y + halfH, halfW, h - halfH) }
        if br { fillRect(ctx, x + halfW, y + halfH, w - halfW, h - halfH) }
    }

    private static func fillRect(_ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) {
        ctx.fill(CGRect(x: x, y: y, width: w, height: h))
    }

    // MARK: - Layer 2: Box drawing (U+2500–U+257F)

    private enum LineStyle { case none, light, heavy, double }

    private struct Lines4 {
        var up: LineStyle = .none
        var right: LineStyle = .none
        var down: LineStyle = .none
        var left: LineStyle = .none

        // Convenience init that accepts the four directions in any
        // order — Swift's memberwise init would force callers to
        // keep declaration order, which is unreadable when only
        // one or two directions are set (the `0x2500..U+2503`
        // glyphs each pick a different combination).
        init(
            up: LineStyle = .none,
            right: LineStyle = .none,
            down: LineStyle = .none,
            left: LineStyle = .none
        ) {
            self.up = up
            self.right = right
            self.down = down
            self.left = left
        }
    }

    private enum Corner { case tl, tr, bl, br }

    private static func drawBoxGlyph(
        in ctx: CGContext, x: CGFloat, y: CGFloat, w: CGFloat, h: CGFloat,
        fg: NSColor, cp: UInt32
    ) -> Bool {
        setFill(ctx, fg)
        setStroke(ctx, fg)
        switch cp {
        // --- simple horizontal/vertical lines ---
        case 0x2500: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, left: .light))
        case 0x2501: drawBoxLines(ctx, x, y, w, h, Lines4(right: .heavy, left: .heavy))
        case 0x2502: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, down: .light))
        case 0x2503: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, down: .heavy))

        // --- dashed (3-count) ---
        case 0x2504: drawHDash(ctx, x, y, w, h, count: 3, style: .light)
        case 0x2505: drawHDash(ctx, x, y, w, h, count: 3, style: .heavy)
        case 0x2506: drawVDash(ctx, x, y, w, h, count: 3, style: .light)
        case 0x2507: drawVDash(ctx, x, y, w, h, count: 3, style: .heavy)
        // (4-count)
        case 0x2508: drawHDash(ctx, x, y, w, h, count: 4, style: .light)
        case 0x2509: drawHDash(ctx, x, y, w, h, count: 4, style: .heavy)
        case 0x250A: drawVDash(ctx, x, y, w, h, count: 4, style: .light)
        case 0x250B: drawVDash(ctx, x, y, w, h, count: 4, style: .heavy)

        // --- single-line corners (light/heavy mixes) ---
        case 0x250C: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, down: .light))
        case 0x250D: drawBoxLines(ctx, x, y, w, h, Lines4(right: .heavy, down: .light))
        case 0x250E: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, down: .heavy))
        case 0x250F: drawBoxLines(ctx, x, y, w, h, Lines4(right: .heavy, down: .heavy))
        case 0x2510: drawBoxLines(ctx, x, y, w, h, Lines4(down: .light, left: .light))
        case 0x2511: drawBoxLines(ctx, x, y, w, h, Lines4(down: .light, left: .heavy))
        case 0x2512: drawBoxLines(ctx, x, y, w, h, Lines4(down: .heavy, left: .light))
        case 0x2513: drawBoxLines(ctx, x, y, w, h, Lines4(down: .heavy, left: .heavy))
        case 0x2514: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .light))
        case 0x2515: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .heavy))
        case 0x2516: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .light))
        case 0x2517: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .heavy))
        case 0x2518: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, left: .light))
        case 0x2519: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, left: .heavy))
        case 0x251A: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, left: .light))
        case 0x251B: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, left: .heavy))

        // --- T-junctions, right side (├ family) ---
        case 0x251C: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .light, down: .light))
        case 0x251D: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .heavy, down: .light))
        case 0x251E: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .light, down: .light))
        case 0x251F: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .light, down: .heavy))
        case 0x2520: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .light, down: .heavy))
        case 0x2521: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .heavy, down: .light))
        case 0x2522: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .heavy, down: .heavy))
        case 0x2523: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .heavy, down: .heavy))

        // --- T-junctions, left side (┤ family) ---
        case 0x2524: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, down: .light, left: .light))
        case 0x2525: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, down: .light, left: .heavy))
        case 0x2526: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, down: .light, left: .light))
        case 0x2527: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, down: .heavy, left: .light))
        case 0x2528: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, down: .heavy, left: .light))
        case 0x2529: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, down: .light, left: .heavy))
        case 0x252A: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, down: .heavy, left: .heavy))
        case 0x252B: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, down: .heavy, left: .heavy))

        // --- T-junctions, down (┬ family) ---
        case 0x252C: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, down: .light, left: .light))
        case 0x252D: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, down: .light, left: .heavy))
        case 0x252E: drawBoxLines(ctx, x, y, w, h, Lines4(right: .heavy, down: .light, left: .light))
        case 0x252F: drawBoxLines(ctx, x, y, w, h, Lines4(right: .heavy, down: .light, left: .heavy))
        case 0x2530: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, down: .heavy, left: .light))
        case 0x2531: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, down: .heavy, left: .heavy))
        case 0x2532: drawBoxLines(ctx, x, y, w, h, Lines4(right: .heavy, down: .heavy, left: .light))
        case 0x2533: drawBoxLines(ctx, x, y, w, h, Lines4(right: .heavy, down: .heavy, left: .heavy))

        // --- T-junctions, up (┴ family) ---
        case 0x2534: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .light, left: .light))
        case 0x2535: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .light, left: .heavy))
        case 0x2536: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .heavy, left: .light))
        case 0x2537: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .heavy, left: .heavy))
        case 0x2538: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .light, left: .light))
        case 0x2539: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .light, left: .heavy))
        case 0x253A: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .heavy, left: .light))
        case 0x253B: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .heavy, left: .heavy))

        // --- crosses (┼ family) ---
        case 0x253C: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .light, down: .light, left: .light))
        case 0x253D: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .light, down: .light, left: .heavy))
        case 0x253E: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .heavy, down: .light, left: .light))
        case 0x253F: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .heavy, down: .light, left: .heavy))
        case 0x2540: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .light, down: .light, left: .light))
        case 0x2541: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .light, down: .heavy, left: .light))
        case 0x2542: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .light, down: .heavy, left: .light))
        case 0x2543: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .light, down: .light, left: .heavy))
        case 0x2544: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .heavy, down: .light, left: .light))
        case 0x2545: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .light, down: .heavy, left: .heavy))
        case 0x2546: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .heavy, down: .heavy, left: .light))
        case 0x2547: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .heavy, down: .light, left: .heavy))
        case 0x2548: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .heavy, down: .heavy, left: .heavy))
        case 0x2549: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .light, down: .heavy, left: .heavy))
        case 0x254A: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .heavy, down: .heavy, left: .light))
        case 0x254B: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, right: .heavy, down: .heavy, left: .heavy))

        // --- 2-count dashed ---
        case 0x254C: drawHDash(ctx, x, y, w, h, count: 2, style: .light)
        case 0x254D: drawHDash(ctx, x, y, w, h, count: 2, style: .heavy)
        case 0x254E: drawVDash(ctx, x, y, w, h, count: 2, style: .light)
        case 0x254F: drawVDash(ctx, x, y, w, h, count: 2, style: .heavy)

        // --- double-line variants ---
        case 0x2550: drawBoxLines(ctx, x, y, w, h, Lines4(right: .double, left: .double))
        case 0x2551: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, down: .double))
        case 0x2552: drawBoxLines(ctx, x, y, w, h, Lines4(right: .double, down: .light))
        case 0x2553: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, down: .double))
        case 0x2554: drawBoxLines(ctx, x, y, w, h, Lines4(right: .double, down: .double))
        case 0x2555: drawBoxLines(ctx, x, y, w, h, Lines4(down: .light, left: .double))
        case 0x2556: drawBoxLines(ctx, x, y, w, h, Lines4(down: .double, left: .light))
        case 0x2557: drawBoxLines(ctx, x, y, w, h, Lines4(down: .double, left: .double))
        case 0x2558: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .double))
        case 0x2559: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, right: .light))
        case 0x255A: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, right: .double))
        case 0x255B: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, left: .double))
        case 0x255C: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, left: .light))
        case 0x255D: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, left: .double))
        case 0x255E: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .double, down: .light))
        case 0x255F: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, right: .light, down: .double))
        case 0x2560: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, right: .double, down: .double))
        case 0x2561: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, down: .light, left: .double))
        case 0x2562: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, down: .double, left: .light))
        case 0x2563: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, down: .double, left: .double))
        case 0x2564: drawBoxLines(ctx, x, y, w, h, Lines4(right: .double, down: .light, left: .double))
        case 0x2565: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, down: .double, left: .light))
        case 0x2566: drawBoxLines(ctx, x, y, w, h, Lines4(right: .double, down: .double, left: .double))
        case 0x2567: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .double, left: .double))
        case 0x2568: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, right: .light, left: .light))
        case 0x2569: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, right: .double, left: .double))
        case 0x256A: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, right: .double, down: .light, left: .double))
        case 0x256B: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, right: .light, down: .double, left: .light))
        case 0x256C: drawBoxLines(ctx, x, y, w, h, Lines4(up: .double, right: .double, down: .double, left: .double))

        // --- rounded corners ---
        case 0x256D: drawArc(ctx, x, y, w, h, .br)
        case 0x256E: drawArc(ctx, x, y, w, h, .bl)
        case 0x256F: drawArc(ctx, x, y, w, h, .tl)
        case 0x2570: drawArc(ctx, x, y, w, h, .tr)

        // --- diagonals ---
        case 0x2571: drawDiag(ctx, x, y, w, h, urToLl: true,  ulToLr: false)
        case 0x2572: drawDiag(ctx, x, y, w, h, urToLl: false, ulToLr: true)
        case 0x2573: drawDiag(ctx, x, y, w, h, urToLl: true,  ulToLr: true)

        // --- half-edges (light) ---
        case 0x2574: drawBoxLines(ctx, x, y, w, h, Lines4(left: .light))
        case 0x2575: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light))
        case 0x2576: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light))
        case 0x2577: drawBoxLines(ctx, x, y, w, h, Lines4(down: .light))

        // --- half-edges (heavy) ---
        case 0x2578: drawBoxLines(ctx, x, y, w, h, Lines4(left: .heavy))
        case 0x2579: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy))
        case 0x257A: drawBoxLines(ctx, x, y, w, h, Lines4(right: .heavy))
        case 0x257B: drawBoxLines(ctx, x, y, w, h, Lines4(down: .heavy))

        // --- mixed-weight half-edges ---
        case 0x257C: drawBoxLines(ctx, x, y, w, h, Lines4(right: .heavy, left: .light))
        case 0x257D: drawBoxLines(ctx, x, y, w, h, Lines4(up: .light, down: .heavy))
        case 0x257E: drawBoxLines(ctx, x, y, w, h, Lines4(right: .light, left: .heavy))
        case 0x257F: drawBoxLines(ctx, x, y, w, h, Lines4(up: .heavy, down: .light))

        default: return false
        }
        return true
    }

    /// "Light" stroke width derived from cell height. Roughly 7% of
    /// cell height, min 1px — mirrors the heuristic in
    /// `cmd/roost/sprite.go::boxThickness`.
    private static func boxThickness(_ h: CGFloat) -> CGFloat {
        let t = (h / 14).rounded()
        return t < 1 ? 1 : t
    }

    /// Paint up to four cardinal-direction strokes that meet at the
    /// cell center with correct heavy/double junction precedence.
    /// Direct port of `box.zig::linesChar` (lines 399-637).
    private static func drawBoxLines(
        _ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat,
        _ ln: Lines4
    ) {
        let light = boxThickness(h)
        let heavy = 2 * light

        let hLightTop  = floor((h - light) / 2)
        let hLightBot  = hLightTop + light
        let hHeavyTop  = floor((h - heavy) / 2)
        let hHeavyBot  = hHeavyTop + heavy
        let hDoubleTop = hLightTop - light
        let hDoubleBot = hLightBot + light

        let vLightLeft  = floor((w - light) / 2)
        let vLightRight = vLightLeft + light
        let vHeavyLeft  = floor((w - heavy) / 2)
        let vHeavyRight = vHeavyLeft + heavy
        let vDoubleLeft  = vLightLeft - light
        let vDoubleRight = vLightRight + light

        let upBottom = pickJunction(ln.left, ln.right, ln.down, ln.up,
                                    hHeavyBot, hDoubleBot, hLightBot, hLightTop)
        let downTop  = pickJunction(ln.left, ln.right, ln.up, ln.down,
                                    hHeavyTop, hDoubleTop, hLightTop, hLightBot)
        let leftRight = pickJunction(ln.up, ln.down, ln.right, ln.left,
                                     vHeavyRight, vDoubleRight, vLightRight, vLightLeft)
        let rightLeft = pickJunction(ln.up, ln.down, ln.left, ln.right,
                                     vHeavyLeft, vDoubleLeft, vLightLeft, vLightRight)

        // UP stroke
        switch ln.up {
        case .none: break
        case .light: boxRect(ctx, x, y, vLightLeft, 0, vLightRight, upBottom)
        case .heavy: boxRect(ctx, x, y, vHeavyLeft, 0, vHeavyRight, upBottom)
        case .double:
            let leftBot  = (ln.left  == .double) ? hLightTop : upBottom
            let rightBot = (ln.right == .double) ? hLightTop : upBottom
            boxRect(ctx, x, y, vDoubleLeft, 0, vLightLeft, leftBot)
            boxRect(ctx, x, y, vLightRight, 0, vDoubleRight, rightBot)
        }

        // RIGHT stroke
        switch ln.right {
        case .none: break
        case .light: boxRect(ctx, x, y, rightLeft, hLightTop, w, hLightBot)
        case .heavy: boxRect(ctx, x, y, rightLeft, hHeavyTop, w, hHeavyBot)
        case .double:
            let topLeft = (ln.up   == .double) ? vLightRight : rightLeft
            let botLeft = (ln.down == .double) ? vLightRight : rightLeft
            boxRect(ctx, x, y, topLeft, hDoubleTop, w, hLightTop)
            boxRect(ctx, x, y, botLeft, hLightBot, w, hDoubleBot)
        }

        // DOWN stroke
        switch ln.down {
        case .none: break
        case .light: boxRect(ctx, x, y, vLightLeft, downTop, vLightRight, h)
        case .heavy: boxRect(ctx, x, y, vHeavyLeft, downTop, vHeavyRight, h)
        case .double:
            let leftTop  = (ln.left  == .double) ? hLightBot : downTop
            let rightTop = (ln.right == .double) ? hLightBot : downTop
            boxRect(ctx, x, y, vDoubleLeft, leftTop, vLightLeft, h)
            boxRect(ctx, x, y, vLightRight, rightTop, vDoubleRight, h)
        }

        // LEFT stroke
        switch ln.left {
        case .none: break
        case .light: boxRect(ctx, x, y, 0, hLightTop, leftRight, hLightBot)
        case .heavy: boxRect(ctx, x, y, 0, hHeavyTop, leftRight, hHeavyBot)
        case .double:
            let topRight = (ln.up   == .double) ? vLightLeft : leftRight
            let botRight = (ln.down == .double) ? vLightLeft : leftRight
            boxRect(ctx, x, y, 0, hDoubleTop, topRight, hLightTop)
            boxRect(ctx, x, y, 0, hLightBot, botRight, hDoubleBot)
        }
    }

    /// Perpendicular-stroke termination logic from `linesChar`. Same
    /// rules as the Rust `pick_junction`; see the Go file for the
    /// original prose explanation.
    private static func pickJunction(
        _ perp1: LineStyle, _ perp2: LineStyle,
        _ parallel: LineStyle, _ this: LineStyle,
        _ heavyEdge: CGFloat, _ doubleEdge: CGFloat,
        _ lightEdgeFar: CGFloat, _ lightEdgeNear: CGFloat
    ) -> CGFloat {
        if perp1 == .heavy || perp2 == .heavy { return heavyEdge }
        if perp1 != perp2 || parallel == this {
            if perp1 == .double || perp2 == .double { return doubleEdge }
            return lightEdgeFar
        }
        if perp1 == .none && perp2 == .none { return lightEdgeFar }
        return lightEdgeNear
    }

    private static func boxRect(
        _ ctx: CGContext, _ x: CGFloat, _ y: CGFloat,
        _ l: CGFloat, _ t: CGFloat, _ r: CGFloat, _ b: CGFloat
    ) {
        if r <= l || b <= t { return }
        ctx.fill(CGRect(x: x + l, y: y + t, width: r - l, height: b - t))
    }

    private static func drawArc(
        _ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat,
        _ c: Corner
    ) {
        let t = boxThickness(h)
        let cx = floor((w - t) / 2) + t / 2
        let cy = floor((h - t) / 2) + t / 2
        let r = min(w, h) / 2
        let s: CGFloat = 0.25

        ctx.beginPath()
        switch c {
        case .tl: // ╯ — strokes go up + left
            ctx.move(to: CGPoint(x: x + cx, y: y))
            ctx.addLine(to: CGPoint(x: x + cx, y: y + cy - r))
            ctx.addCurve(
                to: CGPoint(x: x + cx - r, y: y + cy),
                control1: CGPoint(x: x + cx, y: y + cy - s * r),
                control2: CGPoint(x: x + cx - s * r, y: y + cy)
            )
            ctx.addLine(to: CGPoint(x: x, y: y + cy))
        case .tr: // ╰ — up + right
            ctx.move(to: CGPoint(x: x + cx, y: y))
            ctx.addLine(to: CGPoint(x: x + cx, y: y + cy - r))
            ctx.addCurve(
                to: CGPoint(x: x + cx + r, y: y + cy),
                control1: CGPoint(x: x + cx, y: y + cy - s * r),
                control2: CGPoint(x: x + cx + s * r, y: y + cy)
            )
            ctx.addLine(to: CGPoint(x: x + w, y: y + cy))
        case .bl: // ╮ — down + left
            ctx.move(to: CGPoint(x: x + cx, y: y + h))
            ctx.addLine(to: CGPoint(x: x + cx, y: y + cy + r))
            ctx.addCurve(
                to: CGPoint(x: x + cx - r, y: y + cy),
                control1: CGPoint(x: x + cx, y: y + cy + s * r),
                control2: CGPoint(x: x + cx - s * r, y: y + cy)
            )
            ctx.addLine(to: CGPoint(x: x, y: y + cy))
        case .br: // ╭ — down + right
            ctx.move(to: CGPoint(x: x + cx, y: y + h))
            ctx.addLine(to: CGPoint(x: x + cx, y: y + cy + r))
            ctx.addCurve(
                to: CGPoint(x: x + cx + r, y: y + cy),
                control1: CGPoint(x: x + cx, y: y + cy + s * r),
                control2: CGPoint(x: x + cx + s * r, y: y + cy)
            )
            ctx.addLine(to: CGPoint(x: x + w, y: y + cy))
        }
        ctx.setLineCap(.butt)
        ctx.setLineWidth(t)
        ctx.strokePath()
    }

    private static func drawDiag(
        _ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat,
        urToLl: Bool, ulToLr: Bool
    ) {
        let t = boxThickness(h)
        let slopeX = min(1.0, w / h)
        let slopeY = min(1.0, h / w)

        ctx.setLineCap(.butt)
        ctx.setLineWidth(t)
        if urToLl {
            ctx.beginPath()
            ctx.move(to: CGPoint(x: x + w + 0.5 * slopeX, y: y - 0.5 * slopeY))
            ctx.addLine(to: CGPoint(x: x - 0.5 * slopeX, y: y + h + 0.5 * slopeY))
            ctx.strokePath()
        }
        if ulToLr {
            ctx.beginPath()
            ctx.move(to: CGPoint(x: x - 0.5 * slopeX, y: y - 0.5 * slopeY))
            ctx.addLine(to: CGPoint(x: x + w + 0.5 * slopeX, y: y + h + 0.5 * slopeY))
            ctx.strokePath()
        }
    }

    private static func drawHDash(
        _ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat,
        count: Int, style: LineStyle
    ) {
        var thick = boxThickness(h)
        if style == .heavy { thick *= 2 }
        var desiredGap = thick
        if style == .light && desiredGap < 4 { desiredGap = 4 }

        let wi = Int(w)
        if wi < count * 2 {
            drawBoxLines(ctx, x, y, w, h, Lines4(right: style, left: style))
            return
        }

        var gap = Int(desiredGap)
        let maxGap = wi / (2 * count)
        if gap > maxGap { gap = maxGap }
        let totalGap = gap * count
        let totalDash = wi - totalGap
        let dash = totalDash / count
        var extra = totalDash % count

        let yi = floor((h - thick) / 2)
        var xi = CGFloat(gap / 2)
        for _ in 0..<count {
            var dw = dash
            if extra > 0 { dw += 1; extra -= 1 }
            boxRect(ctx, x, y, xi, yi, xi + CGFloat(dw), yi + thick)
            xi += CGFloat(dw + gap)
        }
    }

    private static func drawVDash(
        _ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat,
        count: Int, style: LineStyle
    ) {
        var thick = boxThickness(h)
        if style == .heavy { thick *= 2 }
        var desiredGap = thick
        if style == .light && desiredGap < 4 { desiredGap = 4 }

        let hi = Int(h)
        if hi < count * 2 {
            drawBoxLines(ctx, x, y, w, h, Lines4(up: style, down: style))
            return
        }

        var gap = Int(desiredGap)
        let maxGap = hi / (2 * count)
        if gap > maxGap { gap = maxGap }
        let totalGap = gap * count
        let totalDash = hi - totalGap
        let dash = totalDash / count
        var extra = totalDash % count

        let xi = floor((w - thick) / 2)
        var yi = CGFloat(gap / 2)
        for _ in 0..<count {
            var dh = dash
            if extra > 0 { dh += 1; extra -= 1 }
            boxRect(ctx, x, y, xi, yi, xi + thick, yi + CGFloat(dh))
            yi += CGFloat(dh + gap)
        }
    }
}
