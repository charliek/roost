package ghostty

import (
	"bytes"
	"testing"
)

// newEncodingFixture builds a Terminal + KeyEncoder pair, syncs the
// encoder from the terminal, and returns both for the test to drive.
// Caller closes via t.Cleanup hooks.
func newEncodingFixture(t *testing.T) (*Terminal, *KeyEncoder) {
	t.Helper()
	term, err := NewTerminal(Options{Cols: 80, Rows: 24, MaxScrollback: 0})
	if err != nil {
		t.Fatalf("NewTerminal: %v", err)
	}
	t.Cleanup(term.Close)
	enc, err := NewKeyEncoder()
	if err != nil {
		t.Fatalf("NewKeyEncoder: %v", err)
	}
	t.Cleanup(enc.Close)
	enc.SyncFromTerminal(term)
	return term, enc
}

func encodeOrFail(t *testing.T, enc *KeyEncoder, ev KeyEvent) []byte {
	t.Helper()
	out, err := enc.Encode(ev)
	if err != nil {
		t.Fatalf("Encode err: %v", err)
	}
	return out
}

func TestKeyEncoder_PlainTab(t *testing.T) {
	_, enc := newEncodingFixture(t)
	got := encodeOrFail(t, enc, KeyEvent{
		Action: KeyActionPress, Key: KeyTab,
	})
	if !bytes.Equal(got, []byte{'\t'}) {
		t.Errorf("plain Tab: got %q, want \\t", got)
	}
}

func TestKeyEncoder_ShiftTabBackTab(t *testing.T) {
	_, enc := newEncodingFixture(t)
	got := encodeOrFail(t, enc, KeyEvent{
		Action: KeyActionPress, Key: KeyTab, Mods: ModShift,
	})
	if !bytes.Equal(got, []byte("\x1b[Z")) {
		t.Errorf("Shift+Tab: got %q, want \\x1b[Z", got)
	}
}

func TestKeyEncoder_PlainEnter(t *testing.T) {
	_, enc := newEncodingFixture(t)
	got := encodeOrFail(t, enc, KeyEvent{
		Action: KeyActionPress, Key: KeyEnter,
	})
	if !bytes.Equal(got, []byte{'\r'}) {
		t.Errorf("Enter: got %q, want \\r", got)
	}
}

func TestKeyEncoder_CtrlC(t *testing.T) {
	_, enc := newEncodingFixture(t)
	got := encodeOrFail(t, enc, KeyEvent{
		Action: KeyActionPress, Key: KeyC, Mods: ModCtrl, UTF8: "c",
	})
	if !bytes.Equal(got, []byte{0x03}) {
		t.Errorf("Ctrl+C: got %q, want \\x03", got)
	}
}

func TestKeyEncoder_ArrowsDefaultMode(t *testing.T) {
	_, enc := newEncodingFixture(t)
	cases := map[string]struct {
		k    Key
		want []byte
	}{
		"Up":    {KeyArrowUp, []byte("\x1b[A")},
		"Down":  {KeyArrowDown, []byte("\x1b[B")},
		"Right": {KeyArrowRight, []byte("\x1b[C")},
		"Left":  {KeyArrowLeft, []byte("\x1b[D")},
	}
	for name, c := range cases {
		t.Run(name, func(t *testing.T) {
			got := encodeOrFail(t, enc, KeyEvent{Action: KeyActionPress, Key: c.k})
			if !bytes.Equal(got, c.want) {
				t.Errorf("%s: got %q, want %q", name, got, c.want)
			}
		})
	}
}

func TestKeyEncoder_ArrowsApplicationCursorMode(t *testing.T) {
	// DECSET 1 (cursor key application mode): arrows produce \x1bO[ABCD]
	// instead of \x1b[[ABCD]. Drive the terminal into that mode and
	// re-sync the encoder.
	term, enc := newEncodingFixture(t)
	term.VTWrite([]byte("\x1b[?1h"))
	enc.SyncFromTerminal(term)

	got := encodeOrFail(t, enc, KeyEvent{Action: KeyActionPress, Key: KeyArrowUp})
	if !bytes.Equal(got, []byte("\x1bOA")) {
		t.Errorf("Up in app-cursor mode: got %q, want \\x1bOA", got)
	}
}

func TestKeyEncoder_ShiftEnterIsDistinguishable(t *testing.T) {
	// Shift+Enter must be encoded distinctly from plain Enter so apps
	// like Claude Code can interpret it as "newline in prompt" instead
	// of "submit." The libghostty-vt encoder produces a disambiguated
	// sequence by default (xterm modifyOtherKeys form \x1b[27;2;13~);
	// when the app has pushed Kitty keyboard flags it switches to the
	// CSI-u form (\x1b[13;2u). Either is fine — what matters is that
	// it's not bare \r.
	term, enc := newEncodingFixture(t)

	gotPlain := encodeOrFail(t, enc, KeyEvent{
		Action: KeyActionPress, Key: KeyEnter,
	})
	if !bytes.Equal(gotPlain, []byte{'\r'}) {
		t.Fatalf("baseline plain Enter: got %q, want \\r", gotPlain)
	}

	gotShift := encodeOrFail(t, enc, KeyEvent{
		Action: KeyActionPress, Key: KeyEnter, Mods: ModShift,
	})
	if bytes.Equal(gotShift, gotPlain) {
		t.Errorf("Shift+Enter: got plain \\r, expected disambiguated sequence")
	}

	// With Kitty flags pushed, Shift+Enter still differs from plain
	// Enter (and may switch to the CSI-u form).
	term.VTWrite([]byte("\x1b[>1u"))
	enc.SyncFromTerminal(term)
	gotShiftKitty := encodeOrFail(t, enc, KeyEvent{
		Action: KeyActionPress, Key: KeyEnter, Mods: ModShift,
	})
	if bytes.Equal(gotShiftKitty, []byte{'\r'}) {
		t.Errorf("Shift+Enter with Kitty flags: got plain \\r, expected disambiguated sequence")
	}
}

func TestKeyEncoder_PrintableLetterUsesText(t *testing.T) {
	_, enc := newEncodingFixture(t)
	got := encodeOrFail(t, enc, KeyEvent{
		Action: KeyActionPress, Key: KeyA, UTF8: "a",
	})
	if !bytes.Equal(got, []byte("a")) {
		t.Errorf("plain a: got %q, want a", got)
	}
}
