package ghostty

// #cgo CFLAGS: -I${SRCDIR}/../../build/out/include
// #cgo LDFLAGS: ${SRCDIR}/../../build/out/lib/libghostty-vt.a
// #include <ghostty/vt.h>
// #include <stdlib.h>
// #include <string.h>
//
// // Sized-struct initializer for the encoder size context. The size
// // field must be set to sizeof(GhosttyMouseEncoderSize) per the
// // libghostty-vt sized-struct convention.
// static void roost_init_mouse_size(GhosttyMouseEncoderSize* s) {
//     memset(s, 0, sizeof(*s));
//     s->size = sizeof(*s);
// }
//
// // Trampoline for setopt void* — same pattern as the key encoder.
// static void roost_mouse_encoder_set_size(GhosttyMouseEncoder enc, GhosttyMouseEncoderSize* s) {
//     ghostty_mouse_encoder_setopt(enc, GHOSTTY_MOUSE_ENCODER_OPT_SIZE, s);
// }
// static void roost_mouse_encoder_set_any_button_pressed(GhosttyMouseEncoder enc, bool v) {
//     ghostty_mouse_encoder_setopt(enc, GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED, &v);
// }
import "C"

import (
	"errors"
	"fmt"
	"runtime"
	"unsafe"
)

// MouseAction mirrors GhosttyMouseAction.
type MouseAction uint8

const (
	MouseActionPress   MouseAction = C.GHOSTTY_MOUSE_ACTION_PRESS
	MouseActionRelease MouseAction = C.GHOSTTY_MOUSE_ACTION_RELEASE
	MouseActionMotion  MouseAction = C.GHOSTTY_MOUSE_ACTION_MOTION
)

// MouseButton mirrors GhosttyMouseButton. Wheel-up and wheel-down are
// encoded as button 4 and button 5 (xterm convention).
type MouseButton uint8

const (
	MouseButtonNone      MouseButton = C.GHOSTTY_MOUSE_BUTTON_UNKNOWN
	MouseButtonLeft      MouseButton = C.GHOSTTY_MOUSE_BUTTON_LEFT
	MouseButtonRight     MouseButton = C.GHOSTTY_MOUSE_BUTTON_RIGHT
	MouseButtonMiddle    MouseButton = C.GHOSTTY_MOUSE_BUTTON_MIDDLE
	MouseButtonWheelUp   MouseButton = C.GHOSTTY_MOUSE_BUTTON_FOUR
	MouseButtonWheelDown MouseButton = C.GHOSTTY_MOUSE_BUTTON_FIVE
)

// MouseEvent is the input to MouseEncoder.Encode. Position is in
// surface-space pixels (relative to the DrawingArea origin).
type MouseEvent struct {
	Action MouseAction
	Button MouseButton // MouseButtonNone for motion events with no button held
	Mods   Mods
	X, Y   float32
}

// MouseEncoder wraps a GhosttyMouseEncoder + a reusable
// GhosttyMouseEvent. Same threading rules as KeyEncoder: main-thread
// only. SetGeometry must be called on resize so position-to-cell
// translation is correct; SyncFromTerminal must be called immediately
// before each Encode (terminal modes can change with PTY output).
type MouseEncoder struct {
	c     C.GhosttyMouseEncoder
	event C.GhosttyMouseEvent
	size  C.GhosttyMouseEncoderSize
}

// NewMouseEncoder creates an encoder + reusable event. Free with Close.
func NewMouseEncoder() (*MouseEncoder, error) {
	e := &MouseEncoder{}
	if rc := C.ghostty_mouse_encoder_new(nil, &e.c); rc != C.GHOSTTY_SUCCESS {
		return nil, fmt.Errorf("ghostty_mouse_encoder_new: %d", int(rc))
	}
	if rc := C.ghostty_mouse_event_new(nil, &e.event); rc != C.GHOSTTY_SUCCESS {
		C.ghostty_mouse_encoder_free(e.c)
		return nil, fmt.Errorf("ghostty_mouse_event_new: %d", int(rc))
	}
	C.roost_init_mouse_size(&e.size)
	runtime.SetFinalizer(e, func(e *MouseEncoder) { e.Close() })
	return e, nil
}

// Close frees the encoder + reusable event. Safe to call twice.
func (e *MouseEncoder) Close() {
	if e.event != nil {
		C.ghostty_mouse_event_free(e.event)
		e.event = nil
	}
	if e.c != nil {
		C.ghostty_mouse_encoder_free(e.c)
		e.c = nil
	}
	runtime.SetFinalizer(e, nil)
}

// SetGeometry tells the encoder the current surface dimensions and
// cell size so it can convert pixel positions to cell coordinates.
// Call on resize. cellW and cellH must be non-zero.
func (e *MouseEncoder) SetGeometry(screenW, screenH, cellW, cellH, padTop, padLeft uint32) {
	if e.c == nil || cellW == 0 || cellH == 0 {
		return
	}
	C.roost_init_mouse_size(&e.size)
	e.size.screen_width = C.uint32_t(screenW)
	e.size.screen_height = C.uint32_t(screenH)
	e.size.cell_width = C.uint32_t(cellW)
	e.size.cell_height = C.uint32_t(cellH)
	e.size.padding_top = C.uint32_t(padTop)
	e.size.padding_left = C.uint32_t(padLeft)
	C.roost_mouse_encoder_set_size(e.c, &e.size)
}

// SyncFromTerminal pulls the terminal's live tracking-mode and output
// format into the encoder. Must be called before each Encode (cheap).
func (e *MouseEncoder) SyncFromTerminal(t *Terminal) {
	if e.c == nil || t == nil || t.c == nil {
		return
	}
	C.ghostty_mouse_encoder_setopt_from_terminal(e.c, t.c)
}

// SetAnyButtonPressed tells the encoder whether the user is currently
// dragging (any mouse button held). Used by button-event tracking
// mode 1002 to decide whether motion events get reported.
func (e *MouseEncoder) SetAnyButtonPressed(pressed bool) {
	if e.c == nil {
		return
	}
	C.roost_mouse_encoder_set_any_button_pressed(e.c, C.bool(pressed))
}

// Encode produces the terminal escape sequence for ev. Returns an
// empty slice when the event doesn't generate output (e.g. motion
// outside the active tracking mode).
func (e *MouseEncoder) Encode(ev MouseEvent) ([]byte, error) {
	if e.c == nil || e.event == nil {
		return nil, errors.New("ghostty: mouse encoder closed")
	}

	C.ghostty_mouse_event_set_action(e.event, C.GhosttyMouseAction(ev.Action))
	if ev.Button == MouseButtonNone {
		C.ghostty_mouse_event_clear_button(e.event)
	} else {
		C.ghostty_mouse_event_set_button(e.event, C.GhosttyMouseButton(ev.Button))
	}
	C.ghostty_mouse_event_set_mods(e.event, C.GhosttyMods(ev.Mods))
	pos := C.GhosttyMousePosition{x: C.float(ev.X), y: C.float(ev.Y)}
	C.ghostty_mouse_event_set_position(e.event, pos)

	buf := make([]byte, 64)
	var written C.size_t
	rc := C.ghostty_mouse_encoder_encode(
		e.c, e.event,
		(*C.char)(unsafe.Pointer(&buf[0])),
		C.size_t(len(buf)),
		&written,
	)
	if rc == C.GHOSTTY_OUT_OF_SPACE {
		buf = make([]byte, int(written))
		rc = C.ghostty_mouse_encoder_encode(
			e.c, e.event,
			(*C.char)(unsafe.Pointer(&buf[0])),
			C.size_t(len(buf)),
			&written,
		)
	}
	if rc != C.GHOSTTY_SUCCESS {
		return nil, fmt.Errorf("ghostty_mouse_encoder_encode: %d", int(rc))
	}
	if written == 0 {
		return nil, nil
	}
	return buf[:int(written)], nil
}
