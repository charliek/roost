package ghostty

import "testing"

func TestNewAndFree(t *testing.T) {
	term, err := NewTerminal(Options{Cols: 80, Rows: 24, MaxScrollback: 1000})
	if err != nil {
		t.Fatalf("NewTerminal: %v", err)
	}
	t.Cleanup(term.Close)

	// Sanity: feed some bytes through the VT parser. Doesn't check screen
	// contents yet (need render-state APIs); just verifies the cgo
	// boundary doesn't crash on the basic write path.
	term.VTWrite([]byte("hello\r\n"))
	term.VTWrite([]byte("\x1b[31mred\x1b[0m\r\n")) // SGR red

	if err := term.Resize(120, 40, 9, 18); err != nil {
		t.Fatalf("Resize: %v", err)
	}
	term.VTWrite([]byte("after-resize\r\n"))
}

func TestNewBadOptions(t *testing.T) {
	if _, err := NewTerminal(Options{Cols: 0, Rows: 24}); err == nil {
		t.Fatal("expected error for zero cols")
	}
}
