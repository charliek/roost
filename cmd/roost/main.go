// Command roost is the GUI binary. Phase 0 spike: one window, one tab,
// one libghostty-vt terminal driven by a single PTY-spawned shell, drawn
// with Cairo on a GtkDrawingArea.
package main

import (
	"io"
	"log"
	"os"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	"github.com/diamondburned/gotk4/pkg/cairo"
	"github.com/diamondburned/gotk4/pkg/core/glib"
	"github.com/diamondburned/gotk4/pkg/gdk/v4"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"
	"github.com/diamondburned/gotk4/pkg/pango"
	"github.com/diamondburned/gotk4/pkg/pangocairo"

	"github.com/charliek/roost/internal/ghostty"
	"github.com/charliek/roost/internal/pty"
)

const (
	initialCols = 80
	initialRows = 24
	fontFamily  = "Monaco"
	fontSizePt  = 12
	pad         = 4
)

// session holds everything for one terminal: PTY, libghostty terminal,
// render state, and the cell-size metrics the renderer measured. There's
// only one of these in the spike; multi-tab moves it into core.
type session struct {
	pty   *pty.PTY
	term  *ghostty.Terminal
	rs    *ghostty.RenderState
	cellW float64
	cellH float64
	font  *pango.FontDescription
	cols  uint16
	rows  uint16
}

func main() {
	app := adw.NewApplication("dev.charliek.roost", 0)
	app.ConnectActivate(func() { activate(app) })
	if code := app.Run(os.Args); code > 0 {
		log.Fatalf("roost exited with code %d", code)
	}
}

func activate(app *adw.Application) {
	win := adw.NewApplicationWindow(&app.Application)
	win.SetTitle("Roost")
	win.SetDefaultSize(900, 600)

	header := adw.NewHeaderBar()

	surface := gtk.NewDrawingArea()
	surface.SetHExpand(true)
	surface.SetVExpand(true)
	surface.SetCanFocus(true)
	surface.SetFocusable(true)
	surface.SetFocusOnClick(true)

	sess, err := newSession(initialCols, initialRows)
	if err != nil {
		log.Fatalf("newSession: %v", err)
	}

	// Measure cell size via Pango at startup. Renderer assumes a
	// monospace font and uniform cell width.
	measureCells(sess, surface)

	surface.SetDrawFunc(func(_ *gtk.DrawingArea, cr *cairo.Context, _, _ int) {
		drawTerminal(cr, sess)
	})

	// Drain PTY in a goroutine, marshal bytes to the main thread for
	// vt_write + render_state_update + queue_draw.
	go pumpPTY(sess, surface)

	// Keyboard.
	keyCtrl := gtk.NewEventControllerKey()
	keyCtrl.ConnectKeyPressed(func(keyval, _ uint, mods gdk.ModifierType) bool {
		return handleKey(sess, keyval, mods)
	})
	surface.AddController(keyCtrl)

	// Click to focus the surface so keys are routed to it.
	clickCtrl := gtk.NewGestureClick()
	clickCtrl.ConnectPressed(func(_ int, _, _ float64) {
		surface.GrabFocus()
	})
	surface.AddController(clickCtrl)

	box := gtk.NewBox(gtk.OrientationVertical, 0)
	box.Append(header)
	box.Append(surface)
	win.SetContent(box)

	// Free libghostty/PTY resources when the window closes.
	win.ConnectCloseRequest(func() bool {
		_ = sess.pty.Close()
		sess.rs.Close()
		sess.term.Close()
		return false
	})

	win.Present()
	surface.GrabFocus()
}

func newSession(cols, rows uint16) (*session, error) {
	term, err := ghostty.NewTerminal(ghostty.Options{Cols: cols, Rows: rows, MaxScrollback: 2000})
	if err != nil {
		return nil, err
	}
	rs, err := ghostty.NewRenderState()
	if err != nil {
		term.Close()
		return nil, err
	}
	p, err := pty.SpawnShell("", cols, rows)
	if err != nil {
		rs.Close()
		term.Close()
		return nil, err
	}

	font := pango.NewFontDescription()
	font.SetFamily(fontFamily)
	font.SetSize(fontSizePt * pango.SCALE)

	return &session{pty: p, term: term, rs: rs, font: font, cols: cols, rows: rows}, nil
}

// measureCells uses a temporary Pango layout to size one monospace cell.
// Stored on the session and used by both layout and rendering.
func measureCells(s *session, da *gtk.DrawingArea) {
	ctx := da.PangoContext()
	layout := pango.NewLayout(ctx)
	layout.SetFontDescription(s.font)
	layout.SetText("M")
	w, h := layout.PixelSize()
	if w < 1 {
		w = 8
	}
	if h < 1 {
		h = 16
	}
	s.cellW = float64(w)
	s.cellH = float64(h)
}

// pumpPTY runs in a goroutine. It reads bytes from the PTY and, for each
// chunk, marshals to the GTK main thread via glib.IdleAdd, where it's
// safe to touch libghostty + GTK. Exits on EOF.
func pumpPTY(s *session, da *gtk.DrawingArea) {
	buf := make([]byte, 4096)
	for {
		n, err := s.pty.Read(buf)
		if n > 0 {
			chunk := append([]byte{}, buf[:n]...)
			glib.IdleAdd(func() {
				s.term.VTWrite(chunk)
				_ = s.rs.Update(s.term)
				da.QueueDraw()
			})
		}
		if err != nil {
			if err != io.EOF {
				log.Printf("pty read: %v", err)
			}
			return
		}
	}
}

// drawTerminal paints the entire visible terminal grid. Runs on the GTK
// main thread inside the GtkDrawingArea draw callback.
func drawTerminal(cr *cairo.Context, s *session) {
	defaultFG, defaultBG, err := s.rs.DefaultColors()
	if err != nil {
		// First frame, before any update: just clear.
		cr.SetSourceRGB(0.07, 0.08, 0.10)
		cr.Paint()
		return
	}

	// Background fill.
	setRGB(cr, defaultBG)
	cr.Paint()

	cellW := s.cellW
	cellH := s.cellH

	// One Pango layout reused for every cell. Cheaper than a new layout
	// per call; correctness is the same since we only render one glyph
	// at a time.
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
			// Cheap fake-bold: redraw 1px right.
			cr.MoveTo(x+1, y)
			pangocairo.ShowLayout(cr, layout)
		}
	})

	// Cursor (a hollow rect for the spike).
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

// appendRune is a small allocation-free utf-8 encoder that doesn't pull
// in unicode/utf8 just for one rune per cell. Buf is grown as needed.
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
