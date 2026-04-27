package ghostty

// #cgo CFLAGS: -I${SRCDIR}/../../build/out/include
// #cgo LDFLAGS: ${SRCDIR}/../../build/out/lib/libghostty-vt.a
// #include <ghostty/vt.h>
// #include <stdlib.h>
//
// // Cgo can't take addresses of typed enum constants directly when the
// // setopt API expects a void*. These trampolines provide stable
// // addressable memory for option values without per-call allocations.
// static void roost_key_encoder_setopt_macos_option_as_alt(GhosttyKeyEncoder enc, GhosttyOptionAsAlt v) {
//     ghostty_key_encoder_setopt(enc, GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT, &v);
// }
import "C"

import (
	"errors"
	"fmt"
	"unsafe"
)

// KeyAction mirrors GhosttyKeyAction.
type KeyAction uint8

const (
	KeyActionRelease KeyAction = C.GHOSTTY_KEY_ACTION_RELEASE
	KeyActionPress   KeyAction = C.GHOSTTY_KEY_ACTION_PRESS
	KeyActionRepeat  KeyAction = C.GHOSTTY_KEY_ACTION_REPEAT
)

// Mods is a bitmask mirroring GhosttyMods. The side bits (right vs left
// for a given modifier) are only meaningful when the corresponding
// modifier bit is set; the encoder ignores side bits we don't supply.
type Mods uint16

const (
	ModShift    Mods = C.GHOSTTY_MODS_SHIFT
	ModCtrl     Mods = C.GHOSTTY_MODS_CTRL
	ModAlt      Mods = C.GHOSTTY_MODS_ALT
	ModSuper    Mods = C.GHOSTTY_MODS_SUPER
	ModCapsLock Mods = C.GHOSTTY_MODS_CAPS_LOCK
	ModNumLock  Mods = C.GHOSTTY_MODS_NUM_LOCK
)

// Key is a physical, layout-independent key code mirroring GhosttyKey.
// "Physical" means the "a" key on a US keyboard and the "ф" key on a
// Russian keyboard both map to KeyA — the encoder pairs the physical
// key with the platform's UTF-8 text to produce the right escape.
type Key uint16

// Subset of GhosttyKey we map from GDK. The encoder accepts the full
// enum, but every key our keymap actually targets is here. Add more as
// the keymap grows. Values come from the C header so they always match.
const (
	KeyUnidentified Key = C.GHOSTTY_KEY_UNIDENTIFIED

	KeyBackquote    Key = C.GHOSTTY_KEY_BACKQUOTE
	KeyBackslash    Key = C.GHOSTTY_KEY_BACKSLASH
	KeyBracketLeft  Key = C.GHOSTTY_KEY_BRACKET_LEFT
	KeyBracketRight Key = C.GHOSTTY_KEY_BRACKET_RIGHT
	KeyComma        Key = C.GHOSTTY_KEY_COMMA
	KeyDigit0       Key = C.GHOSTTY_KEY_DIGIT_0
	KeyDigit1       Key = C.GHOSTTY_KEY_DIGIT_1
	KeyDigit2       Key = C.GHOSTTY_KEY_DIGIT_2
	KeyDigit3       Key = C.GHOSTTY_KEY_DIGIT_3
	KeyDigit4       Key = C.GHOSTTY_KEY_DIGIT_4
	KeyDigit5       Key = C.GHOSTTY_KEY_DIGIT_5
	KeyDigit6       Key = C.GHOSTTY_KEY_DIGIT_6
	KeyDigit7       Key = C.GHOSTTY_KEY_DIGIT_7
	KeyDigit8       Key = C.GHOSTTY_KEY_DIGIT_8
	KeyDigit9       Key = C.GHOSTTY_KEY_DIGIT_9
	KeyEqual        Key = C.GHOSTTY_KEY_EQUAL

	KeyA Key = C.GHOSTTY_KEY_A
	KeyB Key = C.GHOSTTY_KEY_B
	KeyC Key = C.GHOSTTY_KEY_C
	KeyD Key = C.GHOSTTY_KEY_D
	KeyE Key = C.GHOSTTY_KEY_E
	KeyF Key = C.GHOSTTY_KEY_F
	KeyG Key = C.GHOSTTY_KEY_G
	KeyH Key = C.GHOSTTY_KEY_H
	KeyI Key = C.GHOSTTY_KEY_I
	KeyJ Key = C.GHOSTTY_KEY_J
	KeyK Key = C.GHOSTTY_KEY_K
	KeyL Key = C.GHOSTTY_KEY_L
	KeyM Key = C.GHOSTTY_KEY_M
	KeyN Key = C.GHOSTTY_KEY_N
	KeyO Key = C.GHOSTTY_KEY_O
	KeyP Key = C.GHOSTTY_KEY_P
	KeyQ Key = C.GHOSTTY_KEY_Q
	KeyR Key = C.GHOSTTY_KEY_R
	KeyS Key = C.GHOSTTY_KEY_S
	KeyT Key = C.GHOSTTY_KEY_T
	KeyU Key = C.GHOSTTY_KEY_U
	KeyV Key = C.GHOSTTY_KEY_V
	KeyW Key = C.GHOSTTY_KEY_W
	KeyX Key = C.GHOSTTY_KEY_X
	KeyY Key = C.GHOSTTY_KEY_Y
	KeyZ Key = C.GHOSTTY_KEY_Z

	KeyMinus     Key = C.GHOSTTY_KEY_MINUS
	KeyPeriod    Key = C.GHOSTTY_KEY_PERIOD
	KeyQuote     Key = C.GHOSTTY_KEY_QUOTE
	KeySemicolon Key = C.GHOSTTY_KEY_SEMICOLON
	KeySlash     Key = C.GHOSTTY_KEY_SLASH

	KeyBackspace Key = C.GHOSTTY_KEY_BACKSPACE
	KeyEnter     Key = C.GHOSTTY_KEY_ENTER
	KeySpace     Key = C.GHOSTTY_KEY_SPACE
	KeyTab       Key = C.GHOSTTY_KEY_TAB

	KeyDelete   Key = C.GHOSTTY_KEY_DELETE
	KeyEnd      Key = C.GHOSTTY_KEY_END
	KeyHome     Key = C.GHOSTTY_KEY_HOME
	KeyInsert   Key = C.GHOSTTY_KEY_INSERT
	KeyPageDown Key = C.GHOSTTY_KEY_PAGE_DOWN
	KeyPageUp   Key = C.GHOSTTY_KEY_PAGE_UP

	KeyArrowDown  Key = C.GHOSTTY_KEY_ARROW_DOWN
	KeyArrowLeft  Key = C.GHOSTTY_KEY_ARROW_LEFT
	KeyArrowRight Key = C.GHOSTTY_KEY_ARROW_RIGHT
	KeyArrowUp    Key = C.GHOSTTY_KEY_ARROW_UP

	KeyEscape Key = C.GHOSTTY_KEY_ESCAPE

	KeyF1  Key = C.GHOSTTY_KEY_F1
	KeyF2  Key = C.GHOSTTY_KEY_F2
	KeyF3  Key = C.GHOSTTY_KEY_F3
	KeyF4  Key = C.GHOSTTY_KEY_F4
	KeyF5  Key = C.GHOSTTY_KEY_F5
	KeyF6  Key = C.GHOSTTY_KEY_F6
	KeyF7  Key = C.GHOSTTY_KEY_F7
	KeyF8  Key = C.GHOSTTY_KEY_F8
	KeyF9  Key = C.GHOSTTY_KEY_F9
	KeyF10 Key = C.GHOSTTY_KEY_F10
	KeyF11 Key = C.GHOSTTY_KEY_F11
	KeyF12 Key = C.GHOSTTY_KEY_F12
)

// KeyEvent is the input to KeyEncoder.Encode. Mirrors the fields the
// encoder reads from a GhosttyKeyEvent.
//
// UTF8 must be the unmodified character before any Ctrl/Meta
// transformation (the encoder derives those from Key+Mods). Pass empty
// for keys with no platform text (function keys, arrows, modifier-only
// presses). Do not include C0 control characters or platform PUA codes.
type KeyEvent struct {
	Action             KeyAction
	Key                Key
	Mods               Mods
	ConsumedMods       Mods
	UTF8               string
	UnshiftedCodepoint rune
	Composing          bool
}

// KeyEncoder wraps a GhosttyKeyEncoder + a reusable GhosttyKeyEvent so
// per-keystroke encoding doesn't allocate.
//
// THREADING: like Terminal, the encoder is single-thread-only. Create,
// use, and Close it on the GTK main thread. setopt_from_terminal must
// be called immediately before each Encode if the foreground app may
// have changed terminal modes since the previous call (which is to
// say: always, in practice — the call is cheap).
type KeyEncoder struct {
	c     C.GhosttyKeyEncoder
	event C.GhosttyKeyEvent
}

// NewKeyEncoder creates an encoder + reusable event. Free with Close.
func NewKeyEncoder() (*KeyEncoder, error) {
	e := &KeyEncoder{}
	if rc := C.ghostty_key_encoder_new(nil, &e.c); rc != C.GHOSTTY_SUCCESS {
		return nil, fmt.Errorf("ghostty_key_encoder_new: %d", int(rc))
	}
	if rc := C.ghostty_key_event_new(nil, &e.event); rc != C.GHOSTTY_SUCCESS {
		C.ghostty_key_encoder_free(e.c)
		return nil, fmt.Errorf("ghostty_key_event_new: %d", int(rc))
	}
	return e, nil
}

// Close frees the encoder and reusable event. Safe to call twice.
//
// No runtime.SetFinalizer is installed: libghostty_key_encoder_free /
// libghostty_key_event_free must run on the GTK main thread (same
// constraint as the Terminal handle), and finalizers run on the GC
// goroutine. The Session.Close path explicitly schedules cleanup on
// the main thread via glib.IdleAdd; that is the only correct path.
func (e *KeyEncoder) Close() {
	if e.event != nil {
		C.ghostty_key_event_free(e.event)
		e.event = nil
	}
	if e.c != nil {
		C.ghostty_key_encoder_free(e.c)
		e.c = nil
	}
}

// SyncFromTerminal pulls the terminal's live cursor-key/keypad/Kitty
// flags into the encoder. This MUST be called immediately before each
// Encode if any vt_write may have happened since the last Sync — the
// foreground app could have toggled cursor-key application mode, pushed
// Kitty flags, etc. The call resets MACOS_OPTION_AS_ALT to FALSE; if
// you've configured option-as-alt, re-apply it via SetMacOSOptionAsAlt.
func (e *KeyEncoder) SyncFromTerminal(t *Terminal) {
	if e.c == nil || t == nil || t.c == nil {
		return
	}
	C.ghostty_key_encoder_setopt_from_terminal(e.c, t.c)
}

// SetMacOSOptionAsAlt configures whether macOS Option behaves as Alt
// (true) or composes Unicode like a normal modifier (false, default).
// Must be re-applied after every SyncFromTerminal — that call resets
// this option to FALSE per the libghostty-vt contract.
func (e *KeyEncoder) SetMacOSOptionAsAlt(asAlt bool) {
	if e.c == nil {
		return
	}
	v := C.GhosttyOptionAsAlt(C.GHOSTTY_OPTION_AS_ALT_FALSE)
	if asAlt {
		v = C.GhosttyOptionAsAlt(C.GHOSTTY_OPTION_AS_ALT_TRUE)
	}
	C.roost_key_encoder_setopt_macos_option_as_alt(e.c, v)
}

// Encode produces the terminal escape sequence for ev. Returns an
// empty slice for events that don't generate output (e.g. unmodified
// modifier-only presses).
func (e *KeyEncoder) Encode(ev KeyEvent) ([]byte, error) {
	if e.c == nil || e.event == nil {
		return nil, errors.New("ghostty: encoder closed")
	}

	C.ghostty_key_event_set_action(e.event, C.GhosttyKeyAction(ev.Action))
	C.ghostty_key_event_set_key(e.event, C.GhosttyKey(ev.Key))
	C.ghostty_key_event_set_mods(e.event, C.GhosttyMods(ev.Mods))
	C.ghostty_key_event_set_consumed_mods(e.event, C.GhosttyMods(ev.ConsumedMods))
	C.ghostty_key_event_set_composing(e.event, C.bool(ev.Composing))
	C.ghostty_key_event_set_unshifted_codepoint(e.event, C.uint32_t(ev.UnshiftedCodepoint))

	if len(ev.UTF8) > 0 {
		// CGo's CString allocates; CBytes also allocates. Take the
		// address of the first byte of the Go string instead — the
		// encoder doesn't take ownership and only reads it for the
		// duration of this call.
		bs := []byte(ev.UTF8)
		C.ghostty_key_event_set_utf8(e.event, (*C.char)(unsafe.Pointer(&bs[0])), C.size_t(len(bs)))
	} else {
		C.ghostty_key_event_set_utf8(e.event, nil, 0)
	}

	// Most escape sequences are short; start with a stack-friendly
	// 64-byte buffer and grow on OUT_OF_SPACE.
	buf := make([]byte, 64)
	var written C.size_t
	rc := C.ghostty_key_encoder_encode(
		e.c, e.event,
		(*C.char)(unsafe.Pointer(&buf[0])),
		C.size_t(len(buf)),
		&written,
	)
	if rc == C.GHOSTTY_OUT_OF_SPACE {
		buf = make([]byte, int(written))
		rc = C.ghostty_key_encoder_encode(
			e.c, e.event,
			(*C.char)(unsafe.Pointer(&buf[0])),
			C.size_t(len(buf)),
			&written,
		)
	}
	if rc != C.GHOSTTY_SUCCESS {
		return nil, fmt.Errorf("ghostty_key_encoder_encode: %d", int(rc))
	}
	if written == 0 {
		return nil, nil
	}
	return buf[:int(written)], nil
}
