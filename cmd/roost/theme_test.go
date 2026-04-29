package main

import (
	"strings"
	"testing"

	"github.com/charliek/roost/internal/ghostty"
)

func parseThemeString(t *testing.T, body string) Theme {
	t.Helper()
	th, err := parseTheme(strings.NewReader(body))
	if err != nil {
		t.Fatalf("parseTheme: %v", err)
	}
	return th
}

func TestParseThemeMinimal(t *testing.T) {
	// Non-zero palette[0] proves the entry was parsed (vs. left at the
	// zero-value default), and we assert cursor-color so it doesn't
	// silently regress.
	body := "" +
		"background = #112233\n" +
		"foreground = #aabbcc\n" +
		"cursor-color = #ffffff\n" +
		"palette = 0=#010203\n" +
		"palette = 15=#ffffff\n"
	th := parseThemeString(t, body)
	if (th.Background != ghostty.ColorRGB{R: 0x11, G: 0x22, B: 0x33}) {
		t.Errorf("background: %+v", th.Background)
	}
	if (th.Foreground != ghostty.ColorRGB{R: 0xaa, G: 0xbb, B: 0xcc}) {
		t.Errorf("foreground: %+v", th.Foreground)
	}
	if (th.Cursor != ghostty.ColorRGB{R: 0xff, G: 0xff, B: 0xff}) {
		t.Errorf("cursor-color: %+v", th.Cursor)
	}
	if (th.Palette[0] != ghostty.ColorRGB{R: 0x01, G: 0x02, B: 0x03}) {
		t.Errorf("palette[0]: %+v", th.Palette[0])
	}
	if (th.Palette[15] != ghostty.ColorRGB{R: 0xff, G: 0xff, B: 0xff}) {
		t.Errorf("palette[15]: %+v", th.Palette[15])
	}
}

func TestParseThemeFillsCubeAndGrayRamp(t *testing.T) {
	th := parseThemeString(t, "background = #000000\nforeground = #ffffff\n")
	// 16 = (0,0,0) — first cell of the 6×6×6 cube.
	if (th.Palette[16] != ghostty.ColorRGB{}) {
		t.Errorf("palette[16] should be (0,0,0): %+v", th.Palette[16])
	}
	// 231 = last cube entry, all axes maxed out (5,5,5) → 255,255,255.
	if (th.Palette[231] != ghostty.ColorRGB{R: 255, G: 255, B: 255}) {
		t.Errorf("palette[231]: %+v", th.Palette[231])
	}
	// 232 = first gray-ramp entry: 0*10 + 8 = 8.
	if (th.Palette[232] != ghostty.ColorRGB{R: 8, G: 8, B: 8}) {
		t.Errorf("palette[232]: %+v", th.Palette[232])
	}
	// 255 = last gray-ramp entry: 23*10 + 8 = 238.
	if (th.Palette[255] != ghostty.ColorRGB{R: 238, G: 238, B: 238}) {
		t.Errorf("palette[255]: %+v", th.Palette[255])
	}
}

func TestParseThemeIgnoresPaletteIndicesAt16OrAbove(t *testing.T) {
	// Indices 16-255 are computed; explicit overrides in the file must
	// not leak through (would produce a half-customized palette).
	body := "" +
		"background = #000000\nforeground = #ffffff\n" +
		"palette = 16=#ff00ff\n"
	th := parseThemeString(t, body)
	if (th.Palette[16] != ghostty.ColorRGB{}) {
		t.Errorf("palette[16] should be cube start (0,0,0), got %+v", th.Palette[16])
	}
}

func TestParseThemeOptionalDefaults(t *testing.T) {
	// cursor-text falls back to background, bold-color to foreground,
	// selection-* to fg/bg, when not set.
	body := "background = #001122\nforeground = #aabbcc\n"
	th := parseThemeString(t, body)
	if (th.CursorText != ghostty.ColorRGB{R: 0x00, G: 0x11, B: 0x22}) {
		t.Errorf("cursor-text default: %+v", th.CursorText)
	}
	if (th.BoldColor != ghostty.ColorRGB{R: 0xaa, G: 0xbb, B: 0xcc}) {
		t.Errorf("bold-color default: %+v", th.BoldColor)
	}
	if (th.SelectionBackground != ghostty.ColorRGB{R: 0xaa, G: 0xbb, B: 0xcc}) {
		t.Errorf("selection-background default: %+v", th.SelectionBackground)
	}
	if (th.SelectionForeground != ghostty.ColorRGB{R: 0x00, G: 0x11, B: 0x22}) {
		t.Errorf("selection-foreground default: %+v", th.SelectionForeground)
	}
}

func TestParseThemeOptionalSet(t *testing.T) {
	body := "" +
		"background = #000000\nforeground = #ffffff\n" +
		"cursor-text = #aaaaaa\n" +
		"bold-color = #bbbbbb\n" +
		"selection-background = #123456\n" +
		"selection-foreground = #654321\n"
	th := parseThemeString(t, body)
	if (th.CursorText != ghostty.ColorRGB{R: 0xaa, G: 0xaa, B: 0xaa}) {
		t.Errorf("cursor-text: %+v", th.CursorText)
	}
	if (th.BoldColor != ghostty.ColorRGB{R: 0xbb, G: 0xbb, B: 0xbb}) {
		t.Errorf("bold-color: %+v", th.BoldColor)
	}
	if (th.SelectionBackground != ghostty.ColorRGB{R: 0x12, G: 0x34, B: 0x56}) {
		t.Errorf("selection-background: %+v", th.SelectionBackground)
	}
	if (th.SelectionForeground != ghostty.ColorRGB{R: 0x65, G: 0x43, B: 0x21}) {
		t.Errorf("selection-foreground: %+v", th.SelectionForeground)
	}
}

func TestParseThemeIgnoresUnknownKeys(t *testing.T) {
	body := "" +
		"background = #000000\nforeground = #ffffff\n" +
		"unknown-key = whatever\n" +
		"font-family = should be ignored\n"
	if _, err := parseTheme(strings.NewReader(body)); err != nil {
		t.Fatalf("unknown keys should not error: %v", err)
	}
}

func TestParseThemeCommentsAndBlankLines(t *testing.T) {
	body := "" +
		"# this is a comment\n" +
		"\n" +
		"background = #112233\n" +
		"   # indented comment also tolerated\n" +
		"foreground = #aabbcc\n"
	th := parseThemeString(t, body)
	if (th.Background != ghostty.ColorRGB{R: 0x11, G: 0x22, B: 0x33}) {
		t.Errorf("background: %+v", th.Background)
	}
}

func TestParseThemeBadHexErrors(t *testing.T) {
	cases := []string{
		"background = nothex\n",
		"background = #zz0000\n",
		"background = #fff\n",      // too short
		"background = #ffffffff\n", // too long
	}
	for _, body := range cases {
		if _, err := parseTheme(strings.NewReader(body)); err == nil {
			t.Errorf("expected error for %q", body)
		}
	}
}

func TestParseThemeBadPaletteEntry(t *testing.T) {
	cases := []string{
		"palette = 999=#ffffff\n", // out of range
		"palette = abc=#ffffff\n", // non-numeric index
		"palette = 0\n",           // missing color
	}
	for _, body := range cases {
		if _, err := parseTheme(strings.NewReader(body)); err == nil {
			t.Errorf("expected error for %q", body)
		}
	}
}

func TestLoadThemeUnknownErrors(t *testing.T) {
	if _, err := LoadTheme("NotARealTheme"); err == nil {
		t.Fatalf("expected error for unknown theme")
	}
}

func TestEveryBundledThemeParses(t *testing.T) {
	names := BundledThemeNames()
	if len(names) < 7 {
		t.Fatalf("expected at least 7 bundled themes, got %d: %v", len(names), names)
	}
	for _, name := range names {
		t.Run(name, func(t *testing.T) {
			th, err := LoadTheme(name)
			if err != nil {
				t.Fatalf("LoadTheme(%q): %v", name, err)
			}
			// Sanity: 16 palette entries customized, 240 computed.
			// Index 232 must equal the gray-ramp formula regardless of
			// the theme — proves fillPalette240 ran.
			if (th.Palette[232] != ghostty.ColorRGB{R: 8, G: 8, B: 8}) {
				t.Errorf("palette[232] not gray-ramp: %+v", th.Palette[232])
			}
			// Background must be set (every bundled theme defines it).
			if (th.Background == ghostty.ColorRGB{}) {
				t.Errorf("theme %q: background is zero", name)
			}
		})
	}
}

func TestDefaultThemeMatchesRoostDark(t *testing.T) {
	// DefaultTheme should be byte-identical to LoadTheme("roost-dark").
	rd, err := LoadTheme("roost-dark")
	if err != nil {
		t.Fatalf("LoadTheme(roost-dark): %v", err)
	}
	if DefaultTheme != rd {
		t.Errorf("DefaultTheme drifted from roost-dark theme file")
	}
}
