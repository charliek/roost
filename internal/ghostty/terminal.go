// Package ghostty wraps libghostty-vt via cgo. This is the only cgo
// package in Roost; everything else is pure Go.
//
// libghostty-vt is statically linked from build/out/. Run
// `./build/build.sh libghostty` from the repo root before `go build`.
package ghostty

// #cgo CFLAGS: -I${SRCDIR}/../../build/out/include
// #cgo LDFLAGS: ${SRCDIR}/../../build/out/lib/libghostty-vt.a
// #include <ghostty/vt.h>
// #include <stdlib.h>
//
// // ghostty_mode_new is a static inline in modes.h; cgo can't call C
// // statics directly, so wrap the bracketed-paste mode constant.
// static GhosttyMode roost_mode_bracketed_paste(void) {
//     return GHOSTTY_MODE_BRACKETED_PASTE;
// }
//
// // Build a scroll-viewport tagged-union value with a delta. The union
// // member layout is C-only (anonymous fields in Go cgo bindings are
// // awkward), so do the construction in C.
// static GhosttyTerminalScrollViewport roost_scroll_viewport_delta(intptr_t delta) {
//     GhosttyTerminalScrollViewport sv;
//     sv.tag = GHOSTTY_SCROLL_VIEWPORT_DELTA;
//     sv.value.delta = delta;
//     return sv;
// }
// static GhosttyTerminalScrollViewport roost_scroll_viewport_bottom(void) {
//     GhosttyTerminalScrollViewport sv;
//     sv.tag = GHOSTTY_SCROLL_VIEWPORT_BOTTOM;
//     return sv;
// }
import "C"

import (
	"errors"
	"fmt"
	"runtime"
	"runtime/cgo"
	"unsafe"
)

// Terminal is a libghostty-vt terminal: VT parser + screen state. One per
// tab. Not safe for concurrent use; the caller is responsible for keeping
// all calls on a single thread (typically the GTK main thread).
type Terminal struct {
	c C.GhosttyTerminal

	// cbs aggregates the Go-side state for libghostty terminal callbacks.
	// Allocated lazily by ensureCallbacks the first time any Set… method
	// fires. cbsHandle is the matching cgo.Handle stored in libghostty's
	// userdata slot so the callbacks can resolve it.
	cbs       *terminalCallbacks
	cbsHandle cgo.Handle
}

// terminalCallbacks holds Go-side state for every libghostty callback
// Roost wires up. libghostty exposes only one userdata slot per terminal,
// so all callbacks share this struct via a single cgo.Handle.
type terminalCallbacks struct {
	writePty    func([]byte)
	deviceAttrs *DeviceAttrs
	colorScheme C.GhosttyColorScheme
	hasScheme   bool
}

// Options configures a new Terminal.
type Options struct {
	Cols          uint16
	Rows          uint16
	MaxScrollback uint
}

// NewTerminal creates a libghostty-vt terminal. Cols and Rows must be > 0.
func NewTerminal(opts Options) (*Terminal, error) {
	if opts.Cols == 0 || opts.Rows == 0 {
		return nil, errors.New("ghostty: cols and rows must be > 0")
	}
	t := &Terminal{}
	cOpts := C.GhosttyTerminalOptions{
		cols:           C.uint16_t(opts.Cols),
		rows:           C.uint16_t(opts.Rows),
		max_scrollback: C.size_t(opts.MaxScrollback),
	}
	if rc := C.ghostty_terminal_new(nil, &t.c, cOpts); rc != C.GHOSTTY_SUCCESS {
		return nil, fmt.Errorf("ghostty_terminal_new failed: %d", int(rc))
	}
	runtime.SetFinalizer(t, func(t *Terminal) { t.Close() })
	return t, nil
}

// Close frees the terminal. Safe to call multiple times.
func (t *Terminal) Close() {
	if t.c != nil {
		C.ghostty_terminal_free(t.c)
		t.c = nil
		runtime.SetFinalizer(t, nil)
	}
	if t.cbsHandle != 0 {
		t.cbsHandle.Delete()
		t.cbsHandle = 0
		t.cbs = nil
	}
}

// VTWrite feeds bytes into the VT parser. Updates terminal state (cursor,
// styles, screen contents). Must be called from the same thread that
// created the terminal.
func (t *Terminal) VTWrite(data []byte) {
	if len(data) == 0 || t.c == nil {
		return
	}
	C.ghostty_terminal_vt_write(t.c, (*C.uint8_t)(unsafe.Pointer(&data[0])), C.size_t(len(data)))
}

// Resize changes the terminal grid dimensions. cellW/cellH are pixel sizes
// (used for Kitty graphics); pass 0 if you don't know yet.
func (t *Terminal) Resize(cols, rows uint16, cellW, cellH uint32) error {
	if t.c == nil {
		return errors.New("ghostty: terminal closed")
	}
	if rc := C.ghostty_terminal_resize(t.c, C.uint16_t(cols), C.uint16_t(rows), C.uint32_t(cellW), C.uint32_t(cellH)); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("ghostty_terminal_resize failed: %d", int(rc))
	}
	return nil
}

// SetTheme installs default foreground, background, cursor color, and
// 256-color palette on the terminal. Programs running in the terminal can
// still override any of these at runtime via OSC 4 / 10 / 11 / 12. Call
// once after NewTerminal, before any VT bytes are written, so the first
// frame paints with the right colors.
func (t *Terminal) SetTheme(fg, bg, cursor ColorRGB, palette *[256]ColorRGB) error {
	if t.c == nil {
		return errors.New("ghostty: terminal closed")
	}
	cFG := C.GhosttyColorRgb{r: C.uint8_t(fg.R), g: C.uint8_t(fg.G), b: C.uint8_t(fg.B)}
	if rc := C.ghostty_terminal_set(t.c, C.GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, unsafe.Pointer(&cFG)); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("set COLOR_FOREGROUND: %d", int(rc))
	}
	cBG := C.GhosttyColorRgb{r: C.uint8_t(bg.R), g: C.uint8_t(bg.G), b: C.uint8_t(bg.B)}
	if rc := C.ghostty_terminal_set(t.c, C.GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, unsafe.Pointer(&cBG)); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("set COLOR_BACKGROUND: %d", int(rc))
	}
	cCursor := C.GhosttyColorRgb{r: C.uint8_t(cursor.R), g: C.uint8_t(cursor.G), b: C.uint8_t(cursor.B)}
	if rc := C.ghostty_terminal_set(t.c, C.GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, unsafe.Pointer(&cCursor)); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("set COLOR_CURSOR: %d", int(rc))
	}
	if palette != nil {
		var cPalette [256]C.GhosttyColorRgb
		for i, c := range palette {
			cPalette[i] = C.GhosttyColorRgb{r: C.uint8_t(c.R), g: C.uint8_t(c.G), b: C.uint8_t(c.B)}
		}
		if rc := C.ghostty_terminal_set(t.c, C.GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, unsafe.Pointer(&cPalette[0])); rc != C.GHOSTTY_SUCCESS {
			return fmt.Errorf("set COLOR_PALETTE: %d", int(rc))
		}
	}
	return nil
}

// Title returns the terminal's current title (set via OSC 0/1/2). Empty
// if no title has been set. The returned string is a Go-owned copy; the
// underlying libghostty buffer can be reused on the next vt_write.
func (t *Terminal) Title() string {
	var s C.GhosttyString
	if rc := C.ghostty_terminal_get(t.c, C.GHOSTTY_TERMINAL_DATA_TITLE, unsafe.Pointer(&s)); rc != C.GHOSTTY_SUCCESS {
		return ""
	}
	if s.ptr == nil || s.len == 0 {
		return ""
	}
	return C.GoStringN((*C.char)(unsafe.Pointer(s.ptr)), C.int(s.len))
}

// PWD returns the terminal's working directory (set via OSC 7). Empty
// if no pwd has been reported.
func (t *Terminal) PWD() string {
	var s C.GhosttyString
	if rc := C.ghostty_terminal_get(t.c, C.GHOSTTY_TERMINAL_DATA_PWD, unsafe.Pointer(&s)); rc != C.GHOSTTY_SUCCESS {
		return ""
	}
	if s.ptr == nil || s.len == 0 {
		return ""
	}
	return C.GoStringN((*C.char)(unsafe.Pointer(s.ptr)), C.int(s.len))
}

// ScrollViewportDelta scrolls the viewport by `rows`. Negative scrolls
// up (into scrollback), positive scrolls down toward the active area.
// Must be called from the same thread as the terminal.
func (t *Terminal) ScrollViewportDelta(rows int) {
	if t.c == nil || rows == 0 {
		return
	}
	sv := C.roost_scroll_viewport_delta(C.intptr_t(rows))
	C.ghostty_terminal_scroll_viewport(t.c, sv)
}

// ScrollViewportToBottom snaps the viewport to the active area (most
// recent rows). Cheap to call when already at the bottom.
func (t *Terminal) ScrollViewportToBottom() {
	if t.c == nil {
		return
	}
	C.ghostty_terminal_scroll_viewport(t.c, C.roost_scroll_viewport_bottom())
}

// BracketedPasteEnabled reports whether the foreground app has enabled
// DEC private mode 2004 (bracketed paste). When true, pastes should be
// wrapped in \x1b[200~ … \x1b[201~ — use EncodePaste in paste.go for
// the wrapping and unsafe-byte stripping.
func (t *Terminal) BracketedPasteEnabled() bool {
	if t.c == nil {
		return false
	}
	var v C.bool
	if rc := C.ghostty_terminal_mode_get(t.c, C.roost_mode_bracketed_paste(), &v); rc != C.GHOSTTY_SUCCESS {
		return false
	}
	return bool(v)
}

// KittyKeyboardFlags returns the live Kitty keyboard protocol flags
// stack top, or 0 if no app has pushed flags. Used to gate Kitty CSI-u
// sequences (e.g. Shift+Enter as \x1b[13;2u) on apps that opted in.
func (t *Terminal) KittyKeyboardFlags() uint8 {
	if t.c == nil {
		return 0
	}
	var v C.uint8_t
	if rc := C.ghostty_terminal_get(t.c, C.GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS, unsafe.Pointer(&v)); rc != C.GHOSTTY_SUCCESS {
		return 0
	}
	return uint8(v)
}

// MouseTrackingActive reports whether any mouse tracking mode (X10,
// normal, button, any-event) is currently enabled. Use to branch
// between encoding mouse events to the PTY versus driving local
// selection / scroll.
func (t *Terminal) MouseTrackingActive() bool {
	if t.c == nil {
		return false
	}
	var v C.bool
	if rc := C.ghostty_terminal_get(t.c, C.GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, unsafe.Pointer(&v)); rc != C.GHOSTTY_SUCCESS {
		return false
	}
	return bool(v)
}

// AltScreenActive reports whether the alternate screen is currently
// active. Apps like vim, less, htop, jed switch into the alt screen
// for full-screen UIs. The alt screen has no scrollback, so a wheel
// scroll there should be translated to arrow keys (the "alt-scroll"
// convention every modern terminal implements) rather than wasted on
// a no-op viewport scroll.
func (t *Terminal) AltScreenActive() bool {
	if t.c == nil {
		return false
	}
	var screen C.GhosttyTerminalScreen
	if rc := C.ghostty_terminal_get(t.c, C.GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, unsafe.Pointer(&screen)); rc != C.GHOSTTY_SUCCESS {
		return false
	}
	return screen == C.GHOSTTY_TERMINAL_SCREEN_ALTERNATE
}
