package ghostty

// #cgo CFLAGS: -I${SRCDIR}/../../build/out/include
// #cgo LDFLAGS: ${SRCDIR}/../../build/out/lib/libghostty-vt.a
// #include <ghostty/vt.h>
// #include <stdlib.h>
import "C"

import (
	"fmt"
	"unsafe"
)

// EncodePaste prepares clipboard data for writing to a PTY. Wraps
// libghostty-vt's ghostty_paste_encode, which handles bracketed-paste
// wrapping (\x1b[200~ … \x1b[201~) when bracketed is true, strips
// unsafe control bytes (NUL/ESC/DEL → space — including any embedded
// \x1b[201~ sentinel that would otherwise let pasted content escape
// bracketed mode), and replaces \n with \r when bracketed is false.
//
// The input slice may be modified in place during encoding.
//
// Caller is responsible for querying Terminal.BracketedPasteEnabled()
// to decide the bracketed flag.
func EncodePaste(data []byte, bracketed bool) ([]byte, error) {
	if len(data) == 0 {
		return nil, nil
	}
	dataPtr := (*C.char)(unsafe.Pointer(&data[0]))
	dataLen := C.size_t(len(data))

	// Size query: pass a NULL buffer to learn the required size.
	var needed C.size_t
	rc := C.ghostty_paste_encode(dataPtr, dataLen, C.bool(bracketed), nil, 0, &needed)
	if rc != C.GHOSTTY_SUCCESS && rc != C.GHOSTTY_OUT_OF_SPACE {
		return nil, fmt.Errorf("ghostty_paste_encode size query failed: %d", int(rc))
	}
	if needed == 0 {
		return nil, nil
	}

	out := make([]byte, int(needed))
	var written C.size_t
	rc = C.ghostty_paste_encode(
		dataPtr, dataLen, C.bool(bracketed),
		(*C.char)(unsafe.Pointer(&out[0])), needed, &written,
	)
	if rc != C.GHOSTTY_SUCCESS {
		return nil, fmt.Errorf("ghostty_paste_encode failed: %d", int(rc))
	}
	return out[:int(written)], nil
}
