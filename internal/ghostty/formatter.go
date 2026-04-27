package ghostty

// #cgo CFLAGS: -I${SRCDIR}/../../build/out/include
// #cgo LDFLAGS: ${SRCDIR}/../../build/out/lib/libghostty-vt.a
// #include <ghostty/vt.h>
// #include <stdlib.h>
// #include <string.h>
//
// // Build a viewport-tagged GhosttyPoint at (col, row).
// static GhosttyPoint roost_viewport_point(uint16_t col, uint32_t row) {
//     GhosttyPoint p;
//     p.tag = GHOSTTY_POINT_TAG_VIEWPORT;
//     p.value.coordinate.x = col;
//     p.value.coordinate.y = row;
//     return p;
// }
//
// // Initialize a GhosttySelection sized struct.
// static void roost_init_selection(GhosttySelection* sel) {
//     memset(sel, 0, sizeof(*sel));
//     sel->size = sizeof(*sel);
// }
//
// // Initialize a GhosttyFormatterTerminalOptions sized struct.
// static void roost_init_formatter_opts(GhosttyFormatterTerminalOptions* o) {
//     memset(o, 0, sizeof(*o));
//     o->size = sizeof(*o);
//     o->extra.size = sizeof(o->extra);
//     o->extra.screen.size = sizeof(o->extra.screen);
// }
import "C"

import (
	"errors"
	"fmt"
	"unsafe"
)

// CopyViewportSelection extracts plain text from the terminal's
// viewport between (startCol, startRow) and (endCol, endRow), inclusive
// at both endpoints. Coordinates are viewport-relative cell positions
// (top-left = 0,0). The formatter handles soft-wrap unwrapping and
// trailing-whitespace trimming.
//
// Returns the empty string for a zero-area selection. Selection must
// be non-null in the caller's coordinate sense; ordering of start/end
// is normalized internally.
func CopyViewportSelection(t *Terminal, startCol uint16, startRow uint32, endCol uint16, endRow uint32) (string, error) {
	if t == nil || t.c == nil {
		return "", errors.New("ghostty: terminal closed")
	}

	// Normalize so start ≤ end in row-major order.
	if endRow < startRow || (endRow == startRow && endCol < startCol) {
		startCol, endCol = endCol, startCol
		startRow, endRow = endRow, startRow
	}

	startPoint := C.roost_viewport_point(C.uint16_t(startCol), C.uint32_t(startRow))
	endPoint := C.roost_viewport_point(C.uint16_t(endCol), C.uint32_t(endRow))

	var startRef, endRef C.GhosttyGridRef
	startRef.size = C.size_t(unsafe.Sizeof(startRef))
	endRef.size = C.size_t(unsafe.Sizeof(endRef))
	if rc := C.ghostty_terminal_grid_ref(t.c, startPoint, &startRef); rc != C.GHOSTTY_SUCCESS {
		return "", fmt.Errorf("grid_ref(start): %d", int(rc))
	}
	if rc := C.ghostty_terminal_grid_ref(t.c, endPoint, &endRef); rc != C.GHOSTTY_SUCCESS {
		return "", fmt.Errorf("grid_ref(end): %d", int(rc))
	}

	var sel C.GhosttySelection
	C.roost_init_selection(&sel)
	sel.start = startRef
	sel.end = endRef
	sel.rectangle = false

	var opts C.GhosttyFormatterTerminalOptions
	C.roost_init_formatter_opts(&opts)
	opts.emit = C.GHOSTTY_FORMATTER_FORMAT_PLAIN
	opts.unwrap = true
	opts.trim = true
	opts.selection = &sel

	var f C.GhosttyFormatter
	if rc := C.ghostty_formatter_terminal_new(nil, &f, t.c, opts); rc != C.GHOSTTY_SUCCESS {
		return "", fmt.Errorf("formatter_terminal_new: %d", int(rc))
	}
	defer C.ghostty_formatter_free(f)

	var outPtr *C.uint8_t
	var outLen C.size_t
	if rc := C.ghostty_formatter_format_alloc(f, nil, &outPtr, &outLen); rc != C.GHOSTTY_SUCCESS {
		return "", fmt.Errorf("formatter_format_alloc: %d", int(rc))
	}
	if outPtr == nil || outLen == 0 {
		if outPtr != nil {
			C.ghostty_free(nil, outPtr, outLen)
		}
		return "", nil
	}
	defer C.ghostty_free(nil, outPtr, outLen)

	return C.GoStringN((*C.char)(unsafe.Pointer(outPtr)), C.int(outLen)), nil
}
