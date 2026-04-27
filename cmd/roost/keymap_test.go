package main

import (
	"testing"

	"github.com/diamondburned/gotk4/pkg/gdk/v4"

	"github.com/charliek/roost/internal/ghostty"
)

func TestGdkKeyvalToGhosttyKey_Letters(t *testing.T) {
	cases := map[uint]ghostty.Key{
		gdk.KEY_a: ghostty.KeyA,
		gdk.KEY_A: ghostty.KeyA,
		gdk.KEY_z: ghostty.KeyZ,
		gdk.KEY_Z: ghostty.KeyZ,
		gdk.KEY_q: ghostty.KeyQ,
	}
	for keyval, want := range cases {
		if got := gdkKeyvalToGhosttyKey(keyval); got != want {
			t.Errorf("keyval %d: got %v, want %v", keyval, got, want)
		}
	}
}

func TestGdkKeyvalToGhosttyKey_TabFamily(t *testing.T) {
	// All three tab-flavored keyvals must map to KeyTab — the encoder
	// reads Shift from Mods to disambiguate forward Tab from Shift+Tab.
	cases := []uint{gdk.KEY_Tab, gdk.KEY_KP_Tab, gdk.KEY_ISO_Left_Tab}
	for _, keyval := range cases {
		if got := gdkKeyvalToGhosttyKey(keyval); got != ghostty.KeyTab {
			t.Errorf("keyval %d: got %v, want KeyTab", keyval, got)
		}
	}
}

func TestGdkKeyvalToGhosttyKey_NavigationCluster(t *testing.T) {
	cases := map[uint]ghostty.Key{
		gdk.KEY_Up:        ghostty.KeyArrowUp,
		gdk.KEY_Down:      ghostty.KeyArrowDown,
		gdk.KEY_Left:      ghostty.KeyArrowLeft,
		gdk.KEY_Right:     ghostty.KeyArrowRight,
		gdk.KEY_Home:      ghostty.KeyHome,
		gdk.KEY_End:       ghostty.KeyEnd,
		gdk.KEY_Page_Up:   ghostty.KeyPageUp,
		gdk.KEY_Page_Down: ghostty.KeyPageDown,
		gdk.KEY_Insert:    ghostty.KeyInsert,
		gdk.KEY_Delete:    ghostty.KeyDelete,
		gdk.KEY_BackSpace: ghostty.KeyBackspace,
		gdk.KEY_Escape:    ghostty.KeyEscape,
		gdk.KEY_Return:    ghostty.KeyEnter,
		gdk.KEY_KP_Enter:  ghostty.KeyEnter,
	}
	for keyval, want := range cases {
		if got := gdkKeyvalToGhosttyKey(keyval); got != want {
			t.Errorf("keyval %d: got %v, want %v", keyval, got, want)
		}
	}
}

func TestGdkKeyvalToGhosttyKey_FunctionKeys(t *testing.T) {
	cases := map[uint]ghostty.Key{
		gdk.KEY_F1:  ghostty.KeyF1,
		gdk.KEY_F5:  ghostty.KeyF5,
		gdk.KEY_F12: ghostty.KeyF12,
	}
	for keyval, want := range cases {
		if got := gdkKeyvalToGhosttyKey(keyval); got != want {
			t.Errorf("keyval %d: got %v, want %v", keyval, got, want)
		}
	}
}

func TestGdkKeyvalToGhosttyKey_UnknownYieldsUnidentified(t *testing.T) {
	// A keyval we don't map (e.g., KEY_Hyper_L, a niche modifier) must
	// produce KeyUnidentified so the caller knows to fall back.
	if got := gdkKeyvalToGhosttyKey(gdk.KEY_Hyper_L); got != ghostty.KeyUnidentified {
		t.Errorf("unmapped keyval: got %v, want KeyUnidentified", got)
	}
}

func TestGdkModsToGhosttyMods(t *testing.T) {
	cases := []struct {
		in   gdk.ModifierType
		want ghostty.Mods
	}{
		{0, 0},
		{gdk.ShiftMask, ghostty.ModShift},
		{gdk.ControlMask, ghostty.ModCtrl},
		{gdk.AltMask, ghostty.ModAlt},
		{gdk.MetaMask, ghostty.ModSuper},
		{gdk.SuperMask, ghostty.ModSuper},
		{gdk.ShiftMask | gdk.ControlMask, ghostty.ModShift | ghostty.ModCtrl},
	}
	for _, c := range cases {
		if got := gdkModsToGhosttyMods(c.in); got != c.want {
			t.Errorf("mods %v: got %v, want %v", c.in, got, c.want)
		}
	}
}
