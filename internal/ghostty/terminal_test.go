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

func TestRenderStateWalk(t *testing.T) {
	term, err := NewTerminal(Options{Cols: 10, Rows: 3, MaxScrollback: 100})
	if err != nil {
		t.Fatalf("NewTerminal: %v", err)
	}
	t.Cleanup(term.Close)

	rs, err := NewRenderState()
	if err != nil {
		t.Fatalf("NewRenderState: %v", err)
	}
	t.Cleanup(rs.Close)

	term.VTWrite([]byte("hi"))
	if err := rs.Update(term); err != nil {
		t.Fatalf("Update: %v", err)
	}

	var got []rune
	rs.Walk(func(_, _ int, c Cell) {
		if c.Codepoint != 0 {
			got = append(got, c.Codepoint)
		}
	})
	if string(got) != "hi" {
		t.Fatalf("expected codepoints 'hi', got %q", string(got))
	}

	col, row, visible := rs.CursorPos()
	if !visible || row != 0 || col != 2 {
		t.Fatalf("cursor: expected (2,0) visible, got (%d,%d) visible=%v", col, row, visible)
	}
}
