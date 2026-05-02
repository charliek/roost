package main

import (
	"log/slog"

	"github.com/diamondburned/gotk4/pkg/cairo"
	"github.com/diamondburned/gotk4/pkg/pango"
	"github.com/diamondburned/gotk4/pkg/pangocairo"

	"github.com/charliek/roost/internal/ghostty"
)

// showGlyphLayout positions the layout at (x, y) and paints it. When
// thicken is true the glyph is painted again at (x+0.5, y) — a
// poor-man's stem darkening that approximates Apple's Core Text
// behavior on rendering pipelines (notably Cairo on macOS) that don't
// apply the darkening natively.
func showGlyphLayout(cr *cairo.Context, layout *pango.Layout, x, y float64, thicken bool) {
	cr.MoveTo(x, y)
	pangocairo.ShowLayout(cr, layout)
	if thicken {
		cr.MoveTo(x+0.5, y)
		pangocairo.ShowLayout(cr, layout)
	}
}

// drawTerminal walks the session's render state and paints the cell
// grid into the Cairo context. Called from GtkDrawingArea's draw
// function on the main thread.
func drawTerminal(cr *cairo.Context, s *Session) {
	defaultFG, defaultBG, err := s.rs.DefaultColors()
	if err != nil {
		// First frame, before any update. Match the session theme's
		// background so there is no flash when the first VT bytes
		// arrive.
		setRGB(cr, s.theme.Background)
		cr.Paint()
		return
	}
	boldColor := s.theme.BoldColor
	cursorText := s.theme.CursorText

	setRGB(cr, defaultBG)
	cr.Paint()

	cellW := s.cellW
	cellH := s.cellH

	layout := pangocairo.CreateLayout(cr)
	layout.SetFontDescription(s.font)
	if s.fontFeaturesAttr != nil {
		layout.SetAttributes(s.fontFeaturesAttr)
	}

	textBuf := make([]byte, 0, 8)

	// Cursor cell info captured during the glyph pass; drawn *after* the
	// walks so the inverted block goes on top of the underlying glyph.
	cx, cy, cursorVisible := s.rs.CursorPos()
	var cursorCodepoint rune
	cursorHasCell := false
	cursorBold := false

	// Pass A — backgrounds. Painted first across the whole frame so any
	// glyph in Pass B whose descender ink extends into row N+1 lands on
	// top of the next row's BG fill, instead of being painted before
	// (and then overwritten by) it. Without this split, descenders
	// inside multi-row prompt boxes (opencode, codex) would be clipped
	// by the next row's gray BG.
	if err := s.rs.Walk(func(row, col int, cell ghostty.Cell) {
		_, bg, hasExplicitBG := cellColors(cell, defaultFG, defaultBG, boldColor)
		if !hasExplicitBG {
			return
		}
		x := pad + float64(col*cellW)
		y := pad + float64(row*cellH)
		setRGB(cr, bg)
		cr.Rectangle(x, y, float64(cellW), float64(cellH))
		cr.Fill()
	}); err != nil {
		slog.Warn("render-state walk (bg)", "err", err)
	}

	// Pass B — glyphs + cursor capture.
	if err := s.rs.Walk(func(row, col int, cell ghostty.Cell) {
		x := pad + float64(col*cellW)
		y := pad + float64(row*cellH)
		fg, _, _ := cellColors(cell, defaultFG, defaultBG, boldColor)

		if cursorVisible && row == cy && col == cx && cell.Codepoint != 0 {
			cursorCodepoint = cell.Codepoint
			cursorHasCell = true
			cursorBold = cell.Bold
		}
		if cell.Codepoint == 0 {
			return
		}

		// Box-drawing and block elements get a custom geometric renderer
		// so they tile pixel-perfectly across cells; Pango fonts produce
		// visible seams in TUI chrome (Codex/Claude/OpenCode).
		if drawCellSprite(cr, x, y, float64(cellW), float64(cellH), fg, cell.Codepoint) {
			return
		}

		if cell.Bold {
			layout.SetFontDescription(s.fontBold)
		}
		textBuf = textBuf[:0]
		textBuf = appendRune(textBuf, cell.Codepoint)
		layout.SetText(string(textBuf))
		setRGB(cr, fg)
		showGlyphLayout(cr, layout, x, y+float64(s.glyphYOffset), s.fontCfg.FontThicken)
		if cell.Bold {
			layout.SetFontDescription(s.font)
		}
	}); err != nil {
		slog.Warn("render-state walk (glyphs)", "err", err)
	}

	// Selection overlay (Ghostty/iTerm style). Drawn after the cell
	// pass so text underneath stays visible, and before the cursor
	// pass so the cursor remains opaque inside the selection.
	if !s.sel.empty() {
		rects := s.sel.ribbonRects(int(s.cols), s.cellW, s.cellH, pad, pad)
		sb := s.theme.SelectionBackground
		cr.SetSourceRGBA(float64(sb.R)/255, float64(sb.G)/255, float64(sb.B)/255, 0.35)
		for _, r := range rects {
			cr.Rectangle(r.X, r.Y, r.W, r.H)
			cr.Fill()
		}
	}

	if cursorVisible {
		x := pad + float64(cx*cellW)
		y := pad + float64(cy*cellH)
		w := float64(cellW)
		h := float64(cellH)

		switch {
		case !s.windowFocused:
			// Unfocused: hollow outline only, no blink. Outlined in
			// the theme's cursor color so the user can still find it
			// against arbitrary text colors.
			setRGB(cr, s.theme.Cursor)
			cr.SetLineWidth(1)
			cr.Rectangle(x+0.5, y+0.5, w-1, h-1)
			cr.Stroke()
		case s.cursorOn:
			// Focused + on phase: solid block in cursor color, glyph
			// in cursor-text. cursor-text falls back to background if
			// the theme didn't set it (parseTheme handles the default).
			setRGB(cr, s.theme.Cursor)
			cr.Rectangle(x, y, w, h)
			cr.Fill()
			if cursorHasCell {
				if !drawCellSprite(cr, x, y, w, h, cursorText, cursorCodepoint) {
					textBuf = textBuf[:0]
					textBuf = appendRune(textBuf, cursorCodepoint)
					if cursorBold {
						layout.SetFontDescription(s.fontBold)
					} else {
						layout.SetFontDescription(s.font)
					}
					layout.SetText(string(textBuf))
					setRGB(cr, cursorText)
					showGlyphLayout(cr, layout, x, y+float64(s.glyphYOffset), s.fontCfg.FontThicken)
				}
			}
		}
		// Focused + off phase: draw nothing — the underlying cell
		// already painted in the walk above shows through.
	}
}

func setRGB(cr *cairo.Context, c ghostty.ColorRGB) {
	cr.SetSourceRGB(float64(c.R)/255.0, float64(c.G)/255.0, float64(c.B)/255.0)
}

// cellColors resolves a cell's effective fg/bg, applying SGR inverse,
// and reports whether a BG fill is required (true if the cell has an
// explicit BG or is inverted; false for plain default-colour cells, so
// the canvas-wide default-bg paint stays visible).
//
// boldColor is applied only when the cell is bold AND has no explicit
// fg AND is not inverted. The "no explicit fg" check matches the rule
// every terminal honors: bold red text stays red; only bold default-fg
// text gets the bold accent color. Cell.HasFG is the precise signal —
// internal/ghostty/render.go sets it only when libghostty returns an
// explicit FG, not when it falls back to default. The "not inverted"
// check happens AFTER the inverse swap so boldColor never lands as a
// background fill.
func cellColors(cell ghostty.Cell, defaultFG, defaultBG, boldColor ghostty.ColorRGB) (fg, bg ghostty.ColorRGB, hasExplicitBG bool) {
	fg = defaultFG
	bg = defaultBG
	if cell.HasFG {
		fg = cell.FG
	}
	if cell.HasBG {
		bg = cell.BG
	}
	hasExplicitBG = cell.HasBG
	if cell.Inverse {
		fg, bg = bg, fg
		hasExplicitBG = true
	}
	if cell.Bold && !cell.HasFG && !cell.Inverse {
		fg = boldColor
	}
	return
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
