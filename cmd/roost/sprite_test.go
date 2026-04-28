package main

import (
	"testing"

	"github.com/diamondburned/gotk4/pkg/cairo"

	"github.com/charliek/roost/internal/ghostty"
)

// renderSprite paints a single glyph at the origin of a w*h ARGB32 image
// surface in opaque white, against a transparent background. Returns the
// raw byte buffer plus stride so tests can spot-check pixels.
func renderSprite(t *testing.T, cp rune, w, h int) (data []byte, stride int, handled bool) {
	t.Helper()
	surf := cairo.CreateImageSurface(cairo.FormatARGB32, w, h)
	cr := cairo.Create(surf)
	// Cairo image surfaces are zero-initialised → transparent black.
	handled = drawCellSprite(cr, 0, 0, float64(w), float64(h),
		ghostty.ColorRGB{R: 255, G: 255, B: 255}, cp)
	surf.Flush()
	return surf.Data(), surf.Stride(), handled
}

// pixelOn reports whether the pixel at (x,y) is "on" — i.e. has any of
// its colour channels non-zero. ARGB32 in memory on little-endian hosts
// is laid out as B, G, R, A bytes per pixel.
func pixelOn(data []byte, stride, x, y int) bool {
	off := y*stride + x*4
	return data[off] != 0 || data[off+1] != 0 || data[off+2] != 0
}

// pixelsOnRect counts on-pixels within an axis-aligned rectangle.
func pixelsOnRect(data []byte, stride, x0, y0, x1, y1 int) int {
	n := 0
	for y := y0; y < y1; y++ {
		for x := x0; x < x1; x++ {
			if pixelOn(data, stride, x, y) {
				n++
			}
		}
	}
	return n
}

// rectFilled asserts that every pixel in (x0,y0)-(x1,y1) is on.
func rectFilled(t *testing.T, data []byte, stride, x0, y0, x1, y1 int) {
	t.Helper()
	for y := y0; y < y1; y++ {
		for x := x0; x < x1; x++ {
			if !pixelOn(data, stride, x, y) {
				t.Errorf("expected on at (%d,%d), got off", x, y)
				return
			}
		}
	}
}

// rectEmpty asserts that every pixel in (x0,y0)-(x1,y1) is off.
func rectEmpty(t *testing.T, data []byte, stride, x0, y0, x1, y1 int) {
	t.Helper()
	for y := y0; y < y1; y++ {
		for x := x0; x < x1; x++ {
			if pixelOn(data, stride, x, y) {
				t.Errorf("expected off at (%d,%d), got on", x, y)
				return
			}
		}
	}
}

func TestSpriteDispatchSkipsNonGeometric(t *testing.T) {
	for _, cp := range []rune{'A', ' ', '0', 0x24FF, 0x25A0, 0x2700} {
		_, _, handled := renderSprite(t, cp, 8, 16)
		if handled {
			t.Errorf("U+%04X should not be handled by sprite renderer", cp)
		}
	}
}

func TestSpriteDispatchHandlesRanges(t *testing.T) {
	// One representative from each range.
	for _, cp := range []rune{0x2500, 0x2580, 0x2588, 0x256D, 0x2571, 0x257F} {
		_, _, handled := renderSprite(t, cp, 12, 24)
		if !handled {
			t.Errorf("U+%04X should be handled", cp)
		}
	}
}

func TestFullBlock(t *testing.T) {
	// █ should fill every pixel in the cell.
	w, h := 8, 16
	data, stride, _ := renderSprite(t, 0x2588, w, h)
	rectFilled(t, data, stride, 0, 0, w, h)
}

func TestUpperHalfBlock(t *testing.T) {
	// ▀ fills the upper half; lower half is empty.
	w, h := 10, 20
	data, stride, _ := renderSprite(t, 0x2580, w, h)
	rectFilled(t, data, stride, 0, 0, w, h/2)
	rectEmpty(t, data, stride, 0, h/2, w, h)
}

func TestLowerHalfBlock(t *testing.T) {
	// ▄ fills the lower half; upper half is empty.
	w, h := 10, 20
	data, stride, _ := renderSprite(t, 0x2584, w, h)
	rectEmpty(t, data, stride, 0, 0, w, h/2)
	rectFilled(t, data, stride, 0, h/2, w, h)
}

func TestLeftHalfBlock(t *testing.T) {
	// ▌
	w, h := 10, 20
	data, stride, _ := renderSprite(t, 0x258C, w, h)
	rectFilled(t, data, stride, 0, 0, w/2, h)
	rectEmpty(t, data, stride, w/2, 0, w, h)
}

func TestRightHalfBlock(t *testing.T) {
	// ▐
	w, h := 10, 20
	data, stride, _ := renderSprite(t, 0x2590, w, h)
	rectEmpty(t, data, stride, 0, 0, w/2, h)
	rectFilled(t, data, stride, w/2, 0, w, h)
}

func TestQuadrantTL(t *testing.T) {
	// ▘ top-left quadrant only.
	w, h := 10, 20
	data, stride, _ := renderSprite(t, 0x2598, w, h)
	rectFilled(t, data, stride, 0, 0, w/2, h/2)
	rectEmpty(t, data, stride, w/2, 0, w, h/2)
	rectEmpty(t, data, stride, 0, h/2, w/2, h)
	rectEmpty(t, data, stride, w/2, h/2, w, h)
}

func TestQuadrantTRplusBL(t *testing.T) {
	// ▞
	w, h := 10, 20
	data, stride, _ := renderSprite(t, 0x259E, w, h)
	rectEmpty(t, data, stride, 0, 0, w/2, h/2)
	rectFilled(t, data, stride, w/2, 0, w, h/2)
	rectFilled(t, data, stride, 0, h/2, w/2, h)
	rectEmpty(t, data, stride, w/2, h/2, w, h)
}

func TestUpperEighthBlock(t *testing.T) {
	// ▔ upper 1/8.
	w, h := 16, 16
	data, stride, _ := renderSprite(t, 0x2594, w, h)
	rectFilled(t, data, stride, 0, 0, w, h/8)
	rectEmpty(t, data, stride, 0, h/8+1, w, h) // +1 to skip rounding boundary
}

func TestHorizontalLine(t *testing.T) {
	// ─ U+2500: light horizontal stroke spanning full width, centered.
	w, h := 12, 24
	data, stride, _ := renderSprite(t, 0x2500, w, h)
	// At least one pixel on at the cell edges (left and right sides).
	if !pixelOn(data, stride, 0, h/2) {
		t.Error("expected ─ to reach the left edge")
	}
	if !pixelOn(data, stride, w-1, h/2) {
		t.Error("expected ─ to reach the right edge")
	}
	// Top and bottom rows are empty.
	rectEmpty(t, data, stride, 0, 0, w, 1)
	rectEmpty(t, data, stride, 0, h-1, w, h)
}

func TestVerticalLine(t *testing.T) {
	// │ U+2502.
	w, h := 12, 24
	data, stride, _ := renderSprite(t, 0x2502, w, h)
	if !pixelOn(data, stride, w/2, 0) {
		t.Error("expected │ to reach the top edge")
	}
	if !pixelOn(data, stride, w/2, h-1) {
		t.Error("expected │ to reach the bottom edge")
	}
	rectEmpty(t, data, stride, 0, 0, 1, h)
	rectEmpty(t, data, stride, w-1, 0, w, h)
}

func TestLightCross(t *testing.T) {
	// ┼ U+253C: horizontal + vertical strokes.
	w, h := 14, 28
	data, stride, _ := renderSprite(t, 0x253C, w, h)
	if !pixelOn(data, stride, 0, h/2) {
		t.Error("┼: missing left edge")
	}
	if !pixelOn(data, stride, w-1, h/2) {
		t.Error("┼: missing right edge")
	}
	if !pixelOn(data, stride, w/2, 0) {
		t.Error("┼: missing top edge")
	}
	if !pixelOn(data, stride, w/2, h-1) {
		t.Error("┼: missing bottom edge")
	}
}

func TestHeavyCross(t *testing.T) {
	// ╋ U+254B: same connectivity as ┼ but heavier.
	w, h := 14, 28
	dataLight, _, _ := renderSprite(t, 0x253C, w, h)
	dataHeavy, stride, _ := renderSprite(t, 0x254B, w, h)
	on := func(data []byte) int {
		n := 0
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				if pixelOn(data, stride, x, y) {
					n++
				}
			}
		}
		return n
	}
	if on(dataHeavy) <= on(dataLight) {
		t.Errorf("expected heavy cross to have more on-pixels than light (heavy=%d, light=%d)",
			on(dataHeavy), on(dataLight))
	}
}

func TestDoubleHorizontal(t *testing.T) {
	// ═ U+2550: two parallel horizontal strokes with a gap between.
	w, h := 16, 32
	data, stride, _ := renderSprite(t, 0x2550, w, h)
	// Sample the middle column. Expect two separated runs of on-pixels.
	col := w / 2
	runs := 0
	prev := false
	for y := 0; y < h; y++ {
		cur := pixelOn(data, stride, col, y)
		if cur && !prev {
			runs++
		}
		prev = cur
	}
	if runs != 2 {
		t.Errorf("═: expected 2 horizontal stroke runs in middle column, got %d", runs)
	}
}

func TestSquareCornerTL(t *testing.T) {
	// ┌ U+250C: down + right strokes only.
	w, h := 14, 28
	data, stride, _ := renderSprite(t, 0x250C, w, h)
	if !pixelOn(data, stride, w-1, h/2) {
		t.Error("┌: missing right edge")
	}
	if !pixelOn(data, stride, w/2, h-1) {
		t.Error("┌: missing bottom edge")
	}
	// Top edge above center should be empty (no up stroke).
	rectEmpty(t, data, stride, 0, 0, w, h/2-2)
	// Left edge left of center should be empty (no left stroke).
	rectEmpty(t, data, stride, 0, 0, w/2-2, h)
}

func TestRoundedCornerTL(t *testing.T) {
	// ╭ U+256D: rounded down + right (curve bulges into bottom-right).
	w, h := 16, 32
	data, stride, _ := renderSprite(t, 0x256D, w, h)
	if !pixelOn(data, stride, w-1, h/2) {
		t.Error("╭: missing right edge")
	}
	if !pixelOn(data, stride, w/2, h-1) {
		t.Error("╭: missing bottom edge")
	}
	// The exact corner cell (top-left) is empty.
	rectEmpty(t, data, stride, 0, 0, w/4, h/4)
}

func TestDiagonalUpperRightToLowerLeft(t *testing.T) {
	// ╱ U+2571: stroke from top-right to bottom-left.
	w, h := 16, 32
	data, stride, _ := renderSprite(t, 0x2571, w, h)
	// Top-right neighbourhood is on; bottom-right is off.
	if pixelsOnRect(data, stride, w-3, 0, w, 3) == 0 {
		t.Error("╱: expected on-pixels near top-right")
	}
	if pixelsOnRect(data, stride, w-3, h-3, w, h) != 0 {
		t.Error("╱: expected no pixels near bottom-right")
	}
	if pixelsOnRect(data, stride, 0, h-3, 3, h) == 0 {
		t.Error("╱: expected on-pixels near bottom-left")
	}
}

func TestDiagonalCross(t *testing.T) {
	// ╳ U+2573: both diagonals — pixels in all four corners.
	w, h := 16, 32
	data, stride, _ := renderSprite(t, 0x2573, w, h)
	for _, c := range [4][4]int{
		{0, 0, 3, 3},     // tl
		{w - 3, 0, w, 3}, // tr
		{0, h - 3, 3, h}, // bl
		{w - 3, h - 3, w, h},
	} {
		if pixelsOnRect(data, stride, c[0], c[1], c[2], c[3]) == 0 {
			t.Errorf("╳: expected on-pixels in corner %v", c)
		}
	}
}

func TestDashedHorizontal(t *testing.T) {
	// ┄ U+2504: 3-segment dashed horizontal. The stroke is thin and
	// centered — collapse pixels across a small Y window so the test
	// doesn't depend on which exact row the 1px stroke lands on.
	w, h := 30, 16
	data, stride, _ := renderSprite(t, 0x2504, w, h)
	colOn := func(x int) bool {
		for y := h/2 - 2; y <= h/2+2; y++ {
			if pixelOn(data, stride, x, y) {
				return true
			}
		}
		return false
	}
	runs := 0
	prev := false
	for x := 0; x < w; x++ {
		cur := colOn(x)
		if cur && !prev {
			runs++
		}
		prev = cur
	}
	if runs != 3 {
		t.Errorf("┄: expected 3 dash segments, got %d", runs)
	}
}

// TestBlockTilingNoGap covers the OpenCode-logo regression. Two █ cells
// stacked vertically (or side-by-side) must abut without a gap row/col.
func TestBlockTilingNoGap(t *testing.T) {
	w, cellH := 8, 20
	surf := cairo.CreateImageSurface(cairo.FormatARGB32, w*2, cellH*2)
	cr := cairo.Create(surf)
	white := ghostty.ColorRGB{R: 255, G: 255, B: 255}
	for row := 0; row < 2; row++ {
		for col := 0; col < 2; col++ {
			ok := drawCellSprite(cr,
				float64(col*w), float64(row*cellH),
				float64(w), float64(cellH),
				white, 0x2588)
			if !ok {
				t.Fatal("█ not handled")
			}
		}
	}
	surf.Flush()
	data, stride := surf.Data(), surf.Stride()
	rectFilled(t, data, stride, 0, 0, w*2, cellH*2)

	// Half-block adjacency: ▄ above ▀ in the same column should also tile
	// because both halves butt against the shared cell boundary.
	surf2 := cairo.CreateImageSurface(cairo.FormatARGB32, w, cellH*2)
	cr2 := cairo.Create(surf2)
	if !drawCellSprite(cr2, 0, 0, float64(w), float64(cellH), white, 0x2584) {
		t.Fatal("▄ not handled")
	}
	if !drawCellSprite(cr2, 0, float64(cellH), float64(w), float64(cellH), white, 0x2580) {
		t.Fatal("▀ not handled")
	}
	surf2.Flush()
	data2, stride2 := surf2.Data(), surf2.Stride()
	col := w / 2
	// Cell 0 bottom row + cell 1 top row should both be on (the two halves
	// meet at the cell boundary).
	if !pixelOn(data2, stride2, col, cellH-1) {
		t.Error("▄: last row of cell 0 should be on (boundary)")
	}
	if !pixelOn(data2, stride2, col, cellH) {
		t.Error("▀: first row of cell 1 should be on (boundary)")
	}
}
