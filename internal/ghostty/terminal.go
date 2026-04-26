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
import "C"

import (
	"errors"
	"fmt"
	"runtime"
	"unsafe"
)

// Terminal is a libghostty-vt terminal: VT parser + screen state. One per
// tab. Not safe for concurrent use; the caller is responsible for keeping
// all calls on a single thread (typically the GTK main thread).
type Terminal struct {
	c C.GhosttyTerminal
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
	if rc := C.ghostty_terminal_resize(t.c, C.uint16_t(cols), C.uint16_t(rows), C.uint32_t(cellW), C.uint32_t(cellH)); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("ghostty_terminal_resize failed: %d", int(rc))
	}
	return nil
}
