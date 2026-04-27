package main

import (
	"github.com/diamondburned/gotk4/pkg/gdk/v4"

	"github.com/charliek/roost/internal/ghostty"
)

// handleKey translates a GDK key press into the right terminal escape
// sequence and writes it to the PTY. We delegate the actual encoding
// to libghostty-vt's key encoder, which handles legacy xterm sequences,
// Kitty keyboard protocol (when the foreground app opts in via CSI > u),
// modifyOtherKeys, and application-cursor mode automatically. We just
// supply the physical key + modifiers + (optional) typed text.
//
// Returns true if we consumed the key. When the user holds Cmd (Meta on
// macOS) or Super, we bail out so the window-level ShortcutController
// can see the event for app-level keybindings (Cmd-T, Cmd-W, Cmd-1..9
// on macOS; Alt-1..9 also flows through to the controller on Linux —
// see installShortcuts).
func handleKey(s *Session, keyval uint, mods uint) bool {
	gdkMods := gdk.ModifierType(mods)

	// Don't eat app shortcuts. Cmd (Meta) and Super are always
	// reserved for the window-level controller.
	if gdkMods&(gdk.MetaMask|gdk.SuperMask) != 0 {
		return false
	}

	key := gdkKeyvalToGhosttyKey(keyval)
	gMods := gdkModsToGhosttyMods(gdkMods)

	// UTF-8 text for the encoder. Skip C0 control bytes (0x00-0x1f,
	// 0x7f) and macOS PUA function-key codes — the encoder derives
	// those from the physical key + mods. Skip text for events that
	// have no glyph so the encoder doesn't think we're sending a
	// printable character.
	var utf8 string
	r := gdk.KeyvalToUnicode(keyval)
	if r >= 0x20 && r != 0x7f && (r < 0xf700 || r > 0xf8ff) {
		utf8 = string(rune(r))
	}

	// If we have neither a recognized key nor any text, the encoder
	// has nothing to work with — pass the event through to GTK.
	if key == ghostty.KeyUnidentified && utf8 == "" {
		return false
	}

	// Sync encoder options from the live terminal state every encode
	// — modes change with PTY output (cursor-key application, Kitty
	// flags). The call is cheap and resets MACOS_OPTION_AS_ALT to
	// FALSE, which is what we want (Option composes Unicode on macOS).
	s.keys.SyncFromTerminal(s.term)

	out, err := s.keys.Encode(ghostty.KeyEvent{
		Action:             ghostty.KeyActionPress,
		Key:                key,
		Mods:               gMods,
		UTF8:               utf8,
		UnshiftedCodepoint: rune(r),
	})
	if err != nil || len(out) == 0 {
		return false
	}

	// Snap viewport before delivering the keystroke — matches every
	// other terminal's "type to return to the prompt" behavior.
	s.snapToBottom()
	if _, err := s.pty.Write(out); err != nil {
		return false
	}
	return true
}
