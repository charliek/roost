package ghostty

// #include <ghostty/vt.h>
// #include <stdbool.h>
// #include <stdint.h>
//
// // Forward-declare the //export functions defined in callbacks_export.go
// // so the static helpers below can take their addresses. These match the
// // typedefs in <ghostty/vt/terminal.h> exactly.
// extern void roostWritePtyFn(GhosttyTerminal terminal, void* userdata,
//                              const uint8_t* data, size_t len);
// extern bool roostDeviceAttrsFn(GhosttyTerminal terminal, void* userdata,
//                                 GhosttyDeviceAttributes* out);
// extern bool roostColorSchemeFn(GhosttyTerminal terminal, void* userdata,
//                                 GhosttyColorScheme* out);
//
// // Static helpers wrap each ghostty_terminal_set call so we can pass the
// // Go-exported function addresses through cgo (cgo can't take the address
// // of an exported Go function from inside Go itself).
// static GhosttyResult roost_register_write_pty(GhosttyTerminal t) {
//     return ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_WRITE_PTY,
//                                 (const void*)roostWritePtyFn);
// }
// static GhosttyResult roost_register_device_attrs(GhosttyTerminal t) {
//     return ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
//                                 (const void*)roostDeviceAttrsFn);
// }
// static GhosttyResult roost_register_color_scheme(GhosttyTerminal t) {
//     return ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_COLOR_SCHEME,
//                                 (const void*)roostColorSchemeFn);
// }
// // roost_register_userdata stashes the cgo.Handle (an integer-sized
// // uintptr in Go) into libghostty's void* userdata slot. The
// // uintptr_t->void* cast happens in C so go vet's unsafeptr check
// // doesnt flag the equivalent Go-side conversion.
// static GhosttyResult roost_register_userdata(GhosttyTerminal t, uintptr_t h) {
//     return ghostty_terminal_set(t, GHOSTTY_TERMINAL_OPT_USERDATA, (void*)h);
// }
import "C"

import (
	"errors"
	"fmt"
	"runtime/cgo"
)

// DeviceAttrs is the Go-side mirror of GhosttyDeviceAttributes — the
// payload libghostty fills into a CSI c / CSI > c / CSI = c response.
//
// Conservative defaults that match xterm-256color advertise the modern
// feature set so probing programs don't fall back to vt100 mode.
type DeviceAttrs struct {
	// Primary (DA1, CSI c): conformance level + feature codes.
	PrimaryConformance uint16
	PrimaryFeatures    []uint16

	// Secondary (DA2, CSI > c): device type + firmware version + ROM.
	SecondaryDeviceType      uint16
	SecondaryFirmwareVersion uint16
	SecondaryRomCartridge    uint16

	// Tertiary (DA3, CSI = c): unit ID rendered as 8 hex digits.
	TertiaryUnitID uint32
}

// ensureCallbacks lazily allocates the shared callback state and stores
// its cgo handle in libghostty's userdata slot. Idempotent.
func (t *Terminal) ensureCallbacks() error {
	if t.c == nil {
		return errors.New("ghostty: terminal closed")
	}
	if t.cbsHandle != 0 {
		return nil
	}
	t.cbs = &terminalCallbacks{}
	t.cbsHandle = cgo.NewHandle(t.cbs)
	if rc := C.roost_register_userdata(t.c, C.uintptr_t(t.cbsHandle)); rc != C.GHOSTTY_SUCCESS {
		t.cbsHandle.Delete()
		t.cbsHandle = 0
		t.cbs = nil
		return fmt.Errorf("set USERDATA: %d", int(rc))
	}
	return nil
}

// SetPtyWriter registers fn as the callback libghostty invokes when it
// needs to send bytes back to the pty — query responses for OSC 10/11/12,
// DSR, DA1/2/3, etc. Without this, programs that probe terminal state
// (Codex queries OSC 11 for the background colour before deciding what
// bg SGR to emit) get silence and degrade.
//
// fn must not block. It runs on whatever thread libghostty's VT processing
// runs on (the GTK main thread in Roost). The byte slice is owned by fn
// (we copy from libghostty's buffer before invoking).
//
// Pass nil to clear an existing writer.
func (t *Terminal) SetPtyWriter(fn func([]byte)) error {
	if err := t.ensureCallbacks(); err != nil {
		return err
	}
	t.cbs.writePty = fn
	if fn == nil {
		C.ghostty_terminal_set(t.c, C.GHOSTTY_TERMINAL_OPT_WRITE_PTY, nil)
		return nil
	}
	if rc := C.roost_register_write_pty(t.c); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("set WRITE_PTY: %d", int(rc))
	}
	return nil
}

// SetDeviceAttributes installs the response shape libghostty fills in for
// DA1/DA2/DA3 queries. d is captured by reference; mutating its fields
// after the call is undefined. Pass nil to disable DA responses.
func (t *Terminal) SetDeviceAttributes(d *DeviceAttrs) error {
	if err := t.ensureCallbacks(); err != nil {
		return err
	}
	t.cbs.deviceAttrs = d
	if d == nil {
		C.ghostty_terminal_set(t.c, C.GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES, nil)
		return nil
	}
	if rc := C.roost_register_device_attrs(t.c); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("set DEVICE_ATTRIBUTES: %d", int(rc))
	}
	return nil
}

// SetColorSchemeDark configures the response to a CSI ?996n query. When
// dark is true, libghostty answers "dark"; otherwise "light". Roost is
// dark-only for now.
func (t *Terminal) SetColorSchemeDark(dark bool) error {
	if err := t.ensureCallbacks(); err != nil {
		return err
	}
	t.cbs.hasScheme = true
	if dark {
		t.cbs.colorScheme = C.GHOSTTY_COLOR_SCHEME_DARK
	} else {
		t.cbs.colorScheme = C.GHOSTTY_COLOR_SCHEME_LIGHT
	}
	if rc := C.roost_register_color_scheme(t.c); rc != C.GHOSTTY_SUCCESS {
		return fmt.Errorf("set COLOR_SCHEME: %d", int(rc))
	}
	return nil
}
