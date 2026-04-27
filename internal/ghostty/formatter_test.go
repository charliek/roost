package ghostty

import (
	"strings"
	"testing"
)

func TestCopyViewportSelection_RoundTripsAsciiText(t *testing.T) {
	term, err := NewTerminal(Options{Cols: 20, Rows: 5, MaxScrollback: 0})
	if err != nil {
		t.Fatalf("NewTerminal: %v", err)
	}
	defer term.Close()

	// Write three lines of known content. Use \r\n so each line lands
	// at column 0 of the next row.
	term.VTWrite([]byte("line one\r\nline two\r\nline three"))

	// Selection from row 0 col 0 through row 2 col 9 (inclusive) covers
	// all three lines in their entirety.
	got, err := CopyViewportSelection(term, 0, 0, 9, 2)
	if err != nil {
		t.Fatalf("CopyViewportSelection: %v", err)
	}
	if !strings.Contains(got, "line one") || !strings.Contains(got, "line two") || !strings.Contains(got, "line three") {
		t.Errorf("expected all three lines in output, got %q", got)
	}
}

func TestCopyViewportSelection_PreservesInteriorSpaces(t *testing.T) {
	term, err := NewTerminal(Options{Cols: 30, Rows: 3, MaxScrollback: 0})
	if err != nil {
		t.Fatalf("NewTerminal: %v", err)
	}
	defer term.Close()

	// "a   b" — three spaces in the middle. The Walk-based approach
	// would skip the empty cells; the formatter must preserve them.
	term.VTWrite([]byte("a   b"))

	got, err := CopyViewportSelection(term, 0, 0, 4, 0)
	if err != nil {
		t.Fatalf("CopyViewportSelection: %v", err)
	}
	if !strings.Contains(got, "a   b") {
		t.Errorf("expected interior spaces preserved, got %q", got)
	}
}
