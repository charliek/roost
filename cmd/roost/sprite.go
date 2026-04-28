package main

import (
	"math"

	"github.com/diamondburned/gotk4/pkg/cairo"

	"github.com/charliek/roost/internal/ghostty"
)

// Custom geometric renderer for Unicode box-drawing (U+2500–U+257F) and
// block-element (U+2580–U+259F) glyphs. These ranges must align pixel-
// perfectly across cell boundaries — Pango's font glyphs do not, which
// produces visible seams in TUI chrome (Codex header box, OpenCode logo).
//
// Ported from Ghostty's font/sprite/draw/{block,box}.zig. We draw with
// Cairo rect/path primitives instead of writing into a Canvas surface.

// drawCellSprite renders the codepoint as exact geometry. Returns true if
// the codepoint is in a supported range and was drawn; false if the caller
// should fall back to Pango.
func drawCellSprite(cr *cairo.Context, x, y, w, h float64, fg ghostty.ColorRGB, cp rune) bool {
	switch {
	case cp >= 0x2580 && cp <= 0x259F:
		return drawBlockElement(cr, x, y, w, h, fg, cp)
	case cp >= 0x2500 && cp <= 0x257F:
		return drawBoxGlyph(cr, x, y, w, h, fg, cp)
	}
	return false
}

// -----------------------------------------------------------------------------
// Layer 1: Block elements (U+2580–U+259F)
// -----------------------------------------------------------------------------

type halign uint8

const (
	hLeft halign = iota
	hCenter
	hRight
)

type valign uint8

const (
	vTop valign = iota
	vMiddle
	vBottom
)

const (
	fEighth    = 1.0 / 8
	fQuarter   = 1.0 / 4
	f3Eighths  = 3.0 / 8
	fHalf      = 1.0 / 2
	f5Eighths  = 5.0 / 8
	f3Quarters = 3.0 / 4
	f7Eighths  = 7.0 / 8
)

func drawBlockElement(cr *cairo.Context, x, y, w, h float64, fg ghostty.ColorRGB, cp rune) bool {
	// Block elements are pure axis-aligned rect fills. Cairo's default
	// antialiasing softens edges by a fraction of a pixel even on
	// integer-aligned coordinates under some surface transforms; turning
	// it off here ensures adjacent cells (e.g. the OpenCode wordmark)
	// abut with no visible seam. Box-drawing curves and diagonals keep
	// the default AA so they don't go jaggy.
	cr.Save()
	defer cr.Restore()
	cr.SetAntialias(cairo.AntialiasNone)

	setRGB(cr, fg)
	switch cp {
	case 0x2580: // ▀ upper half
		alignedBlock(cr, x, y, w, h, hCenter, vTop, 1, fHalf)
	case 0x2581: // ▁ lower 1/8
		alignedBlock(cr, x, y, w, h, hCenter, vBottom, 1, fEighth)
	case 0x2582: // ▂ lower 1/4
		alignedBlock(cr, x, y, w, h, hCenter, vBottom, 1, fQuarter)
	case 0x2583: // ▃ lower 3/8
		alignedBlock(cr, x, y, w, h, hCenter, vBottom, 1, f3Eighths)
	case 0x2584: // ▄ lower half
		alignedBlock(cr, x, y, w, h, hCenter, vBottom, 1, fHalf)
	case 0x2585: // ▅ lower 5/8
		alignedBlock(cr, x, y, w, h, hCenter, vBottom, 1, f5Eighths)
	case 0x2586: // ▆ lower 3/4
		alignedBlock(cr, x, y, w, h, hCenter, vBottom, 1, f3Quarters)
	case 0x2587: // ▇ lower 7/8
		alignedBlock(cr, x, y, w, h, hCenter, vBottom, 1, f7Eighths)
	case 0x2588: // █ full
		fillRect(cr, x, y, w, h)
	case 0x2589: // ▉ left 7/8
		alignedBlock(cr, x, y, w, h, hLeft, vMiddle, f7Eighths, 1)
	case 0x258A: // ▊ left 3/4
		alignedBlock(cr, x, y, w, h, hLeft, vMiddle, f3Quarters, 1)
	case 0x258B: // ▋ left 5/8
		alignedBlock(cr, x, y, w, h, hLeft, vMiddle, f5Eighths, 1)
	case 0x258C: // ▌ left half
		alignedBlock(cr, x, y, w, h, hLeft, vMiddle, fHalf, 1)
	case 0x258D: // ▍ left 3/8
		alignedBlock(cr, x, y, w, h, hLeft, vMiddle, f3Eighths, 1)
	case 0x258E: // ▎ left 1/4
		alignedBlock(cr, x, y, w, h, hLeft, vMiddle, fQuarter, 1)
	case 0x258F: // ▏ left 1/8
		alignedBlock(cr, x, y, w, h, hLeft, vMiddle, fEighth, 1)
	case 0x2590: // ▐ right half
		alignedBlock(cr, x, y, w, h, hRight, vMiddle, fHalf, 1)
	case 0x2591, 0x2592, 0x2593: // ░ ▒ ▓ shades
		alpha := [...]float64{0.25, 0.5, 0.75}[cp-0x2591]
		cr.SetSourceRGBA(float64(fg.R)/255, float64(fg.G)/255, float64(fg.B)/255, alpha)
		fillRect(cr, x, y, w, h)
	case 0x2594: // ▔ upper 1/8
		alignedBlock(cr, x, y, w, h, hCenter, vTop, 1, fEighth)
	case 0x2595: // ▕ right 1/8
		alignedBlock(cr, x, y, w, h, hRight, vMiddle, fEighth, 1)
	case 0x2596: // ▖ bl quadrant
		drawQuads(cr, x, y, w, h, false, false, true, false)
	case 0x2597: // ▗ br quadrant
		drawQuads(cr, x, y, w, h, false, false, false, true)
	case 0x2598: // ▘ tl quadrant
		drawQuads(cr, x, y, w, h, true, false, false, false)
	case 0x2599: // ▙ tl + bl + br
		drawQuads(cr, x, y, w, h, true, false, true, true)
	case 0x259A: // ▚ tl + br
		drawQuads(cr, x, y, w, h, true, false, false, true)
	case 0x259B: // ▛ tl + tr + bl
		drawQuads(cr, x, y, w, h, true, true, true, false)
	case 0x259C: // ▜ tl + tr + br
		drawQuads(cr, x, y, w, h, true, true, false, true)
	case 0x259D: // ▝ tr quadrant
		drawQuads(cr, x, y, w, h, false, true, false, false)
	case 0x259E: // ▞ tr + bl
		drawQuads(cr, x, y, w, h, false, true, true, false)
	case 0x259F: // ▟ tr + bl + br
		drawQuads(cr, x, y, w, h, false, true, true, true)
	default:
		return false
	}
	return true
}

// alignedBlock fills a sub-rect of the cell whose size is (w*fw, h*fh),
// rounded to integer pixels, then placed by the given alignment. Mirrors
// block.zig:121-152's blockShade.
func alignedBlock(cr *cairo.Context, x, y, w, h float64, ha halign, va valign, fw, fh float64) {
	rw := math.Round(w * fw)
	rh := math.Round(h * fh)
	var ox, oy float64
	switch ha {
	case hLeft:
		ox = 0
	case hCenter:
		ox = math.Floor((w - rw) / 2)
	case hRight:
		ox = w - rw
	}
	switch va {
	case vTop:
		oy = 0
	case vMiddle:
		oy = math.Floor((h - rh) / 2)
	case vBottom:
		oy = h - rh
	}
	fillRect(cr, x+ox, y+oy, rw, rh)
}

// drawQuads paints any combination of the four quadrants. The bottom and
// right rects use (h-halfH) / (w-halfW) so the quadrants tile the cell
// exactly even when h or w is odd.
func drawQuads(cr *cairo.Context, x, y, w, h float64, tl, tr, bl, br bool) {
	halfW := math.Round(w / 2)
	halfH := math.Round(h / 2)
	if tl {
		fillRect(cr, x, y, halfW, halfH)
	}
	if tr {
		fillRect(cr, x+halfW, y, w-halfW, halfH)
	}
	if bl {
		fillRect(cr, x, y+halfH, halfW, h-halfH)
	}
	if br {
		fillRect(cr, x+halfW, y+halfH, w-halfW, h-halfH)
	}
}

func fillRect(cr *cairo.Context, x, y, w, h float64) {
	cr.Rectangle(x, y, w, h)
	cr.Fill()
}

// -----------------------------------------------------------------------------
// Layer 2: Box drawing (U+2500–U+257F)
// -----------------------------------------------------------------------------

type lineStyle uint8

const (
	lineNone lineStyle = iota
	lineLight
	lineHeavy
	lineDouble
)

type lines4 struct {
	up, right, down, left lineStyle
}

type corner uint8

const (
	cornerTL corner = iota
	cornerTR
	cornerBL
	cornerBR
)

func drawBoxGlyph(cr *cairo.Context, x, y, w, h float64, fg ghostty.ColorRGB, cp rune) bool {
	setRGB(cr, fg)
	switch cp {
	// --- simple horizontal/vertical lines ---
	case 0x2500: // ─
		drawBoxLines(cr, x, y, w, h, lines4{left: lineLight, right: lineLight})
	case 0x2501: // ━
		drawBoxLines(cr, x, y, w, h, lines4{left: lineHeavy, right: lineHeavy})
	case 0x2502: // │
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight})
	case 0x2503: // ┃
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, down: lineHeavy})

	// --- dashed (3-count) ---
	case 0x2504: // ┄
		drawHDash(cr, x, y, w, h, 3, lineLight)
	case 0x2505: // ┅
		drawHDash(cr, x, y, w, h, 3, lineHeavy)
	case 0x2506: // ┆
		drawVDash(cr, x, y, w, h, 3, lineLight)
	case 0x2507: // ┇
		drawVDash(cr, x, y, w, h, 3, lineHeavy)
	// (4-count)
	case 0x2508: // ┈
		drawHDash(cr, x, y, w, h, 4, lineLight)
	case 0x2509: // ┉
		drawHDash(cr, x, y, w, h, 4, lineHeavy)
	case 0x250A: // ┊
		drawVDash(cr, x, y, w, h, 4, lineLight)
	case 0x250B: // ┋
		drawVDash(cr, x, y, w, h, 4, lineHeavy)

	// --- single-line corners (light/heavy mixes) ---
	case 0x250C: // ┌
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, right: lineLight})
	case 0x250D: // ┍
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, right: lineHeavy})
	case 0x250E: // ┎
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy, right: lineLight})
	case 0x250F: // ┏
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy, right: lineHeavy})
	case 0x2510: // ┐
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, left: lineLight})
	case 0x2511: // ┑
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, left: lineHeavy})
	case 0x2512: // ┒
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy, left: lineLight})
	case 0x2513: // ┓
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy, left: lineHeavy})
	case 0x2514: // └
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, right: lineLight})
	case 0x2515: // ┕
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, right: lineHeavy})
	case 0x2516: // ┖
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, right: lineLight})
	case 0x2517: // ┗
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, right: lineHeavy})
	case 0x2518: // ┘
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, left: lineLight})
	case 0x2519: // ┙
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, left: lineHeavy})
	case 0x251A: // ┚
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, left: lineLight})
	case 0x251B: // ┛
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, left: lineHeavy})

	// --- T-junctions, right side (├ family) ---
	case 0x251C: // ├
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight, right: lineLight})
	case 0x251D: // ┝
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight, right: lineHeavy})
	case 0x251E: // ┞
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, right: lineLight, down: lineLight})
	case 0x251F: // ┟
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy, right: lineLight, up: lineLight})
	case 0x2520: // ┠
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, down: lineHeavy, right: lineLight})
	case 0x2521: // ┡
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, right: lineHeavy, up: lineHeavy})
	case 0x2522: // ┢
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, right: lineHeavy, down: lineHeavy})
	case 0x2523: // ┣
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, down: lineHeavy, right: lineHeavy})

	// --- T-junctions, left side (┤ family) ---
	case 0x2524: // ┤
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight, left: lineLight})
	case 0x2525: // ┥
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight, left: lineHeavy})
	case 0x2526: // ┦
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, left: lineLight, down: lineLight})
	case 0x2527: // ┧
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy, left: lineLight, up: lineLight})
	case 0x2528: // ┨
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, down: lineHeavy, left: lineLight})
	case 0x2529: // ┩
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, left: lineHeavy, up: lineHeavy})
	case 0x252A: // ┪
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, left: lineHeavy, down: lineHeavy})
	case 0x252B: // ┫
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, down: lineHeavy, left: lineHeavy})

	// --- T-junctions, down (┬ family) ---
	case 0x252C: // ┬
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, left: lineLight, right: lineLight})
	case 0x252D: // ┭
		drawBoxLines(cr, x, y, w, h, lines4{left: lineHeavy, right: lineLight, down: lineLight})
	case 0x252E: // ┮
		drawBoxLines(cr, x, y, w, h, lines4{right: lineHeavy, left: lineLight, down: lineLight})
	case 0x252F: // ┯
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, left: lineHeavy, right: lineHeavy})
	case 0x2530: // ┰
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy, left: lineLight, right: lineLight})
	case 0x2531: // ┱
		drawBoxLines(cr, x, y, w, h, lines4{right: lineLight, left: lineHeavy, down: lineHeavy})
	case 0x2532: // ┲
		drawBoxLines(cr, x, y, w, h, lines4{left: lineLight, right: lineHeavy, down: lineHeavy})
	case 0x2533: // ┳
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy, left: lineHeavy, right: lineHeavy})

	// --- T-junctions, up (┴ family) ---
	case 0x2534: // ┴
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, left: lineLight, right: lineLight})
	case 0x2535: // ┵
		drawBoxLines(cr, x, y, w, h, lines4{left: lineHeavy, right: lineLight, up: lineLight})
	case 0x2536: // ┶
		drawBoxLines(cr, x, y, w, h, lines4{right: lineHeavy, left: lineLight, up: lineLight})
	case 0x2537: // ┷
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, left: lineHeavy, right: lineHeavy})
	case 0x2538: // ┸
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, left: lineLight, right: lineLight})
	case 0x2539: // ┹
		drawBoxLines(cr, x, y, w, h, lines4{right: lineLight, left: lineHeavy, up: lineHeavy})
	case 0x253A: // ┺
		drawBoxLines(cr, x, y, w, h, lines4{left: lineLight, right: lineHeavy, up: lineHeavy})
	case 0x253B: // ┻
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, left: lineHeavy, right: lineHeavy})

	// --- crosses (┼ family) ---
	case 0x253C: // ┼
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight, left: lineLight, right: lineLight})
	case 0x253D: // ┽
		drawBoxLines(cr, x, y, w, h, lines4{left: lineHeavy, right: lineLight, up: lineLight, down: lineLight})
	case 0x253E: // ┾
		drawBoxLines(cr, x, y, w, h, lines4{right: lineHeavy, left: lineLight, up: lineLight, down: lineLight})
	case 0x253F: // ┿
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight, left: lineHeavy, right: lineHeavy})
	case 0x2540: // ╀
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, down: lineLight, left: lineLight, right: lineLight})
	case 0x2541: // ╁
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy, up: lineLight, left: lineLight, right: lineLight})
	case 0x2542: // ╂
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, down: lineHeavy, left: lineLight, right: lineLight})
	case 0x2543: // ╃
		drawBoxLines(cr, x, y, w, h, lines4{left: lineHeavy, up: lineHeavy, right: lineLight, down: lineLight})
	case 0x2544: // ╄
		drawBoxLines(cr, x, y, w, h, lines4{right: lineHeavy, up: lineHeavy, left: lineLight, down: lineLight})
	case 0x2545: // ╅
		drawBoxLines(cr, x, y, w, h, lines4{left: lineHeavy, down: lineHeavy, right: lineLight, up: lineLight})
	case 0x2546: // ╆
		drawBoxLines(cr, x, y, w, h, lines4{right: lineHeavy, down: lineHeavy, left: lineLight, up: lineLight})
	case 0x2547: // ╇
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, up: lineHeavy, left: lineHeavy, right: lineHeavy})
	case 0x2548: // ╈
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineHeavy, left: lineHeavy, right: lineHeavy})
	case 0x2549: // ╉
		drawBoxLines(cr, x, y, w, h, lines4{right: lineLight, left: lineHeavy, up: lineHeavy, down: lineHeavy})
	case 0x254A: // ╊
		drawBoxLines(cr, x, y, w, h, lines4{left: lineLight, right: lineHeavy, up: lineHeavy, down: lineHeavy})
	case 0x254B: // ╋
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, down: lineHeavy, left: lineHeavy, right: lineHeavy})

	// --- 2-count dashed ---
	case 0x254C: // ╌
		drawHDash(cr, x, y, w, h, 2, lineLight)
	case 0x254D: // ╍
		drawHDash(cr, x, y, w, h, 2, lineHeavy)
	case 0x254E: // ╎
		drawVDash(cr, x, y, w, h, 2, lineLight)
	case 0x254F: // ╏
		drawVDash(cr, x, y, w, h, 2, lineHeavy)

	// --- double-line variants ---
	case 0x2550: // ═
		drawBoxLines(cr, x, y, w, h, lines4{left: lineDouble, right: lineDouble})
	case 0x2551: // ║
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, down: lineDouble})
	case 0x2552: // ╒
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, right: lineDouble})
	case 0x2553: // ╓
		drawBoxLines(cr, x, y, w, h, lines4{down: lineDouble, right: lineLight})
	case 0x2554: // ╔
		drawBoxLines(cr, x, y, w, h, lines4{down: lineDouble, right: lineDouble})
	case 0x2555: // ╕
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, left: lineDouble})
	case 0x2556: // ╖
		drawBoxLines(cr, x, y, w, h, lines4{down: lineDouble, left: lineLight})
	case 0x2557: // ╗
		drawBoxLines(cr, x, y, w, h, lines4{down: lineDouble, left: lineDouble})
	case 0x2558: // ╘
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, right: lineDouble})
	case 0x2559: // ╙
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, right: lineLight})
	case 0x255A: // ╚
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, right: lineDouble})
	case 0x255B: // ╛
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, left: lineDouble})
	case 0x255C: // ╜
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, left: lineLight})
	case 0x255D: // ╝
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, left: lineDouble})
	case 0x255E: // ╞
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight, right: lineDouble})
	case 0x255F: // ╟
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, down: lineDouble, right: lineLight})
	case 0x2560: // ╠
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, down: lineDouble, right: lineDouble})
	case 0x2561: // ╡
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight, left: lineDouble})
	case 0x2562: // ╢
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, down: lineDouble, left: lineLight})
	case 0x2563: // ╣
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, down: lineDouble, left: lineDouble})
	case 0x2564: // ╤
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight, left: lineDouble, right: lineDouble})
	case 0x2565: // ╥
		drawBoxLines(cr, x, y, w, h, lines4{down: lineDouble, left: lineLight, right: lineLight})
	case 0x2566: // ╦
		drawBoxLines(cr, x, y, w, h, lines4{down: lineDouble, left: lineDouble, right: lineDouble})
	case 0x2567: // ╧
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, left: lineDouble, right: lineDouble})
	case 0x2568: // ╨
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, left: lineLight, right: lineLight})
	case 0x2569: // ╩
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, left: lineDouble, right: lineDouble})
	case 0x256A: // ╪
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineLight, left: lineDouble, right: lineDouble})
	case 0x256B: // ╫
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, down: lineDouble, left: lineLight, right: lineLight})
	case 0x256C: // ╬
		drawBoxLines(cr, x, y, w, h, lines4{up: lineDouble, down: lineDouble, left: lineDouble, right: lineDouble})

	// --- rounded corners (Codex header uses these) ---
	case 0x256D: // ╭
		drawArc(cr, x, y, w, h, cornerBR)
	case 0x256E: // ╮
		drawArc(cr, x, y, w, h, cornerBL)
	case 0x256F: // ╯
		drawArc(cr, x, y, w, h, cornerTL)
	case 0x2570: // ╰
		drawArc(cr, x, y, w, h, cornerTR)

	// --- diagonals ---
	case 0x2571: // ╱
		drawDiag(cr, x, y, w, h, true, false)
	case 0x2572: // ╲
		drawDiag(cr, x, y, w, h, false, true)
	case 0x2573: // ╳
		drawDiag(cr, x, y, w, h, true, true)

	// --- half-edges (light) ---
	case 0x2574: // ╴
		drawBoxLines(cr, x, y, w, h, lines4{left: lineLight})
	case 0x2575: // ╵
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight})
	case 0x2576: // ╶
		drawBoxLines(cr, x, y, w, h, lines4{right: lineLight})
	case 0x2577: // ╷
		drawBoxLines(cr, x, y, w, h, lines4{down: lineLight})

	// --- half-edges (heavy) ---
	case 0x2578: // ╸
		drawBoxLines(cr, x, y, w, h, lines4{left: lineHeavy})
	case 0x2579: // ╹
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy})
	case 0x257A: // ╺
		drawBoxLines(cr, x, y, w, h, lines4{right: lineHeavy})
	case 0x257B: // ╻
		drawBoxLines(cr, x, y, w, h, lines4{down: lineHeavy})

	// --- mixed weight half-edges ---
	case 0x257C: // ╼
		drawBoxLines(cr, x, y, w, h, lines4{left: lineLight, right: lineHeavy})
	case 0x257D: // ╽
		drawBoxLines(cr, x, y, w, h, lines4{up: lineLight, down: lineHeavy})
	case 0x257E: // ╾
		drawBoxLines(cr, x, y, w, h, lines4{left: lineHeavy, right: lineLight})
	case 0x257F: // ╿
		drawBoxLines(cr, x, y, w, h, lines4{up: lineHeavy, down: lineLight})

	default:
		return false
	}
	return true
}

// boxThickness derives the "light" stroke width from cell height. Roughly
// matches what Ghostty derives from font metrics (its `box_thickness`); we
// don't have those here so use a heuristic: ~7% of cell height, min 1px.
func boxThickness(h float64) float64 {
	t := math.Round(h / 14)
	if t < 1 {
		return 1
	}
	return t
}

// drawBoxLines is the heart of Layer 2: paint up to four cardinal-direction
// strokes that meet at the cell center with correct heavy/double junction
// precedence. Direct port of linesChar at box.zig:399-637.
func drawBoxLines(cr *cairo.Context, x, y, w, h float64, ln lines4) {
	light := boxThickness(h)
	heavy := 2 * light

	// Horizontal stroke top/bottom edges
	hLightTop := math.Floor((h - light) / 2)
	hLightBot := hLightTop + light
	hHeavyTop := math.Floor((h - heavy) / 2)
	hHeavyBot := hHeavyTop + heavy
	hDoubleTop := hLightTop - light
	hDoubleBot := hLightBot + light

	// Vertical stroke left/right edges
	vLightLeft := math.Floor((w - light) / 2)
	vLightRight := vLightLeft + light
	vHeavyLeft := math.Floor((w - heavy) / 2)
	vHeavyRight := vHeavyLeft + heavy
	vDoubleLeft := vLightLeft - light
	vDoubleRight := vLightRight + light

	// Where each stroke terminates near the center. The rules below mirror
	// box.zig:438-487 — they encode the visual precedence so heavy and
	// double junctions look right (the lighter line stops before the
	// heavier crossbar; matching styles meet flush).
	upBottom := pickJunction(ln.left, ln.right, ln.down, ln.up,
		hHeavyBot, hDoubleBot, hLightBot, hLightTop)
	downTop := pickJunction(ln.left, ln.right, ln.up, ln.down,
		hHeavyTop, hDoubleTop, hLightTop, hLightBot)
	leftRight := pickJunction(ln.up, ln.down, ln.right, ln.left,
		vHeavyRight, vDoubleRight, vLightRight, vLightLeft)
	rightLeft := pickJunction(ln.up, ln.down, ln.left, ln.right,
		vHeavyLeft, vDoubleLeft, vLightLeft, vLightRight)

	// UP stroke
	switch ln.up {
	case lineLight:
		boxRect(cr, x, y, vLightLeft, 0, vLightRight, upBottom)
	case lineHeavy:
		boxRect(cr, x, y, vHeavyLeft, 0, vHeavyRight, upBottom)
	case lineDouble:
		leftBot := upBottom
		if ln.left == lineDouble {
			leftBot = hLightTop
		}
		rightBot := upBottom
		if ln.right == lineDouble {
			rightBot = hLightTop
		}
		boxRect(cr, x, y, vDoubleLeft, 0, vLightLeft, leftBot)
		boxRect(cr, x, y, vLightRight, 0, vDoubleRight, rightBot)
	}

	// RIGHT stroke
	switch ln.right {
	case lineLight:
		boxRect(cr, x, y, rightLeft, hLightTop, w, hLightBot)
	case lineHeavy:
		boxRect(cr, x, y, rightLeft, hHeavyTop, w, hHeavyBot)
	case lineDouble:
		topLeft := rightLeft
		if ln.up == lineDouble {
			topLeft = vLightRight
		}
		botLeft := rightLeft
		if ln.down == lineDouble {
			botLeft = vLightRight
		}
		boxRect(cr, x, y, topLeft, hDoubleTop, w, hLightTop)
		boxRect(cr, x, y, botLeft, hLightBot, w, hDoubleBot)
	}

	// DOWN stroke
	switch ln.down {
	case lineLight:
		boxRect(cr, x, y, vLightLeft, downTop, vLightRight, h)
	case lineHeavy:
		boxRect(cr, x, y, vHeavyLeft, downTop, vHeavyRight, h)
	case lineDouble:
		leftTop := downTop
		if ln.left == lineDouble {
			leftTop = hLightBot
		}
		rightTop := downTop
		if ln.right == lineDouble {
			rightTop = hLightBot
		}
		boxRect(cr, x, y, vDoubleLeft, leftTop, vLightLeft, h)
		boxRect(cr, x, y, vLightRight, rightTop, vDoubleRight, h)
	}

	// LEFT stroke
	switch ln.left {
	case lineLight:
		boxRect(cr, x, y, 0, hLightTop, leftRight, hLightBot)
	case lineHeavy:
		boxRect(cr, x, y, 0, hHeavyTop, leftRight, hHeavyBot)
	case lineDouble:
		topRight := leftRight
		if ln.up == lineDouble {
			topRight = vLightLeft
		}
		botRight := leftRight
		if ln.down == lineDouble {
			botRight = vLightLeft
		}
		boxRect(cr, x, y, 0, hDoubleTop, topRight, hLightTop)
		boxRect(cr, x, y, 0, hLightBot, botRight, hDoubleBot)
	}
}

// pickJunction implements the perpendicular-stroke termination logic from
// linesChar. Given the perpendicular pair (perp1, perp2) and the parallel
// pair (parallel, this), return the coordinate where `this`'s stroke ends.
//
//	heavyEdge    = the "far" edge if any perpendicular is heavy
//	doubleEdge   = the "far" edge if any perpendicular is double
//	lightEdgeFar = the "far" edge for plain light junctions
//	lightEdgeNear = the "near" edge (used when one perpendicular is empty
//	                and the parallel is set, so the stroke just meets the
//	                centerline of the perpendicular instead of crossing it)
func pickJunction(perp1, perp2, parallel, this lineStyle, heavyEdge, doubleEdge, lightEdgeFar, lightEdgeNear float64) float64 {
	if perp1 == lineHeavy || perp2 == lineHeavy {
		return heavyEdge
	}
	if perp1 != perp2 || parallel == this {
		if perp1 == lineDouble || perp2 == lineDouble {
			return doubleEdge
		}
		return lightEdgeFar
	}
	if perp1 == lineNone && perp2 == lineNone {
		return lightEdgeFar
	}
	return lightEdgeNear
}

// boxRect paints a rect in cell-relative pixel coordinates (left, top,
// right, bottom). Mirrors Ghostty's `canvas.box(l, t, r, b, .on)`.
func boxRect(cr *cairo.Context, x, y, l, t, r, b float64) {
	if r <= l || b <= t {
		return
	}
	cr.Rectangle(x+l, y+t, r-l, b-t)
	cr.Fill()
}

// drawArc paints a quadrant Bezier for the rounded-corner glyphs ╭╮╯╰.
// The "corner" parameter is the *interior* corner — i.e. the side of the
// cell the arc bulges into. ╭ (down+right strokes) curves from the bottom
// midpoint up and around to the right midpoint, with the arc bulging
// toward the bottom-right.
func drawArc(cr *cairo.Context, x, y, w, h float64, c corner) {
	t := boxThickness(h)
	cx := math.Floor((w-t)/2) + t/2
	cy := math.Floor((h-t)/2) + t/2
	r := math.Min(w, h) / 2
	const s = 0.25 // Bezier control offset, matches box.zig:710

	cr.NewPath()
	switch c {
	case cornerTL: // ╯ — strokes go up + left
		cr.MoveTo(x+cx, y+0)
		cr.LineTo(x+cx, y+cy-r)
		cr.CurveTo(x+cx, y+cy-s*r, x+cx-s*r, y+cy, x+cx-r, y+cy)
		cr.LineTo(x+0, y+cy)
	case cornerTR: // ╰ — up + right
		cr.MoveTo(x+cx, y+0)
		cr.LineTo(x+cx, y+cy-r)
		cr.CurveTo(x+cx, y+cy-s*r, x+cx+s*r, y+cy, x+cx+r, y+cy)
		cr.LineTo(x+w, y+cy)
	case cornerBL: // ╮ — down + left
		cr.MoveTo(x+cx, y+h)
		cr.LineTo(x+cx, y+cy+r)
		cr.CurveTo(x+cx, y+cy+s*r, x+cx-s*r, y+cy, x+cx-r, y+cy)
		cr.LineTo(x+0, y+cy)
	case cornerBR: // ╭ — down + right
		cr.MoveTo(x+cx, y+h)
		cr.LineTo(x+cx, y+cy+r)
		cr.CurveTo(x+cx, y+cy+s*r, x+cx+s*r, y+cy, x+cx+r, y+cy)
		cr.LineTo(x+w, y+cy)
	}
	cr.SetLineCap(cairo.LineCapButt)
	cr.SetLineWidth(t)
	cr.Stroke()
}

// drawDiag paints one or both light diagonals across the cell. The
// strokes overshoot the corners slightly so the slope stays correct;
// see box.zig:638-692.
func drawDiag(cr *cairo.Context, x, y, w, h float64, urToLL, ulToLR bool) {
	t := boxThickness(h)
	slopeX := math.Min(1.0, w/h)
	slopeY := math.Min(1.0, h/w)

	cr.SetLineCap(cairo.LineCapButt)
	cr.SetLineWidth(t)
	if urToLL {
		cr.NewPath()
		cr.MoveTo(x+w+0.5*slopeX, y-0.5*slopeY)
		cr.LineTo(x-0.5*slopeX, y+h+0.5*slopeY)
		cr.Stroke()
	}
	if ulToLR {
		cr.NewPath()
		cr.MoveTo(x-0.5*slopeX, y-0.5*slopeY)
		cr.LineTo(x+w+0.5*slopeX, y+h+0.5*slopeY)
		cr.Stroke()
	}
}

// drawHDash paints `count` horizontal dash segments centered vertically.
// Direct port of dashHorizontal at box.zig:779-851 — the dash and gap
// math is preserved so cells tile cleanly into one continuous dashed
// line when placed side by side.
func drawHDash(cr *cairo.Context, x, y, w, h float64, count int, style lineStyle) {
	thick := boxThickness(h)
	if style == lineHeavy {
		thick *= 2
	}
	desiredGap := thick
	if style == lineLight && desiredGap < 4 {
		desiredGap = 4
	}

	wi := int(w)
	if wi < count*2 {
		drawBoxLines(cr, x, y, w, h, lines4{left: style, right: style})
		return
	}

	gap := int(desiredGap)
	if maxGap := wi / (2 * count); gap > maxGap {
		gap = maxGap
	}
	totalGap := gap * count
	totalDash := wi - totalGap
	dash := totalDash / count
	extra := totalDash % count

	yi := math.Floor((h - thick) / 2)
	xi := float64(gap / 2)
	for i := 0; i < count; i++ {
		dw := dash
		if extra > 0 {
			dw++
			extra--
		}
		boxRect(cr, x, y, xi, yi, xi+float64(dw), yi+thick)
		xi += float64(dw + gap)
	}
}

// drawVDash is the vertical analogue of drawHDash.
func drawVDash(cr *cairo.Context, x, y, w, h float64, count int, style lineStyle) {
	thick := boxThickness(h)
	if style == lineHeavy {
		thick *= 2
	}
	desiredGap := thick
	if style == lineLight && desiredGap < 4 {
		desiredGap = 4
	}

	hi := int(h)
	if hi < count*2 {
		drawBoxLines(cr, x, y, w, h, lines4{up: style, down: style})
		return
	}

	gap := int(desiredGap)
	if maxGap := hi / (2 * count); gap > maxGap {
		gap = maxGap
	}
	totalGap := gap * count
	totalDash := hi - totalGap
	dash := totalDash / count
	extra := totalDash % count

	xi := math.Floor((w - thick) / 2)
	yi := float64(gap / 2)
	for i := 0; i < count; i++ {
		dh := dash
		if extra > 0 {
			dh++
			extra--
		}
		boxRect(cr, x, y, xi, yi, xi+thick, yi+float64(dh))
		yi += float64(dh + gap)
	}
}
