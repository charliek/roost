package main

import (
	"github.com/diamondburned/gotk4/pkg/gdk/v4"

	"github.com/charliek/roost/internal/ghostty"
)

// gdkKeyvalToGhosttyKey maps a GDK key symbol to libghostty-vt's
// physical key enum.
//
// GhosttyKey values are physical/layout-independent (e.g. KeyA is the
// "a" position on every keyboard regardless of layout). GDK keyvals
// are layout-dependent (KEY_a on US QWERTY, KEY_q on US Dvorak for
// the same physical key). For letter/digit keys we accept this
// mismatch as a Phase-2 best-effort: it doesn't affect typed text
// (the encoder uses UTF8 for that) but does affect modifier combos
// encoded via Kitty CSI-u (e.g. Ctrl+Shift+T → \x1b[84;6u). A future
// phase can add platform-specific keycode → Key tables for full
// layout-independence.
//
// Returns KeyUnidentified for keyvals we don't recognize; the encoder
// can still emit text-based output if KeyEvent.UTF8 is set.
func gdkKeyvalToGhosttyKey(keyval uint) ghostty.Key {
	switch keyval {
	// Letters — both lowercase and uppercase keyvals map to the
	// same physical key. The encoder reads the Shift modifier from
	// Mods; we don't double-encode it via the Key.
	case gdk.KEY_a, gdk.KEY_A:
		return ghostty.KeyA
	case gdk.KEY_b, gdk.KEY_B:
		return ghostty.KeyB
	case gdk.KEY_c, gdk.KEY_C:
		return ghostty.KeyC
	case gdk.KEY_d, gdk.KEY_D:
		return ghostty.KeyD
	case gdk.KEY_e, gdk.KEY_E:
		return ghostty.KeyE
	case gdk.KEY_f, gdk.KEY_F:
		return ghostty.KeyF
	case gdk.KEY_g, gdk.KEY_G:
		return ghostty.KeyG
	case gdk.KEY_h, gdk.KEY_H:
		return ghostty.KeyH
	case gdk.KEY_i, gdk.KEY_I:
		return ghostty.KeyI
	case gdk.KEY_j, gdk.KEY_J:
		return ghostty.KeyJ
	case gdk.KEY_k, gdk.KEY_K:
		return ghostty.KeyK
	case gdk.KEY_l, gdk.KEY_L:
		return ghostty.KeyL
	case gdk.KEY_m, gdk.KEY_M:
		return ghostty.KeyM
	case gdk.KEY_n, gdk.KEY_N:
		return ghostty.KeyN
	case gdk.KEY_o, gdk.KEY_O:
		return ghostty.KeyO
	case gdk.KEY_p, gdk.KEY_P:
		return ghostty.KeyP
	case gdk.KEY_q, gdk.KEY_Q:
		return ghostty.KeyQ
	case gdk.KEY_r, gdk.KEY_R:
		return ghostty.KeyR
	case gdk.KEY_s, gdk.KEY_S:
		return ghostty.KeyS
	case gdk.KEY_t, gdk.KEY_T:
		return ghostty.KeyT
	case gdk.KEY_u, gdk.KEY_U:
		return ghostty.KeyU
	case gdk.KEY_v, gdk.KEY_V:
		return ghostty.KeyV
	case gdk.KEY_w, gdk.KEY_W:
		return ghostty.KeyW
	case gdk.KEY_x, gdk.KEY_X:
		return ghostty.KeyX
	case gdk.KEY_y, gdk.KEY_Y:
		return ghostty.KeyY
	case gdk.KEY_z, gdk.KEY_Z:
		return ghostty.KeyZ

	// Digits.
	case gdk.KEY_0:
		return ghostty.KeyDigit0
	case gdk.KEY_1:
		return ghostty.KeyDigit1
	case gdk.KEY_2:
		return ghostty.KeyDigit2
	case gdk.KEY_3:
		return ghostty.KeyDigit3
	case gdk.KEY_4:
		return ghostty.KeyDigit4
	case gdk.KEY_5:
		return ghostty.KeyDigit5
	case gdk.KEY_6:
		return ghostty.KeyDigit6
	case gdk.KEY_7:
		return ghostty.KeyDigit7
	case gdk.KEY_8:
		return ghostty.KeyDigit8
	case gdk.KEY_9:
		return ghostty.KeyDigit9

	// Punctuation.
	case gdk.KEY_grave, gdk.KEY_asciitilde:
		return ghostty.KeyBackquote
	case gdk.KEY_minus, gdk.KEY_underscore:
		return ghostty.KeyMinus
	case gdk.KEY_equal, gdk.KEY_plus:
		return ghostty.KeyEqual
	case gdk.KEY_bracketleft, gdk.KEY_braceleft:
		return ghostty.KeyBracketLeft
	case gdk.KEY_bracketright, gdk.KEY_braceright:
		return ghostty.KeyBracketRight
	case gdk.KEY_backslash, gdk.KEY_bar:
		return ghostty.KeyBackslash
	case gdk.KEY_semicolon, gdk.KEY_colon:
		return ghostty.KeySemicolon
	case gdk.KEY_apostrophe, gdk.KEY_quotedbl:
		return ghostty.KeyQuote
	case gdk.KEY_comma, gdk.KEY_less:
		return ghostty.KeyComma
	case gdk.KEY_period, gdk.KEY_greater:
		return ghostty.KeyPeriod
	case gdk.KEY_slash, gdk.KEY_question:
		return ghostty.KeySlash

	// Whitespace + control.
	case gdk.KEY_space:
		return ghostty.KeySpace
	case gdk.KEY_Return, gdk.KEY_KP_Enter:
		return ghostty.KeyEnter
	case gdk.KEY_Tab, gdk.KEY_KP_Tab, gdk.KEY_ISO_Left_Tab:
		// Both forward Tab and ISO_Left_Tab (which GTK delivers when
		// Shift is held) map to the same physical key. Shift is
		// communicated through Mods, and the encoder produces
		// CSI Z when Shift+Tab arrives.
		return ghostty.KeyTab
	case gdk.KEY_BackSpace:
		return ghostty.KeyBackspace
	case gdk.KEY_Escape:
		return ghostty.KeyEscape

	// Navigation cluster.
	case gdk.KEY_Up:
		return ghostty.KeyArrowUp
	case gdk.KEY_Down:
		return ghostty.KeyArrowDown
	case gdk.KEY_Left:
		return ghostty.KeyArrowLeft
	case gdk.KEY_Right:
		return ghostty.KeyArrowRight
	case gdk.KEY_Home:
		return ghostty.KeyHome
	case gdk.KEY_End:
		return ghostty.KeyEnd
	case gdk.KEY_Page_Up:
		return ghostty.KeyPageUp
	case gdk.KEY_Page_Down:
		return ghostty.KeyPageDown
	case gdk.KEY_Insert:
		return ghostty.KeyInsert
	case gdk.KEY_Delete:
		return ghostty.KeyDelete

	// Function keys.
	case gdk.KEY_F1:
		return ghostty.KeyF1
	case gdk.KEY_F2:
		return ghostty.KeyF2
	case gdk.KEY_F3:
		return ghostty.KeyF3
	case gdk.KEY_F4:
		return ghostty.KeyF4
	case gdk.KEY_F5:
		return ghostty.KeyF5
	case gdk.KEY_F6:
		return ghostty.KeyF6
	case gdk.KEY_F7:
		return ghostty.KeyF7
	case gdk.KEY_F8:
		return ghostty.KeyF8
	case gdk.KEY_F9:
		return ghostty.KeyF9
	case gdk.KEY_F10:
		return ghostty.KeyF10
	case gdk.KEY_F11:
		return ghostty.KeyF11
	case gdk.KEY_F12:
		return ghostty.KeyF12
	}
	return ghostty.KeyUnidentified
}

// gdkModsToGhosttyMods translates the GDK modifier bitmask to
// libghostty-vt's modifier bitmask. Side bits (left vs right) are
// omitted — GTK's general key event delivery doesn't distinguish them
// reliably, and the encoder works fine without them.
func gdkModsToGhosttyMods(m gdk.ModifierType) ghostty.Mods {
	var out ghostty.Mods
	if m&gdk.ShiftMask != 0 {
		out |= ghostty.ModShift
	}
	if m&gdk.ControlMask != 0 {
		out |= ghostty.ModCtrl
	}
	if m&gdk.AltMask != 0 {
		out |= ghostty.ModAlt
	}
	// GDK's MetaMask is Cmd on macOS, Super on Linux. SuperMask is
	// also delivered on some platforms. Map either to ModSuper — the
	// encoder treats Super as Cmd for Kitty-protocol purposes.
	if m&(gdk.MetaMask|gdk.SuperMask) != 0 {
		out |= ghostty.ModSuper
	}
	if m&gdk.LockMask != 0 {
		out |= ghostty.ModCapsLock
	}
	return out
}
