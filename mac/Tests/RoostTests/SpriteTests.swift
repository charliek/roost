// Pixel-assertion suite mirroring `crates/roost-linux/src/sprite.rs::tests`
// and the legacy `cmd/roost/sprite_test.go`. Renders each glyph into a
// CGBitmapContext and pokes at raw bytes to verify fills land in the
// right places. The OpenCode-logo regression is `blockTilingNoGap` —
// two adjacent █ cells must abut with no seam.
//
// When the Rust suite grows a case, mirror it here in the same order so
// both UIs stay at behavioral parity.

import AppKit
import CoreGraphics
import Testing

@testable import Roost

private struct Bitmap {
    let data: [UInt8]
    let stride: Int
    let width: Int
    let height: Int
}

/// Render `cp` into a fresh `w*h` premultiplied-RGBA bitmap with the
/// foreground color white-on-clear. Returns the raw bytes + per-row
/// stride for spot-checking pixels.
@MainActor
private func render(_ cp: UInt32, _ w: Int, _ h: Int) -> (bitmap: Bitmap, handled: Bool) {
    let bytesPerRow = w * 4
    var buf = [UInt8](repeating: 0, count: bytesPerRow * h)
    let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!
    let bitmapInfo = CGImageAlphaInfo.premultipliedLast.rawValue
    let ctx = buf.withUnsafeMutableBytes { ptr -> CGContext in
        CGContext(
            data: ptr.baseAddress,
            width: w,
            height: h,
            bitsPerComponent: 8,
            bytesPerRow: bytesPerRow,
            space: colorSpace,
            bitmapInfo: bitmapInfo
        )!
    }
    // CGBitmapContextCreate's default coordinate space is Y-up
    // (origin at bottom-left). TerminalView's draw context is
    // Y-down (isFlipped=true), and Sprite.draw is written to that
    // convention. Flip Y here so bitmap row 0 is "top" — matching
    // both Sprite's expectations and how pixelOn(_, _, _, _) reads
    // the buffer.
    ctx.translateBy(x: 0, y: CGFloat(h))
    ctx.scaleBy(x: 1, y: -1)
    let handled = Sprite.draw(
        in: ctx,
        x: 0, y: 0,
        w: CGFloat(w), h: CGFloat(h),
        fg: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        codepoint: cp
    )
    // Force any deferred drawing to flush before we read the buffer.
    ctx.flush()
    return (Bitmap(data: buf, stride: bytesPerRow, width: w, height: h), handled)
}

private func pixelOn(_ b: Bitmap, _ x: Int, _ y: Int) -> Bool {
    let off = y * b.stride + x * 4
    return b.data[off] != 0 || b.data[off + 1] != 0 || b.data[off + 2] != 0
}

private func pixelsOnRect(_ b: Bitmap, _ x0: Int, _ y0: Int, _ x1: Int, _ y1: Int) -> Int {
    var n = 0
    for y in y0..<y1 {
        for x in x0..<x1 {
            if pixelOn(b, x, y) { n += 1 }
        }
    }
    return n
}

private func rectFilled(_ b: Bitmap, _ x0: Int, _ y0: Int, _ x1: Int, _ y1: Int, _ msg: String) {
    for y in y0..<y1 {
        for x in x0..<x1 {
            #expect(pixelOn(b, x, y), "\(msg): expected on at (\(x),\(y)), got off")
        }
    }
}

private func rectEmpty(_ b: Bitmap, _ x0: Int, _ y0: Int, _ x1: Int, _ y1: Int, _ msg: String) {
    for y in y0..<y1 {
        for x in x0..<x1 {
            #expect(!pixelOn(b, x, y), "\(msg): expected off at (\(x),\(y)), got on")
        }
    }
}

// MARK: - Tests

@Test @MainActor
func sprite_dispatchSkipsNonGeometric() {
    for cp: UInt32 in [0x41, 0x20, 0x30, 0x24FF, 0x25A0, 0x2700] {
        let (_, handled) = render(cp, 8, 16)
        #expect(!handled, "U+\(String(cp, radix: 16, uppercase: true)) should not be handled")
    }
}

@Test @MainActor
func sprite_dispatchHandlesRanges() {
    for cp: UInt32 in [0x2500, 0x2580, 0x2588, 0x256D, 0x2571, 0x257F] {
        let (_, handled) = render(cp, 12, 24)
        #expect(handled, "U+\(String(cp, radix: 16, uppercase: true)) should be handled")
    }
}

@Test @MainActor
func sprite_fullBlockFillsCell() {
    let (bmp, _) = render(0x2588, 8, 16)
    rectFilled(bmp, 0, 0, 8, 16, "█")
}

@Test @MainActor
func sprite_upperHalfBlock() {
    let w = 10, h = 20
    let (bmp, _) = render(0x2580, w, h)
    rectFilled(bmp, 0, 0, w, h / 2, "▀ top half")
    rectEmpty(bmp, 0, h / 2, w, h, "▀ bottom half")
}

@Test @MainActor
func sprite_lowerHalfBlock() {
    let w = 10, h = 20
    let (bmp, _) = render(0x2584, w, h)
    rectEmpty(bmp, 0, 0, w, h / 2, "▄ top half")
    rectFilled(bmp, 0, h / 2, w, h, "▄ bottom half")
}

@Test @MainActor
func sprite_leftHalfBlock() {
    let w = 10, h = 20
    let (bmp, _) = render(0x258C, w, h)
    rectFilled(bmp, 0, 0, w / 2, h, "▌ left half")
    rectEmpty(bmp, w / 2, 0, w, h, "▌ right half")
}

@Test @MainActor
func sprite_rightHalfBlock() {
    let w = 10, h = 20
    let (bmp, _) = render(0x2590, w, h)
    rectEmpty(bmp, 0, 0, w / 2, h, "▐ left half")
    rectFilled(bmp, w / 2, 0, w, h, "▐ right half")
}

@Test @MainActor
func sprite_quadrantTL() {
    let w = 10, h = 20
    let (bmp, _) = render(0x2598, w, h)
    rectFilled(bmp, 0, 0, w / 2, h / 2, "▘ TL")
    rectEmpty(bmp, w / 2, 0, w, h / 2, "▘ TR")
    rectEmpty(bmp, 0, h / 2, w / 2, h, "▘ BL")
    rectEmpty(bmp, w / 2, h / 2, w, h, "▘ BR")
}

@Test @MainActor
func sprite_quadrantTRplusBL() {
    let w = 10, h = 20
    let (bmp, _) = render(0x259E, w, h)
    rectEmpty(bmp, 0, 0, w / 2, h / 2, "▞ TL")
    rectFilled(bmp, w / 2, 0, w, h / 2, "▞ TR")
    rectFilled(bmp, 0, h / 2, w / 2, h, "▞ BL")
    rectEmpty(bmp, w / 2, h / 2, w, h, "▞ BR")
}

@Test @MainActor
func sprite_horizontalLineReachesEdges() {
    let w = 12, h = 24
    let (bmp, _) = render(0x2500, w, h)
    #expect(pixelOn(bmp, 0, h / 2), "─ left edge")
    #expect(pixelOn(bmp, w - 1, h / 2), "─ right edge")
    rectEmpty(bmp, 0, 0, w, 1, "─ top row")
    rectEmpty(bmp, 0, h - 1, w, h, "─ bottom row")
}

@Test @MainActor
func sprite_verticalLineReachesEdges() {
    let w = 12, h = 24
    let (bmp, _) = render(0x2502, w, h)
    #expect(pixelOn(bmp, w / 2, 0), "│ top edge")
    #expect(pixelOn(bmp, w / 2, h - 1), "│ bottom edge")
    rectEmpty(bmp, 0, 0, 1, h, "│ left col")
    rectEmpty(bmp, w - 1, 0, w, h, "│ right col")
}

@Test @MainActor
func sprite_lightCrossReachesAllEdges() {
    let w = 14, h = 28
    let (bmp, _) = render(0x253C, w, h)
    #expect(pixelOn(bmp, 0, h / 2), "┼ left")
    #expect(pixelOn(bmp, w - 1, h / 2), "┼ right")
    #expect(pixelOn(bmp, w / 2, 0), "┼ top")
    #expect(pixelOn(bmp, w / 2, h - 1), "┼ bottom")
}

@Test @MainActor
func sprite_heavyCrossHasMorePixelsThanLight() {
    let w = 14, h = 28
    let (light, _) = render(0x253C, w, h)
    let (heavy, _) = render(0x254B, w, h)
    let lightOn = pixelsOnRect(light, 0, 0, w, h)
    let heavyOn = pixelsOnRect(heavy, 0, 0, w, h)
    #expect(heavyOn > lightOn, "expected ╋ to have more on-pixels than ┼ (heavy=\(heavyOn), light=\(lightOn))")
}

@Test @MainActor
func sprite_doubleHorizontalHasTwoRuns() {
    let w = 16, h = 32
    let (bmp, _) = render(0x2550, w, h)
    let col = w / 2
    var runs = 0
    var prev = false
    for y in 0..<h {
        let cur = pixelOn(bmp, col, y)
        if cur && !prev { runs += 1 }
        prev = cur
    }
    #expect(runs == 2, "═ expected 2 horizontal stroke runs in middle column, got \(runs)")
}

@Test @MainActor
func sprite_squareCornerTL() {
    let w = 14, h = 28
    let (bmp, _) = render(0x250C, w, h)
    #expect(pixelOn(bmp, w - 1, h / 2), "┌ right edge")
    #expect(pixelOn(bmp, w / 2, h - 1), "┌ bottom edge")
    rectEmpty(bmp, 0, 0, w, h / 2 - 2, "┌ no up stroke")
    rectEmpty(bmp, 0, 0, w / 2 - 2, h, "┌ no left stroke")
}

@Test @MainActor
func sprite_roundedCornerTL() {
    let w = 16, h = 32
    let (bmp, _) = render(0x256D, w, h)
    #expect(pixelOn(bmp, w - 1, h / 2), "╭ right edge")
    #expect(pixelOn(bmp, w / 2, h - 1), "╭ bottom edge")
    rectEmpty(bmp, 0, 0, w / 4, h / 4, "╭ corner interior empty")
}

@Test @MainActor
func sprite_diagonalURtoLL() {
    let w = 16, h = 32
    let (bmp, _) = render(0x2571, w, h)
    #expect(pixelsOnRect(bmp, w - 3, 0, w, 3) > 0, "╱ expected on-pixels near top-right")
    #expect(pixelsOnRect(bmp, w - 3, h - 3, w, h) == 0, "╱ expected no pixels near bottom-right")
    #expect(pixelsOnRect(bmp, 0, h - 3, 3, h) > 0, "╱ expected on-pixels near bottom-left")
}

@Test @MainActor
func sprite_diagonalCross() {
    let w = 16, h = 32
    let (bmp, _) = render(0x2573, w, h)
    let corners: [(Int, Int, Int, Int)] = [
        (0, 0, 3, 3),
        (w - 3, 0, w, 3),
        (0, h - 3, 3, h),
        (w - 3, h - 3, w, h),
    ]
    for c in corners {
        #expect(pixelsOnRect(bmp, c.0, c.1, c.2, c.3) > 0,
                "╳ expected on-pixels in corner \(c)")
    }
}

@Test @MainActor
func sprite_dashedHorizontalThreeSegments() {
    let w = 30, h = 16
    let (bmp, _) = render(0x2504, w, h)
    let colOn = { (x: Int) -> Bool in
        for y in (h / 2 - 2)...(h / 2 + 2) {
            if pixelOn(bmp, x, y) { return true }
        }
        return false
    }
    var runs = 0
    var prev = false
    for x in 0..<w {
        let cur = colOn(x)
        if cur && !prev { runs += 1 }
        prev = cur
    }
    #expect(runs == 3, "┄ expected 3 dash segments, got \(runs)")
}

/// THE regression test — opencode-logo seams. Two █ cells stacked
/// (or side-by-side) must abut without a gap row/column. Also
/// verifies ▄ above ▀ tiles cleanly across the cell boundary.
@Test @MainActor
func sprite_blockTilingNoGap() {
    let w = 8, cellH = 20

    // 2x2 grid of █: every pixel in the union should be on.
    let bytesPerRow = (w * 2) * 4
    var buf = [UInt8](repeating: 0, count: bytesPerRow * (cellH * 2))
    let cs = CGColorSpace(name: CGColorSpace.sRGB)!
    let ctx = buf.withUnsafeMutableBytes { ptr -> CGContext in
        CGContext(
            data: ptr.baseAddress,
            width: w * 2, height: cellH * 2,
            bitsPerComponent: 8, bytesPerRow: bytesPerRow,
            space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        )!
    }
    ctx.translateBy(x: 0, y: CGFloat(cellH * 2))
    ctx.scaleBy(x: 1, y: -1)
    for row in 0..<2 {
        for col in 0..<2 {
            let ok = Sprite.draw(
                in: ctx,
                x: CGFloat(col * w), y: CGFloat(row * cellH),
                w: CGFloat(w), h: CGFloat(cellH),
                fg: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
                codepoint: 0x2588
            )
            #expect(ok, "█ not handled")
        }
    }
    ctx.flush()
    let bmp = Bitmap(data: buf, stride: bytesPerRow, width: w * 2, height: cellH * 2)
    rectFilled(bmp, 0, 0, w * 2, cellH * 2, "█x4 grid")

    // ▄ above ▀ in the same column → bottom row of cell 0 + top row
    // of cell 1 must both be on (the two halves meet at the boundary).
    let bpr2 = w * 4
    var buf2 = [UInt8](repeating: 0, count: bpr2 * (cellH * 2))
    let ctx2 = buf2.withUnsafeMutableBytes { ptr -> CGContext in
        CGContext(
            data: ptr.baseAddress,
            width: w, height: cellH * 2,
            bitsPerComponent: 8, bytesPerRow: bpr2,
            space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        )!
    }
    ctx2.translateBy(x: 0, y: CGFloat(cellH * 2))
    ctx2.scaleBy(x: 1, y: -1)
    #expect(Sprite.draw(
        in: ctx2, x: 0, y: 0, w: CGFloat(w), h: CGFloat(cellH),
        fg: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        codepoint: 0x2584
    ))
    #expect(Sprite.draw(
        in: ctx2, x: 0, y: CGFloat(cellH), w: CGFloat(w), h: CGFloat(cellH),
        fg: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        codepoint: 0x2580
    ))
    ctx2.flush()
    let bmp2 = Bitmap(data: buf2, stride: bpr2, width: w, height: cellH * 2)
    let col = w / 2
    #expect(pixelOn(bmp2, col, cellH - 1), "▄: last row of cell 0 should be on (boundary)")
    #expect(pixelOn(bmp2, col, cellH), "▀: first row of cell 1 should be on (boundary)")
}
