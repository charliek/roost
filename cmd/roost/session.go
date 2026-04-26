package main

import (
	"io"
	"log/slog"
	"sync/atomic"

	"github.com/diamondburned/gotk4/pkg/cairo"
	"github.com/diamondburned/gotk4/pkg/core/glib"
	"github.com/diamondburned/gotk4/pkg/gdk/v4"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"
	"github.com/diamondburned/gotk4/pkg/pango"

	"github.com/charliek/roost/internal/core"
	"github.com/charliek/roost/internal/ghostty"
	"github.com/charliek/roost/internal/pty"
)

// Session is one tab's runtime state: the persistent record from core
// plus the PTY, libghostty terminal, render state, and the drawing area
// that hosts it. One Session per open tab; alive even when not visible
// (its PTY keeps draining so output isn't lost).
type Session struct {
	ws  *core.Workspace
	tab core.Tab

	pty  *pty.PTY
	term *ghostty.Terminal
	rs   *ghostty.RenderState
	da   *gtk.DrawingArea

	font  *pango.FontDescription
	cellW float64
	cellH float64
	cols  uint16
	rows  uint16

	// closed is set to true when Close has been called. The PTY-pump
	// goroutine's queued IdleAdd callbacks check it before touching
	// libghostty state, so callbacks already in the GTK queue at the
	// time of Close become no-ops instead of use-after-free.
	closed atomic.Bool

	// pumpDone closes when the PTY-pump goroutine exits, letting Close
	// wait until all IdleAdd callbacks from pump have been queued
	// before scheduling its own cleanup callback at the end.
	pumpDone chan struct{}
}

// NewSession spawns a shell at the tab's persisted cwd, allocates a
// libghostty terminal + render state, builds the DrawingArea, and
// starts the PTY-pump goroutine. Caller adds da to a parent widget.
func NewSession(ws *core.Workspace, tab core.Tab, cols, rows uint16) (*Session, error) {
	term, err := ghostty.NewTerminal(ghostty.Options{
		Cols: cols, Rows: rows, MaxScrollback: 2000,
	})
	if err != nil {
		return nil, err
	}
	rs, err := ghostty.NewRenderState()
	if err != nil {
		term.Close()
		return nil, err
	}
	p, err := pty.SpawnShell(tab.CWD, cols, rows)
	if err != nil {
		rs.Close()
		term.Close()
		return nil, err
	}

	font := pango.NewFontDescription()
	font.SetFamily(fontFamily)
	font.SetSize(fontSizePt * pango.SCALE)

	s := &Session{
		ws: ws, tab: tab,
		pty: p, term: term, rs: rs,
		font:     font,
		cols:     cols,
		rows:     rows,
		pumpDone: make(chan struct{}),
	}

	s.da = gtk.NewDrawingArea()
	s.da.SetHExpand(true)
	s.da.SetVExpand(true)
	s.da.SetCanFocus(true)
	s.da.SetFocusable(true)
	s.da.SetFocusOnClick(true)
	s.measureCells()
	s.da.SetDrawFunc(func(_ *gtk.DrawingArea, cr *cairo.Context, _, _ int) {
		drawTerminal(cr, s)
	})
	s.da.ConnectResize(s.onResize)

	keyCtrl := gtk.NewEventControllerKey()
	keyCtrl.ConnectKeyPressed(func(keyval, _ uint, state gdk.ModifierType) (ok bool) {
		return handleKey(s, keyval, uint(state))
	})
	s.da.AddController(keyCtrl)

	clickCtrl := gtk.NewGestureClick()
	clickCtrl.ConnectPressed(func(_ int, _, _ float64) {
		s.da.GrabFocus()
	})
	s.da.AddController(clickCtrl)

	go s.pumpPTY()
	return s, nil
}

// DrawingArea returns the widget to host inside a tab page.
func (s *Session) DrawingArea() *gtk.DrawingArea { return s.da }

// Tab returns the persistent tab record this session is bound to.
func (s *Session) Tab() core.Tab { return s.tab }

// Close stops the PTY pump, kills the child shell, and frees libghostty
// resources in the correct order: cancel pump → close PTY (forces pump's
// blocking Read to return) → wait for pump exit → queue final IdleAdd
// for libghostty cleanup so it runs after any IdleAdd callbacks pump
// already queued.
func (s *Session) Close() {
	if !s.closed.CompareAndSwap(false, true) {
		return
	}
	_ = s.pty.Close()
	<-s.pumpDone
	// At this point no more IdleAdd callbacks will be queued from the
	// pump, but already-queued ones may still run. Schedule libghostty
	// cleanup to run at the tail of the GTK queue (FIFO at default prio),
	// so it lands after any pending vt_write callbacks.
	rs := s.rs
	term := s.term
	glib.IdleAdd(func() {
		rs.Close()
		term.Close()
	})
}

// pumpPTY runs in a goroutine. Each chunk of PTY output is marshalled
// to the GTK main thread, where vt_write + render_state_update +
// QueueDraw all run. Exits on EOF/error from the PTY.
func (s *Session) pumpPTY() {
	defer close(s.pumpDone)
	buf := make([]byte, 4096)
	for {
		n, err := s.pty.Read(buf)
		if n > 0 {
			chunk := append([]byte{}, buf[:n]...)
			glib.IdleAdd(func() {
				if s.closed.Load() {
					return
				}
				s.term.VTWrite(chunk)
				_ = s.rs.Update(s.term)
				s.da.QueueDraw()
			})
		}
		if err != nil {
			if err != io.EOF {
				slog.Warn("pty read", "tab_id", s.tab.ID, "err", err)
			}
			return
		}
	}
}

// measureCells uses Pango to size one monospace cell. Called once at
// construction; the renderer reads cellW/cellH on every draw.
func (s *Session) measureCells() {
	ctx := s.da.PangoContext()
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

// onResize reflows the terminal to fit the new pixel dimensions. Called
// by GtkDrawingArea every time it's allocated a new size.
func (s *Session) onResize(width, height int) {
	if s.closed.Load() {
		return
	}
	cols := uint16((float64(width) - 2*pad) / s.cellW)
	rows := uint16((float64(height) - 2*pad) / s.cellH)
	if cols < 1 {
		cols = 1
	}
	if rows < 1 {
		rows = 1
	}
	if cols == s.cols && rows == s.rows {
		return
	}
	s.cols = cols
	s.rows = rows
	if err := s.term.Resize(cols, rows, uint32(s.cellW), uint32(s.cellH)); err != nil {
		slog.Warn("ghostty resize", "tab_id", s.tab.ID, "err", err)
	}
	if err := s.pty.Resize(cols, rows, uint16(s.cellW), uint16(s.cellH)); err != nil {
		slog.Warn("pty resize", "tab_id", s.tab.ID, "err", err)
	}
	_ = s.rs.Update(s.term)
	s.da.QueueDraw()
}
