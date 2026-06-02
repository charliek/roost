// Standard xterm 256-color palette tests — companion to the Rust
// `theme::tests::standard_xterm_256_*` cases. The 6×6×6 cube (16–231)
// and 24-step grayscale ramp (232–255) must be populated so 256-color
// content (`SGR 48;5;N`) renders correctly. Regression: opencode over
// SSH (256-color, COLORTERM unset) backgrounds with `48;5;232` (#080808);
// pre-fix the palette default was a flat gray placeholder for 16–255 and
// every 256-color cell rendered the same wrong color.

import AppKit
import Testing

@testable import Roost

private func rgb255(_ c: NSColor) -> (Int, Int, Int) {
    let s = c.usingColorSpace(.sRGB)!
    return (
        Int((s.redComponent * 255).rounded()),
        Int((s.greenComponent * 255).rounded()),
        Int((s.blueComponent * 255).rounded())
    )
}

@Test
func standardXterm256_cubeAndGrayscale() {
    let p = Theme.standardXterm256Palette()
    #expect(p.count == 256)
    // 6×6×6 cube corners.
    #expect(rgb255(p[16]) == (0, 0, 0))
    #expect(rgb255(p[231]) == (255, 255, 255))
    #expect(rgb255(p[21]) == (0, 0, 255))
    #expect(rgb255(p[196]) == (255, 0, 0))
    #expect(rgb255(p[46]) == (0, 255, 0))
    // Grayscale ramp ends.
    #expect(rgb255(p[232]) == (8, 8, 8))
    #expect(rgb255(p[255]) == (238, 238, 238))
    // ANSI base.
    #expect(rgb255(p[15]) == (255, 255, 255))
}

@Test
func fallbackThemePopulates256ColorCube() {
    let p = Theme.fallback.palette
    #expect(rgb255(p[232]) == (8, 8, 8), "gray 232 must be #080808, not a placeholder")
    #expect(rgb255(p[196]) == (255, 0, 0), "cube red 196")
    #expect(rgb255(p[21]) == (0, 0, 255), "cube blue 21")
}

@Test @MainActor
func bundledThemesKeep256ColorCube() {
    // Theme files only define the 16 ANSI slots; the cube/ramp must
    // survive theme loading (the base is inherited, then overridden).
    for name in Theme.bundledNames() {
        let p = Theme.loadBundled(name: name).palette
        #expect(rgb255(p[232]) == (8, 8, 8), "\(name): gray 232")
        #expect(rgb255(p[196]) == (255, 0, 0), "\(name): cube red 196")
    }
}
