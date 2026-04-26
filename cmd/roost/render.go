package main

import (
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

	_ = s.rs.Walk(func(row, col int, cell ghostty.Cell) {
		x := pad + float64(col)*cellW
		y := pad + float64(row)*cellH

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
			cr.Rectangle(x, y, cellW, cellH)
			cr.Fill()
		}
		if cell.Codepoint == 0 {
			return
		}

		textBuf = textBuf[:0]
		textBuf = appendRune(textBuf, cell.Codepoint)
		layout.SetText(string(textBuf))
		setRGB(cr, fg)
		cr.MoveTo(x, y)
		pangocairo.ShowLayout(cr, layout)

		if cell.Bold {
			cr.MoveTo(x+1, y)
			pangocairo.ShowLayout(cr, layout)
		}
	})

	if cx, cy, visible := s.rs.CursorPos(); visible {
		x := pad + float64(cx)*cellW
		y := pad + float64(cy)*cellH
		setRGB(cr, defaultFG)
		cr.SetLineWidth(1)
		cr.Rectangle(x+0.5, y+0.5, cellW-1, cellH-1)
		cr.Stroke()
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
