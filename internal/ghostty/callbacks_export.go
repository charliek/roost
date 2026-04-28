package ghostty

// #include <ghostty/vt.h>
import "C"

import (
	"runtime/cgo"
	"unsafe"
)

// This file holds the cgo //export functions that libghostty-vt invokes
// from C. They live in their own file so callbacks.go's preamble can
// declare them as extern symbols without colliding with cgo's auto-
// generated _cgo_export.h prototypes.
//
// All three resolve their Go-side state from libghostty's userdata slot
// (a cgo.Handle wrapping *terminalCallbacks).

// resolveCallbacks pulls the shared callback struct out of libghostty's
// userdata slot. Returns nil if userdata is unset or the handle is bad.
//
// The recover guard catches the panic cgo.Handle.Value emits if the
// handle was already deleted, which can happen if libghostty fires a
// callback during the brief window between Terminal.Close calling
// cbsHandle.Delete and the C-side teardown completing.
func resolveCallbacks(userdata unsafe.Pointer) (cbs *terminalCallbacks) {
	if userdata == nil {
		return nil
	}
	defer func() {
		if recover() != nil {
			cbs = nil
		}
	}()
	v := cgo.Handle(uintptr(userdata)).Value()
	cbs, _ = v.(*terminalCallbacks)
	return cbs
}

//export roostWritePtyFn
func roostWritePtyFn(_ C.GhosttyTerminal, userdata unsafe.Pointer, data *C.uint8_t, length C.size_t) {
	if data == nil || length == 0 {
		return
	}
	cbs := resolveCallbacks(userdata)
	if cbs == nil || cbs.writePty == nil {
		return
	}
	src := unsafe.Slice((*byte)(unsafe.Pointer(data)), int(length))
	cp := append([]byte(nil), src...)
	cbs.writePty(cp)
}

//export roostDeviceAttrsFn
func roostDeviceAttrsFn(_ C.GhosttyTerminal, userdata unsafe.Pointer, out *C.GhosttyDeviceAttributes) C.bool {
	if out == nil {
		return C.bool(false)
	}
	cbs := resolveCallbacks(userdata)
	if cbs == nil || cbs.deviceAttrs == nil {
		return C.bool(false)
	}
	d := cbs.deviceAttrs

	out.primary.conformance_level = C.uint16_t(d.PrimaryConformance)
	n := len(d.PrimaryFeatures)
	if n > len(out.primary.features) {
		n = len(out.primary.features)
	}
	for i := 0; i < n; i++ {
		out.primary.features[i] = C.uint16_t(d.PrimaryFeatures[i])
	}
	out.primary.num_features = C.size_t(n)

	out.secondary.device_type = C.uint16_t(d.SecondaryDeviceType)
	out.secondary.firmware_version = C.uint16_t(d.SecondaryFirmwareVersion)
	out.secondary.rom_cartridge = C.uint16_t(d.SecondaryRomCartridge)

	out.tertiary.unit_id = C.uint32_t(d.TertiaryUnitID)

	return C.bool(true)
}

//export roostColorSchemeFn
func roostColorSchemeFn(_ C.GhosttyTerminal, userdata unsafe.Pointer, out *C.GhosttyColorScheme) C.bool {
	if out == nil {
		return C.bool(false)
	}
	cbs := resolveCallbacks(userdata)
	if cbs == nil || !cbs.hasScheme {
		return C.bool(false)
	}
	*out = cbs.colorScheme
	return C.bool(true)
}
