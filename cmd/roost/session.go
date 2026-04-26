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
	"github.com/charliek/roost/internal/osc"
	"github.com/charliek/roost/internal/pty"
)

// blinkPeriodMs is the cursor blink half-period in milliseconds. 530ms
// matches xterm/iTerm; faster reads as jittery, slower reads as dead.
const blinkPeriodMs = 530

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

	font     *pango.FontDescription
	fontBold *pango.FontDescription
	cellW    int
	cellH    int
	cols     uint16
	rows     uint16

	// cursorOn is the current blink phase. windowFocused mirrors the
	// containing window's is-active so the cursor stops blinking and
	// degrades to a hollow outline when the window is unfocused.
	// blinkSrc is the timeout source ID, removed in Close.
	cursorOn      bool
	windowFocused bool
	blinkSrc      glib.SourceHandle

	// closed is set to true when Close has been called. The PTY-pump
	// goroutine's queued IdleAdd callbacks check it before touching
	// libghostty state, so callbacks already in the GTK queue at the
	// time of Close become no-ops instead of use-after-free.
	closed atomic.Bool

	// pumpDone closes when the PTY-pump goroutine exits, letting Close
	// wait until all IdleAdd callbacks from pump have been queued
	// before scheduling its own cleanup callback at the end.
	pumpDone chan struct{}

	// onTitleChanged is invoked on the GTK main thread when the
	// terminal's OSC-set title changes. The App wires this up to
	// refresh the AdwTabPage title and persist via core.Workspace.
	onTitleChanged func(string)

	// lastTitle is the most recent title we surfaced to the App, used
	// to debounce changes (vt_write may run hundreds of times per
	// second; we only want to fire when the title actually changes).
	lastTitle string

	// lastPWD mirrors lastTitle for the cwd reported via OSC 7.
	lastPWD string

	// onPWDChanged is invoked on the main thread on cwd updates.
	onPWDChanged func(string)

	// onPTYExit fires once when the PTY's read loop hits EOF (the
	// child shell exited). Used by App to close the tab page so the
	// view doesn't show a frozen post-exit screen.
	onPTYExit func()

	// osc is the streaming OSC scanner used as a fallback notification
	// path. Fed from the pump goroutine in parallel with vt_write.
	osc *osc.Scanner
}

// NewSession spawns a shell at the tab's persisted cwd, allocates a
// libghostty terminal + render state, builds the DrawingArea, and
// starts the PTY-pump goroutine. Caller adds da to a parent widget.
//
// extraEnv is forwarded to pty.SpawnShell so callers can inject
// ROOST_TAB_ID + ROOST_SOCKET (or any tab-specific env).
func NewSession(ws *core.Workspace, tab core.Tab, cols, rows uint16, extraEnv ...string) (*Session, error) {
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
	p, err := pty.SpawnShell(tab.CWD, cols, rows, extraEnv...)
	if err != nil {
		rs.Close()
		term.Close()
		return nil, err
	}

	font := pango.NewFontDescription()
	// Resolve "JetBrains Mono, Monaco, monospace" → the first installed
	// family. Avoids Pango falling back to Verdana on macOS when the
	// head of the list is missing (which produces wide cells with
	// narrow glyphs and huge gaps between letters).
	font.SetFamily(pickFontFamily(fontFamily))
	font.SetSize(fontSizePt * pango.SCALE)
	fontBold := font.Copy()
	fontBold.SetWeight(pango.WeightBold)

	s := &Session{
		ws: ws, tab: tab,
		pty: p, term: term, rs: rs,
		font:          font,
		fontBold:      fontBold,
		cols:          cols,
		rows:          rows,
		pumpDone:      make(chan struct{}),
		cursorOn:      true,
		windowFocused: true,
	}

	s.da = gtk.NewDrawingArea()
	s.da.SetHExpand(true)
	s.da.SetVExpand(true)
	s.da.SetCanFocus(true)
	s.da.SetFocusable(true)
	s.da.SetFocusOnClick(true)
	// NOTE: pango_cairo_context_set_font_options would normally pin
	// glyph metrics to the integer pixel grid (the single biggest text
	// crispness win) but the gotk4 binding for ContextSetFontOptions
	// crashes — it expects cairo.FontOptions to follow the gextras
	// "record" struct convention, which the cairo binding does not.
	// We rely on integer cellW/cellH + bold-via-FontDescription
	// instead; revisit if/when that binding is fixed upstream.
	s.measureCells()
	s.da.SetDrawFunc(func(_ *gtk.DrawingArea, cr *cairo.Context, _, _ int) {
		drawTerminal(cr, s)
	})
	s.da.ConnectResize(s.onResize)

	// Cursor blink. The toggle queues a redraw on the same DA; cheap
	// enough to leave running at all times, but we still pause it when
	// the window is unfocused (drawTerminal renders a hollow outline
	// in that case). Removed in Close.
	s.blinkSrc = glib.TimeoutAdd(blinkPeriodMs, func() bool {
		if s.closed.Load() {
			return false
		}
		if s.windowFocused {
			s.cursorOn = !s.cursorOn
			s.da.QueueDraw()
		}
		return true
	})

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

// oscScanner returns the per-session OSC scanner. Lazily allocated so
// sessions that never receive an OSC notification don't pay the cost.
// Called only from the pump goroutine.
func (s *Session) oscScanner() *osc.Scanner {
	if s.osc == nil {
		tabID := s.tab.ID
		ws := s.ws
		s.osc = osc.NewScanner(func(n osc.Notification) {
			title := n.Title
			if title == "" {
				title = "(notification)"
			}
			_ = ws.Notify(tabID, title, n.Body)
		})
	}
	return s.osc
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
	if s.blinkSrc != 0 {
		glib.SourceRemove(s.blinkSrc)
		s.blinkSrc = 0
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
			// OSC scanning happens in the pump goroutine so we don't
			// burn main-thread cycles on byte-by-byte parsing. The
			// scanner only fires the workspace event channel — no GTK.
			s.oscScanner().Feed(chunk)
			glib.IdleAdd(func() {
				if s.closed.Load() {
					return
				}
				s.term.VTWrite(chunk)
				_ = s.rs.Update(s.term)
				s.checkTitleAndPWD()
				s.da.QueueDraw()
			})
		}
		if err != nil {
			if err != io.EOF {
				slog.Warn("pty read", "tab_id", s.tab.ID, "err", err)
			}
			// Notify the App that the shell exited so it can close
			// the tab. Marshalled to the main thread because
			// onPTYExit touches widgets. Guard against double-close
			// in case Close() races us.
			if cb := s.onPTYExit; cb != nil {
				glib.IdleAdd(func() {
					if s.closed.Load() {
						return
					}
					cb()
				})
			}
			return
		}
	}
}

// measureCells uses Pango to size one monospace cell. Called once at
// construction; the renderer reads cellW/cellH on every draw. Stored
// as ints so cell origins land on integer pixel boundaries (text
// crispness).
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
	s.cellW = w
	s.cellH = h
}

// checkTitleAndPWD polls the terminal's OSC-set title and cwd after a
// vt_write. Cheap (one cgo call each) and runs only when the on-change
// callback is set. Fires the callback only when the value actually
// changes.
func (s *Session) checkTitleAndPWD() {
	if s.onTitleChanged != nil {
		t := s.term.Title()
		if t != s.lastTitle {
			s.lastTitle = t
			s.onTitleChanged(t)
		}
	}
	if s.onPWDChanged != nil {
		p := s.term.PWD()
		if p != s.lastPWD {
			s.lastPWD = p
			s.onPWDChanged(p)
		}
	}
}

// onResize reflows the terminal to fit the new pixel dimensions. Called
// by GtkDrawingArea every time it's allocated a new size.
func (s *Session) onResize(width, height int) {
	if s.closed.Load() {
		return
	}
	cols := uint16((width - 2*pad) / s.cellW)
	rows := uint16((height - 2*pad) / s.cellH)
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

// SetWindowFocused updates the cursor blink state based on whether the
// containing window has keyboard focus. Called from App in response to
// the AdwApplicationWindow's is-active notify. When unfocused the
// cursor is rendered as a hollow outline; on regain we snap back to
// the on phase so it's immediately visible.
func (s *Session) SetWindowFocused(focused bool) {
	s.windowFocused = focused
	if focused {
		s.cursorOn = true
	}
	s.da.QueueDraw()
}
