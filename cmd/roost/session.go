package main

import (
	"io"
	"log/slog"
	"sync"
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

	pty   *pty.PTY
	term  *ghostty.Terminal
	rs    *ghostty.RenderState
	keys  *ghostty.KeyEncoder
	mouse *ghostty.MouseEncoder
	da    *gtk.DrawingArea

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

	// writeMu serializes background PTY writes spawned by QueueWrite
	// so that bytes from concurrent paste / keystroke / mouse-event
	// goroutines don't interleave on the wire.
	writeMu sync.Mutex

	// scrollAccum aggregates fractional smooth-scroll deltas
	// (gdk.ScrollUnitSurface, common on macOS trackpads) so we only
	// dispatch whole-row scrolls to libghostty. Reset whenever it
	// crosses ±1.0 by subtracting the dispatched integer.
	scrollAccum float64

	// scrolledBack is true when the user has scrolled the viewport
	// above the active area. Cleared on snap-to-bottom, on returning
	// to bottom via wheel, and used by handleKey to snap back before
	// delivering an input-producing keystroke.
	scrolledBack bool

	// sel is the local mouse-drag selection (viewport coordinates).
	// Cleared on resize and on PTY output that touches selected rows.
	sel selection

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
	if err := term.SetTheme(
		DefaultTheme.Foreground,
		DefaultTheme.Background,
		DefaultTheme.Cursor,
		&DefaultTheme.Palette,
	); err != nil {
		term.Close()
		return nil, err
	}
	rs, err := ghostty.NewRenderState()
	if err != nil {
		term.Close()
		return nil, err
	}
	keys, err := ghostty.NewKeyEncoder()
	if err != nil {
		rs.Close()
		term.Close()
		return nil, err
	}
	mouse, err := ghostty.NewMouseEncoder()
	if err != nil {
		keys.Close()
		rs.Close()
		term.Close()
		return nil, err
	}
	p, err := pty.SpawnShell(tab.CWD, cols, rows, extraEnv...)
	if err != nil {
		mouse.Close()
		keys.Close()
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
		pty: p, term: term, rs: rs, keys: keys, mouse: mouse,
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
	// Capture phase so we see Tab / Shift+Tab before GTK's default
	// focus chain consumes them for widget traversal. Without this,
	// the toplevel may eat Shift+Tab and our handleKey never fires.
	keyCtrl.SetPropagationPhase(gtk.PhaseCapture)
	keyCtrl.ConnectKeyPressed(func(keyval, _ uint, state gdk.ModifierType) (ok bool) {
		return handleKey(s, keyval, uint(state))
	})
	s.da.AddController(keyCtrl)

	// Mouse button + drag handling. Two controllers cooperate:
	//   - GestureClick: focus grab on press, plus encoded press/release
	//     when the foreground app has mouse tracking on (vim, htop,
	//     tmux). When tracking is off, click starts a local selection.
	//   - GestureDrag: extends the selection on drag (or sends encoded
	//     motion events when tracking is on).
	// Shift always bypasses tracking → local selection / scroll, the
	// xterm convention every terminal emulator implements.
	clickCtrl := gtk.NewGestureClick()
	clickCtrl.SetButton(0) // listen for any button, not just primary
	clickCtrl.ConnectPressed(func(_ int, x, y float64) {
		s.da.GrabFocus()
		state := clickCtrl.CurrentEventState()
		button := clickCtrl.CurrentButton()
		if s.useMouseTracking(state) {
			s.sendMouseEvent(ghostty.MouseActionPress, gtkButtonToGhostty(button), state, x, y)
		} else if button == 1 {
			col, row := s.cellAt(x, y)
			s.sel.start(col, row)
			s.da.QueueDraw()
		}
	})
	clickCtrl.ConnectReleased(func(_ int, x, y float64) {
		state := clickCtrl.CurrentEventState()
		button := clickCtrl.CurrentButton()
		if s.useMouseTracking(state) {
			s.sendMouseEvent(ghostty.MouseActionRelease, gtkButtonToGhostty(button), state, x, y)
		}
	})
	s.da.AddController(clickCtrl)

	dragCtrl := gtk.NewGestureDrag()
	dragCtrl.SetButton(1) // selection drags only on primary button
	var pressX, pressY float64
	dragCtrl.ConnectDragBegin(func(x, y float64) {
		pressX, pressY = x, y
	})
	dragCtrl.ConnectDragUpdate(func(dx, dy float64) {
		state := dragCtrl.CurrentEventState()
		x, y := pressX+dx, pressY+dy
		if s.useMouseTracking(state) {
			s.sendMouseEvent(ghostty.MouseActionMotion, ghostty.MouseButtonLeft, state, x, y)
		} else {
			col, row := s.cellAt(x, y)
			s.sel.update(col, row)
			s.da.QueueDraw()
		}
	})
	s.da.AddController(dragCtrl)

	// Scroll wheel → scrollback. Three rows per discrete wheel notch;
	// for macOS-style smooth-scroll (gdk.ScrollUnitSurface) we
	// accumulate fractional deltas and dispatch whole rows when
	// |accum| ≥ 1.0. Returning true tells GTK we consumed the event
	// so it doesn't bubble to a parent ScrolledWindow.
	scrollCtrl := gtk.NewEventControllerScroll(gtk.EventControllerScrollVertical)
	scrollCtrl.ConnectScroll(func(_, dy float64) (ok bool) {
		s.handleScroll(scrollCtrl, dy)
		return true
	})
	s.da.AddController(scrollCtrl)

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
	keys := s.keys
	mouse := s.mouse
	glib.IdleAdd(func() {
		mouse.Close()
		keys.Close()
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
				// Clear any active selection on output. Most
				// terminals clear-on-any-write rather than tracking
				// which rows changed; that matches user
				// expectations and avoids the bookkeeping.
				if s.sel.active {
					s.sel.clear()
				}
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
	// Selection cells become invalid after a reflow (the underlying
	// content reorganizes around the new column count); clearing is
	// simpler and matches user expectations.
	if s.sel.active {
		s.sel.clear()
	}
	if err := s.term.Resize(cols, rows, uint32(s.cellW), uint32(s.cellH)); err != nil {
		slog.Warn("ghostty resize", "tab_id", s.tab.ID, "err", err)
	}
	if err := s.pty.Resize(cols, rows, uint16(s.cellW), uint16(s.cellH)); err != nil {
		slog.Warn("pty resize", "tab_id", s.tab.ID, "err", err)
	}
	// Update mouse encoder geometry so its pixel→cell conversion
	// matches the new layout.
	s.mouse.SetGeometry(
		uint32(width), uint32(height),
		uint32(s.cellW), uint32(s.cellH),
		uint32(pad), uint32(pad),
	)
	_ = s.rs.Update(s.term)
	s.da.QueueDraw()
}

// handleScroll converts a GTK scroll event into one of three actions,
// in order of priority:
//
//  1. **Mouse-tracking pass-through** (htop, tmux, vim with mouse=a):
//     encode wheel as button-4/5 press+release pairs to the PTY.
//  2. **Alt-screen "alt-scroll"** (vim, less, jed without mouse=a,
//     anything on the alternate screen): translate wheel into
//     ArrowUp/ArrowDown keystrokes via the key encoder. This is the
//     convention every modern terminal implements; the alt screen
//     has no scrollback, so a literal viewport scroll there is a
//     no-op and users would (correctly) see it as a bug.
//  3. **Local viewport scroll** (primary screen, no app tracking):
//     scroll roost's own scrollback buffer.
//
// Shift held bypasses mouse-tracking pass-through (xterm convention)
// but does NOT bypass alt-scroll — the alt screen still has no
// scrollback to scroll into.
//
// Quantization: discrete wheel notches contribute 3 rows per tick;
// smooth-scroll (Surface unit, common on macOS trackpads) is in
// surface pixels, scaled to rows by cellH and accumulated. Both
// branches use the same row count so trackpads "just work" wherever
// the wheel does.
//
// scrolledBack is set whenever a local scroll moves the viewport
// above the active area, and cleared as soon as the user scrolls
// back to (or past) the bottom. handleKey reads it to snap the
// viewport down before delivering an input-producing keystroke.
func (s *Session) handleScroll(ctrl *gtk.EventControllerScroll, dy float64) {
	if s.closed.Load() || dy == 0 {
		return
	}
	rows := s.quantizeScroll(ctrl.Unit(), dy)
	if rows == 0 {
		return
	}

	state := ctrl.CurrentEventState()

	// 1. Mouse-tracking pass-through.
	if s.useMouseTracking(state) {
		btn := ghostty.MouseButtonWheelUp
		count := -rows
		if rows > 0 {
			btn = ghostty.MouseButtonWheelDown
			count = rows
		}
		for i := 0; i < count; i++ {
			s.sendMouseEvent(ghostty.MouseActionPress, btn, state, 0, 0)
			s.sendMouseEvent(ghostty.MouseActionRelease, btn, state, 0, 0)
		}
		return
	}

	// 2. Alt-screen alt-scroll. Translate wheel into arrow keys so
	// vim/less/jed/etc. respond to trackpad scroll the way users
	// expect. One keystroke per row of motion.
	if s.term.AltScreenActive() {
		key := ghostty.KeyArrowUp
		count := -rows
		if rows > 0 {
			key = ghostty.KeyArrowDown
			count = rows
		}
		s.keys.SyncFromTerminal(s.term)
		for i := 0; i < count; i++ {
			out, err := s.keys.Encode(ghostty.KeyEvent{
				Action: ghostty.KeyActionPress,
				Key:    key,
			})
			if err != nil || len(out) == 0 {
				continue
			}
			s.QueueWrite(out)
		}
		return
	}

	// 3. Local viewport scroll (primary screen).
	s.term.ScrollViewportDelta(rows)
	if rows < 0 {
		s.scrolledBack = true
	} else if s.scrolledBack {
		if _, _, visible := s.rs.CursorPos(); visible {
			s.scrolledBack = false
			s.scrollAccum = 0
		}
	}
	_ = s.rs.Update(s.term)
	s.da.QueueDraw()
}

// quantizeScroll converts a raw dy delta into integer rows using the
// per-session accumulator. Discrete wheel notches dispatch immediately
// (3 rows per tick). Smooth-scroll deltas (gdk.ScrollUnitSurface) are
// in *surface pixels*; we divide by cell height to get rows then
// scale by surfaceScrollSensitivity, accumulating fractional
// remainders across events. A single fast trackpad event can dispatch
// many rows; a slow drag accumulates over events.
//
// surfaceScrollSensitivity is a hand-tuned multiplier: 1.0 = one row
// per cellH pixels of motion (sluggish on macOS trackpads); 2.0 =
// two rows per cellH (the default — feels right on a Magic Trackpad
// and on a notched mouse with high-DPI smooth scroll).
const surfaceScrollSensitivity = 2.0

func (s *Session) quantizeScroll(unit gdk.ScrollUnit, dy float64) int {
	const rowsPerWheelNotch = 3
	switch unit {
	case gdk.ScrollUnitWheel:
		if dy > 0 {
			return rowsPerWheelNotch
		}
		return -rowsPerWheelNotch
	case gdk.ScrollUnitSurface:
		if s.cellH <= 0 {
			return 0
		}
		s.scrollAccum += dy * surfaceScrollSensitivity / float64(s.cellH)
		if s.scrollAccum >= 1 || s.scrollAccum <= -1 {
			rows := int(s.scrollAccum)
			s.scrollAccum -= float64(rows)
			return rows
		}
		return 0
	default:
		return 0
	}
}

// QueueWrite serializes a write to the PTY off the GTK main thread.
// Spawns a goroutine that takes writeMu and performs pty.Write with
// a short-write loop. Order is preserved across calls because every
// goroutine must acquire writeMu before writing — a paste followed
// by a keystroke arrives at the shell in that order even though both
// run concurrently.
//
// Per CLAUDE.md, PTY read/write must run in per-tab goroutines so a
// slow consumer can't stall the GTK main thread. The data slice is
// copied so the caller can recycle its buffer immediately. Safe to
// call from the main thread; safe to call after Close (no-op).
func (s *Session) QueueWrite(data []byte) {
	if s.closed.Load() || len(data) == 0 {
		return
	}
	buf := append([]byte(nil), data...)
	tabID := s.tab.ID
	go func() {
		s.writeMu.Lock()
		defer s.writeMu.Unlock()
		if s.closed.Load() {
			return
		}
		for off := 0; off < len(buf); {
			n, err := s.pty.Write(buf[off:])
			if err != nil {
				slog.Warn("pty write", "tab_id", tabID, "err", err,
					"wrote", off+n, "total", len(buf))
				return
			}
			off += n
		}
	}()
}

// useMouseTracking decides whether mouse events should be encoded and
// forwarded to the PTY (true) or drive local selection/scroll (false).
//
// Pass-through is suppressed when:
//   - Shift is held — xterm/iTerm2 convention; lets users select text
//     in apps that grab the mouse (vim, tmux). Non-optional muscle
//     memory for the target user base.
//   - The user has scrolled into history. Mouse interactions during
//     scrollback are about reading, not interacting with the app.
func (s *Session) useMouseTracking(state gdk.ModifierType) bool {
	if s.scrolledBack {
		return false
	}
	if state&gdk.ShiftMask != 0 {
		return false
	}
	return s.term.MouseTrackingActive()
}

// sendMouseEvent encodes one mouse event via libghostty-vt and writes
// the result to the PTY. SyncFromTerminal is called per-event so live
// terminal-mode changes (X10/normal/button/any-event, SGR/X10 format)
// are honored. The encoder picks the right escape sequence based on
// those modes.
func (s *Session) sendMouseEvent(action ghostty.MouseAction, button ghostty.MouseButton, state gdk.ModifierType, x, y float64) {
	if s.closed.Load() || s.mouse == nil {
		return
	}
	s.mouse.SyncFromTerminal(s.term)
	s.mouse.SetAnyButtonPressed(action != ghostty.MouseActionRelease)
	out, err := s.mouse.Encode(ghostty.MouseEvent{
		Action: action,
		Button: button,
		Mods:   gdkModsToGhosttyMods(state),
		X:      float32(x),
		Y:      float32(y),
	})
	if err != nil || len(out) == 0 {
		return
	}
	s.QueueWrite(out)
}

// gtkButtonToGhostty maps GTK's 1=left/2=middle/3=right convention to
// libghostty-vt's MouseButton enum.
func gtkButtonToGhostty(btn uint) ghostty.MouseButton {
	switch btn {
	case 1:
		return ghostty.MouseButtonLeft
	case 2:
		return ghostty.MouseButtonMiddle
	case 3:
		return ghostty.MouseButtonRight
	default:
		return ghostty.MouseButtonNone
	}
}

// cellAt converts pixel coordinates within the DrawingArea into
// viewport cell (col, row), clamped to [0, cols) and [0, rows).
func (s *Session) cellAt(x, y float64) (col, row int) {
	if s.cellW <= 0 || s.cellH <= 0 {
		return 0, 0
	}
	col = int((x - float64(pad)) / float64(s.cellW))
	row = int((y - float64(pad)) / float64(s.cellH))
	if col < 0 {
		col = 0
	}
	if row < 0 {
		row = 0
	}
	if col >= int(s.cols) {
		col = int(s.cols) - 1
	}
	if row >= int(s.rows) {
		row = int(s.rows) - 1
	}
	return
}

// snapToBottom returns the viewport to the active area if the user
// has scrolled back. Called from handleKey before dispatching an
// input-producing keystroke, mirroring the behavior of every other
// terminal multiplexer.
func (s *Session) snapToBottom() {
	if !s.scrolledBack {
		return
	}
	s.term.ScrollViewportToBottom()
	s.scrolledBack = false
	s.scrollAccum = 0
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
