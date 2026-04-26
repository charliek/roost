package ghostty

// #include <ghostty/vt.h>
// #include <stdlib.h>
// #include <string.h>
//
// // Sized-struct initializers (the GHOSTTY_INIT_SIZED macro is C-only).
// static void roost_init_render_colors(GhosttyRenderStateColors* c) {
//     memset(c, 0, sizeof(*c));
//     c->size = sizeof(*c);
// }
// static void roost_init_style(GhosttyStyle* s) {
//     memset(s, 0, sizeof(*s));
//     s->size = sizeof(*s);
// }
import "C"

import (
	"fmt"
	"runtime"
	"unsafe"
)

// ColorRGB is a 24-bit color. Matches GhosttyColorRgb { r, g, b }.
type ColorRGB struct {
	R, G, B uint8
}

// Cell is one terminal cell's renderable state. We expose only what the
// renderer needs: the (first) grapheme codepoint, resolved fg/bg, and the
// minimal style flags. Multi-codepoint graphemes (emoji, ZWJ) collapse to
// the base codepoint in the spike.
type Cell struct {
	Codepoint rune    // 0 = empty cell
	FG        ColorRGB // valid if HasFG
	BG        ColorRGB // valid if HasBG
	HasFG     bool
	HasBG     bool
	Bold      bool
	Italic    bool
	Inverse   bool
}

// RenderState is a per-frame snapshot of a Terminal's grid + colors. Reuse
// across frames; call Update before each frame.
//
// Holds three opaque libghostty handles: the state itself, a row iterator,
// and a row-cells iterator. All three are reused across frames to avoid
// per-frame allocation.
type RenderState struct {
	c       C.GhosttyRenderState
	rowIter C.GhosttyRenderStateRowIterator
	cells   C.GhosttyRenderStateRowCells
}

// NewRenderState allocates a render state plus a reusable row iterator and
// cells iterator. Free with Close.
func NewRenderState() (*RenderState, error) {
	rs := &RenderState{}
	if rc := C.ghostty_render_state_new(nil, &rs.c); rc != C.GHOSTTY_SUCCESS {
		return nil, fmt.Errorf("ghostty_render_state_new: %d", int(rc))
	}
	if rc := C.ghostty_render_state_row_iterator_new(nil, &rs.rowIter); rc != C.GHOSTTY_SUCCESS {
		C.ghostty_render_state_free(rs.c)
		return nil, fmt.Errorf("row_iterator_new: %d", int(rc))
	}
	if rc := C.ghostty_render_state_row_cells_new(nil, &rs.cells); rc != C.GHOSTTY_SUCCESS {
		C.ghostty_render_state_row_iterator_free(rs.rowIter)
		C.ghostty_render_state_free(rs.c)
		return nil, fmt.Errorf("row_cells_new: %d", int(rc))
	}
	runtime.SetFinalizer(rs, func(rs *RenderState) { rs.Close() })
	return rs, nil
}

// Close frees the render state and its iterators.
func (rs *RenderState) Close() {
	if rs.cells != nil {
		C.ghostty_render_state_row_cells_free(rs.cells)
		rs.cells = nil
	}
	if rs.rowIter != nil {
		C.ghostty_render_state_row_iterator_free(rs.rowIter)
		rs.rowIter = nil
	}
	if rs.c != nil {
		C.ghostty_render_state_free(rs.c)
		rs.c = nil
	}
	runtime.SetFinalizer(rs, nil)
}

// Update snapshots the terminal state into this render state. Call once
// per frame before walking. Must be called from the same thread as the
// terminal.
func (rs *RenderState) Update(t *Terminal) error {
	if rc := C.ghostty_render_state_update(rs.c, t.c); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("render_state_update: %d", int(rc))
	}
	return nil
}

// DefaultColors returns the terminal's current default fg and bg colors.
func (rs *RenderState) DefaultColors() (fg, bg ColorRGB, err error) {
	var c C.GhosttyRenderStateColors
	C.roost_init_render_colors(&c)
	if rc := C.ghostty_render_state_colors_get(rs.c, &c); rc != C.GHOSTTY_SUCCESS {
		return ColorRGB{}, ColorRGB{}, fmt.Errorf("colors_get: %d", int(rc))
	}
	return ColorRGB{uint8(c.foreground.r), uint8(c.foreground.g), uint8(c.foreground.b)},
		ColorRGB{uint8(c.background.r), uint8(c.background.g), uint8(c.background.b)},
		nil
}

// Walk invokes fn(row, col, cell) for every populated cell in the current
// snapshot. Empty cells with no background are skipped silently. Empty
// cells with a background are emitted with Codepoint == 0 and HasBG set.
//
// The renderer is expected to draw cells at (col*cellW, row*cellH) using
// fixed monospace cell sizes. fn must not call any RenderState methods.
func (rs *RenderState) Walk(fn func(row, col int, cell Cell)) error {
	// Reset the row iterator from the latest update.
	rowIterPtr := unsafe.Pointer(&rs.rowIter)
	if rc := C.ghostty_render_state_get(rs.c, C.GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, rowIterPtr); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("get ROW_ITERATOR: %d", int(rc))
	}

	row := -1
	for C.ghostty_render_state_row_iterator_next(rs.rowIter) {
		row++

		cellsPtr := unsafe.Pointer(&rs.cells)
		if rc := C.ghostty_render_state_row_get(rs.rowIter, C.GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, cellsPtr); rc != C.GHOSTTY_SUCCESS {
			continue
		}

		col := -1
		for C.ghostty_render_state_row_cells_next(rs.cells) {
			col++

			var graphLen C.uint32_t
			C.ghostty_render_state_row_cells_get(rs.cells,
				C.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
				unsafe.Pointer(&graphLen))

			var cell Cell

			// Background — even empty cells may have a bg (e.g. erase-with-color).
			var bg C.GhosttyColorRgb
			if rc := C.ghostty_render_state_row_cells_get(rs.cells,
				C.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
				unsafe.Pointer(&bg)); rc == C.GHOSTTY_SUCCESS {
				cell.HasBG = true
				cell.BG = ColorRGB{uint8(bg.r), uint8(bg.g), uint8(bg.b)}
			}

			if graphLen == 0 {
				if cell.HasBG {
					fn(row, col, cell)
				}
				continue
			}

			// Read the base codepoint (we ignore extra grapheme codepoints in
			// the spike — emoji/ZWJ render as the base char only).
			cps := make([]C.uint32_t, graphLen)
			C.ghostty_render_state_row_cells_get(rs.cells,
				C.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
				unsafe.Pointer(&cps[0]))
			cell.Codepoint = rune(cps[0])

			// Foreground.
			var fg C.GhosttyColorRgb
			if rc := C.ghostty_render_state_row_cells_get(rs.cells,
				C.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
				unsafe.Pointer(&fg)); rc == C.GHOSTTY_SUCCESS {
				cell.HasFG = true
				cell.FG = ColorRGB{uint8(fg.r), uint8(fg.g), uint8(fg.b)}
			}

			// Style (only the flags we care about).
			var style C.GhosttyStyle
			C.roost_init_style(&style)
			C.ghostty_render_state_row_cells_get(rs.cells,
				C.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
				unsafe.Pointer(&style))
			cell.Bold = bool(style.bold)
			cell.Italic = bool(style.italic)
			cell.Inverse = bool(style.inverse)

			fn(row, col, cell)
		}
	}
	return nil
}

// CursorPos returns the cursor's viewport position. Returns (0,0,false)
// if the cursor isn't currently in the viewport (scrolled out of view).
func (rs *RenderState) CursorPos() (col, row int, visible bool) {
	var hasValue C.bool
	C.ghostty_render_state_get(rs.c,
		C.GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE,
		unsafe.Pointer(&hasValue))
	if !bool(hasValue) {
		return 0, 0, false
	}

	var visibleC C.bool
	C.ghostty_render_state_get(rs.c,
		C.GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE,
		unsafe.Pointer(&visibleC))

	var x, y C.uint16_t
	C.ghostty_render_state_get(rs.c,
		C.GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X,
		unsafe.Pointer(&x))
	C.ghostty_render_state_get(rs.c,
		C.GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y,
		unsafe.Pointer(&y))
	return int(x), int(y), bool(visibleC)
}
