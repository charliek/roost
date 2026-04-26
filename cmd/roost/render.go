package main

import (
	"log/slog"

	"github.com/diamondburned/gotk4/pkg/cairo"
	"github.com/diamondburned/gotk4/pkg/pangocairo"

	"github.com/charliek/roost/internal/ghostty"
)

// drawTerminal walks the session's render state and paints the cell
// grid into the Cairo context. Called from GtkDrawingArea's draw
// function on the main thread.
func drawTerminal(cr *cairo.Context, s *Session) {
	defaultFG, defaultBG, err := s.rs.DefaultColors()
	if err != nil {
		// First frame, before any update.
		cr.SetSourceRGB(0.07, 0.08, 0.10)
		cr.Paint()
		return
	}

	setRGB(cr, defaultBG)
	cr.Paint()

	cellW := s.cellW
	cellH := s.cellH

	layout := pangocairo.CreateLayout(cr)
	layout.SetFontDescription(s.font)

	textBuf := make([]byte, 0, 8)

	// Cursor cell info captured during the walk; drawn *after* the
	// walk so the inverted block goes on top of the underlying glyph.
	cx, cy, cursorVisible := s.rs.CursorPos()
	var cursorCodepoint rune
	cursorHasCell := false
	cursorBold := false

	if err := s.rs.Walk(func(row, col int, cell ghostty.Cell) {
		x := pad + float64(col*cellW)
		y := pad + float64(row*cellH)

		fg := defaultFG
		bg := defaultBG
		if cell.HasFG {
			fg = cell.FG
		}
		hasExplicitBG := cell.HasBG
		if cell.HasBG {
			bg = cell.BG
		}
		if cell.Inverse {
			fg, bg = bg, fg
			hasExplicitBG = true
		}

		if hasExplicitBG {
			setRGB(cr, bg)
			cr.Rectangle(x, y, float64(cellW), float64(cellH))
			cr.Fill()
		}
		if cursorVisible && row == cy && col == cx && cell.Codepoint != 0 {
			cursorCodepoint = cell.Codepoint
			cursorHasCell = true
			cursorBold = cell.Bold
		}
		if cell.Codepoint == 0 {
			return
		}

		if cell.Bold {
			layout.SetFontDescription(s.fontBold)
		}
		textBuf = textBuf[:0]
		textBuf = appendRune(textBuf, cell.Codepoint)
		layout.SetText(string(textBuf))
		setRGB(cr, fg)
		cr.MoveTo(x, y)
		pangocairo.ShowLayout(cr, layout)
		if cell.Bold {
			layout.SetFontDescription(s.font)
		}
	}); err != nil {
		// libghostty's row iterator failed. Frame so far is partial;
		// no fallback paint — surfacing the error in the log is what
		// we actually want, since this should never happen in normal
		// operation.
		slog.Warn("render-state walk", "err", err)
	}

	if cursorVisible {
		x := pad + float64(cx*cellW)
		y := pad + float64(cy*cellH)
		w := float64(cellW)
		h := float64(cellH)

		switch {
		case !s.windowFocused:
			// Unfocused: hollow outline only, no blink.
			setRGB(cr, defaultFG)
			cr.SetLineWidth(1)
			cr.Rectangle(x+0.5, y+0.5, w-1, h-1)
			cr.Stroke()
		case s.cursorOn:
			// Focused + on phase: solid block in FG, glyph in BG.
			setRGB(cr, defaultFG)
			cr.Rectangle(x, y, w, h)
			cr.Fill()
			if cursorHasCell {
				textBuf = textBuf[:0]
				textBuf = appendRune(textBuf, cursorCodepoint)
				if cursorBold {
					layout.SetFontDescription(s.fontBold)
				} else {
					layout.SetFontDescription(s.font)
				}
				layout.SetText(string(textBuf))
				setRGB(cr, defaultBG)
				cr.MoveTo(x, y)
				pangocairo.ShowLayout(cr, layout)
			}
		}
		// Focused + off phase: draw nothing — the underlying cell
		// already painted in the walk above shows through.
	}
}

func setRGB(cr *cairo.Context, c ghostty.ColorRGB) {
	cr.SetSourceRGB(float64(c.R)/255.0, float64(c.G)/255.0, float64(c.B)/255.0)
}

// appendRune is an allocation-light utf-8 encoder. Saves a string()
// allocation per cell relative to []byte(string(r)).
func appendRune(buf []byte, r rune) []byte {
	switch {
	case r < 0x80:
		return append(buf, byte(r))
	case r < 0x800:
		return append(buf, byte(0xC0|r>>6), byte(0x80|r&0x3F))
	case r < 0x10000:
		return append(buf, byte(0xE0|r>>12), byte(0x80|(r>>6)&0x3F), byte(0x80|r&0x3F))
	default:
		return append(buf, byte(0xF0|r>>18), byte(0x80|(r>>12)&0x3F), byte(0x80|(r>>6)&0x3F), byte(0x80|r&0x3F))
	}
}
