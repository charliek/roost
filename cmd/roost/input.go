package main

import (
	"github.com/diamondburned/gotk4/pkg/gdk/v4"
)

// handleKey translates GDK key events into bytes and writes them to the
// PTY. Spike-grade: handles printable text via the keyval's Unicode
// mapping, plus a small switch for the special keys you can't live
// without (Enter, Backspace, Tab, arrows, Esc). The proper key encoder
// (full modifier handling, Kitty keyboard protocol) is later work.
//
// Returns true if we consumed the key. When the user holds Cmd (Meta on
// macOS) or Super, we bail out so the window-level ShortcutController
// can see the event for app-level keybindings (Cmd-T, Cmd-W, etc.).
func handleKey(s *Session, keyval uint, mods uint) bool {
	gdkMods := gdk.ModifierType(mods)

	// Don't eat app shortcuts. On macOS, GTK's <primary> resolves to
	// Meta (Cmd); on Linux it resolves to Ctrl, but we explicitly want
	// Ctrl-letter to flow to the shell as control bytes there. Super
	// (Linux Win key) is also reserved for app/system shortcuts.
	if gdkMods&(gdk.MetaMask|gdk.SuperMask) != 0 {
		return false
	}

	var buf []byte

	switch keyval {
	case gdk.KEY_Return, gdk.KEY_KP_Enter:
		buf = []byte{'\r'}
	case gdk.KEY_BackSpace:
		buf = []byte{0x7f}
	case gdk.KEY_Tab, gdk.KEY_KP_Tab:
		buf = []byte{'\t'}
	case gdk.KEY_Escape:
		buf = []byte{0x1b}
	case gdk.KEY_Up:
		buf = []byte("\x1b[A")
	case gdk.KEY_Down:
		buf = []byte("\x1b[B")
	case gdk.KEY_Right:
		buf = []byte("\x1b[C")
	case gdk.KEY_Left:
		buf = []byte("\x1b[D")
	case gdk.KEY_Home:
		buf = []byte("\x1b[H")
	case gdk.KEY_End:
		buf = []byte("\x1b[F")
	case gdk.KEY_Page_Up:
		buf = []byte("\x1b[5~")
	case gdk.KEY_Page_Down:
		buf = []byte("\x1b[6~")
	case gdk.KEY_Delete:
		buf = []byte("\x1b[3~")
	default:
		r := gdk.KeyvalToUnicode(keyval)
		if r == 0 {
			return false
		}
		// Ctrl-letter → control byte. Modifiers beyond plain Ctrl
		// (Ctrl-Shift, Ctrl-Alt, etc.) are not handled yet.
		if gdkMods&gdk.ControlMask != 0 && r >= 'a' && r <= 'z' {
			buf = []byte{byte(r) - 'a' + 1}
		} else if gdkMods&gdk.ControlMask != 0 && r >= 'A' && r <= 'Z' {
			buf = []byte{byte(r) - 'A' + 1}
		} else {
			buf = []byte(string(rune(r)))
		}
	}

	if len(buf) == 0 {
		return false
	}
	if _, err := s.pty.Write(buf); err != nil {
		return false
	}
	return true
}
