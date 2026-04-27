package ghostty

import (
	"bytes"
	"strings"
	"testing"
)

func TestEncodePaste_BracketedWrapsAndStripsControl(t *testing.T) {
	// Bracketed mode: prefix \x1b[200~, suffix \x1b[201~, embedded
	// control bytes (NUL/ESC/DEL) replaced with spaces, embedded
	// \x1b[201~ sentinel stripped so pasted content can't escape
	// bracketed mode.
	input := []byte("hello\x00\x1b[201~world")
	out, err := EncodePaste(input, true)
	if err != nil {
		t.Fatalf("EncodePaste err: %v", err)
	}
	s := string(out)
	if !strings.HasPrefix(s, "\x1b[200~") {
		t.Errorf("missing bracketed prefix: %q", s)
	}
	if !strings.HasSuffix(s, "\x1b[201~") {
		t.Errorf("missing bracketed suffix: %q", s)
	}
	// The embedded sentinel must not survive verbatim. Strip the outer
	// wrap and check the body.
	body := s[len("\x1b[200~") : len(s)-len("\x1b[201~")]
	if strings.Contains(body, "\x1b[201~") {
		t.Errorf("embedded \\x1b[201~ sentinel survived: %q", body)
	}
	if strings.Contains(body, "\x00") {
		t.Errorf("NUL byte survived: %q", body)
	}
	if !strings.Contains(body, "hello") || !strings.Contains(body, "world") {
		t.Errorf("body lost real content: %q", body)
	}
}

func TestEncodePaste_NonBracketedConvertsNewlines(t *testing.T) {
	// Non-bracketed mode: no wrap; \n is converted to \r so a paste
	// into a shell looks like Enter key presses, one per line.
	input := []byte("a\nb\nc")
	out, err := EncodePaste(input, false)
	if err != nil {
		t.Fatalf("EncodePaste err: %v", err)
	}
	if bytes.Contains(out, []byte("\x1b[200~")) || bytes.Contains(out, []byte("\x1b[201~")) {
		t.Errorf("non-bracketed mode emitted brackets: %q", out)
	}
	if bytes.Contains(out, []byte("\n")) {
		t.Errorf("non-bracketed mode kept LF: %q", out)
	}
	if !bytes.Equal(out, []byte("a\rb\rc")) {
		t.Errorf("expected a\\rb\\rc, got %q", out)
	}
}

func TestEncodePaste_EmptyInput(t *testing.T) {
	out, err := EncodePaste(nil, true)
	if err != nil {
		t.Fatalf("EncodePaste err: %v", err)
	}
	if len(out) != 0 {
		t.Errorf("expected empty output for empty input, got %q", out)
	}
}
