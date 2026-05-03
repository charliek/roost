package ghostty

// #include <ghostty/vt.h>
import "C"

import "unsafe"

// HyperlinkAt returns the OSC 8 hyperlink URI for the cell at (col, row)
// in viewport coordinates, if any. Returns ("", false) if the cell has no
// hyperlink, the coordinates are out of range, or the terminal is closed.
//
// libghostty's grid_ref API documents that refs are invalidated by any
// mutating terminal operation, so we read the URI immediately and copy
// it into a Go string before returning.
func (t *Terminal) HyperlinkAt(col, row int) (string, bool) {
	if t.c == nil || col < 0 || row < 0 {
		return "", false
	}

	var pt C.GhosttyPoint
	pt.tag = C.GHOSTTY_POINT_TAG_VIEWPORT
	coord := (*C.GhosttyPointCoordinate)(unsafe.Pointer(&pt.value))
	coord.x = C.uint16_t(col)
	coord.y = C.uint32_t(row)

	var ref C.GhosttyGridRef
	ref.size = C.size_t(unsafe.Sizeof(ref))
	if rc := C.ghostty_terminal_grid_ref(t.c, pt, &ref); rc != C.GHOSTTY_SUCCESS {
		return "", false
	}

	var buf [1024]C.uint8_t
	var outLen C.size_t
	rc := C.ghostty_grid_ref_hyperlink_uri(&ref, &buf[0], C.size_t(len(buf)), &outLen)
	switch rc {
	case C.GHOSTTY_SUCCESS:
		if outLen == 0 {
			return "", false
		}
		return C.GoStringN((*C.char)(unsafe.Pointer(&buf[0])), C.int(outLen)), true
	case C.GHOSTTY_OUT_OF_SPACE:
		big := make([]C.uint8_t, outLen)
		if rc2 := C.ghostty_grid_ref_hyperlink_uri(&ref, &big[0], outLen, &outLen); rc2 != C.GHOSTTY_SUCCESS {
			return "", false
		}
		return C.GoStringN((*C.char)(unsafe.Pointer(&big[0])), C.int(outLen)), true
	default:
		return "", false
	}
}
