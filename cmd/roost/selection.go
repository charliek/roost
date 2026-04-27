package main

// selection is a viewport-relative cell range built from a mouse drag.
//
// Coordinates are cell-grid (col, row) within the visible viewport,
// inclusive at both endpoints. Selections are short-lived: cleared on
// PTY output mutating any selected row, on resize, and on a new drag
// start. That keeps us out of the business of tracking absolute
// scrollback row IDs through scroll/rotate.
type selection struct {
	active bool

	// anchor is where the drag started; current is where it is now.
	// Either may be "earlier" in row-major order — normalize() returns
	// the topological start/end.
	anchorCol, anchorRow int
	currentCol, currentRow int
}

func (s *selection) start(col, row int) {
	s.active = true
	s.anchorCol = col
	s.anchorRow = row
	s.currentCol = col
	s.currentRow = row
}

func (s *selection) update(col, row int) {
	if !s.active {
		return
	}
	s.currentCol = col
	s.currentRow = row
}

func (s *selection) clear() {
	s.active = false
}

// empty is true for a selection that's inactive or covers a single
// cell that the user hasn't moved off of (no real drag yet).
func (s *selection) empty() bool {
	return !s.active || (s.anchorCol == s.currentCol && s.anchorRow == s.currentRow)
}

// normalized returns the start (top-left) and end (bottom-right) of
// the selection in row-major order.
func (s *selection) normalized() (sCol, sRow, eCol, eRow int) {
	sCol, sRow = s.anchorCol, s.anchorRow
	eCol, eRow = s.currentCol, s.currentRow
	if eRow < sRow || (eRow == sRow && eCol < sCol) {
		sCol, eCol = eCol, sCol
		sRow, eRow = eRow, sRow
	}
	return
}

// touches reports whether the selection covers any row in [minRow,
// maxRow]. Used to decide whether incoming PTY output should clear
// the selection.
func (s *selection) touches(minRow, maxRow int) bool {
	if !s.active {
		return false
	}
	sCol, sRow, _, eRow := s.normalized()
	_ = sCol
	return !(eRow < minRow || sRow > maxRow)
}

// ribbonRect describes one of the (up to three) rectangles that make
// up the selection ribbon. Coordinates are pixels, ready to feed to
// Cairo.
type ribbonRect struct {
	X, Y, W, H float64
}

// ribbonRects converts the normalized selection into pixel rectangles
// for the renderer. Returns at most three rects:
//   - Single-row selection: one rect from sCol to eCol.
//   - Multi-row: first-row partial (sCol → cols), middle full-width
//     block (every row sRow+1 .. eRow-1), last-row partial (0 → eCol).
//
// cols is the number of cells per row; cellW/cellH are pixel dims;
// padX/padY are the renderer's left/top padding.
func (s *selection) ribbonRects(cols, cellW, cellH int, padX, padY float64) []ribbonRect {
	if s.empty() {
		return nil
	}
	sCol, sRow, eCol, eRow := s.normalized()
	// Make end-column exclusive so width math doesn't off-by-one when
	// the user clicks the very right edge.
	eCol++

	cw := float64(cellW)
	ch := float64(cellH)

	switch {
	case sRow == eRow:
		return []ribbonRect{{
			X: padX + float64(sCol)*cw,
			Y: padY + float64(sRow)*ch,
			W: float64(eCol-sCol) * cw,
			H: ch,
		}}

	default:
		out := make([]ribbonRect, 0, 3)
		// First row: from start col to right edge.
		out = append(out, ribbonRect{
			X: padX + float64(sCol)*cw,
			Y: padY + float64(sRow)*ch,
			W: float64(cols-sCol) * cw,
			H: ch,
		})
		// Middle rows: full width.
		if eRow-sRow > 1 {
			out = append(out, ribbonRect{
				X: padX,
				Y: padY + float64(sRow+1)*ch,
				W: float64(cols) * cw,
				H: float64(eRow-sRow-1) * ch,
			})
		}
		// Last row: from left edge to end col.
		out = append(out, ribbonRect{
			X: padX,
			Y: padY + float64(eRow)*ch,
			W: float64(eCol) * cw,
			H: ch,
		})
		return out
	}
}
