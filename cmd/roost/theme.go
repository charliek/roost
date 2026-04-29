package main

import (
	"bufio"
	"embed"
	"fmt"
	"io"
	"strconv"
	"strings"

	"github.com/charliek/roost/internal/ghostty"
)

// bundledThemes ships the user-selectable color schemes. Filenames mirror
// ghostty's themes/ directory exactly (no extension, spaces and `+`
// preserved) so users can copy theme files in from
// /Applications/Ghostty.app/Contents/Resources/ghostty/themes/ without
// renaming.
//
//go:embed all:themes
var bundledThemes embed.FS

// Theme is the set of colors Roost installs on every new terminal. The
// shape mirrors a ghostty theme file: a 256-entry palette plus the
// renderer-side colors libghostty doesn't model (selection, cursor-text,
// bold-color).
type Theme struct {
	Foreground          ghostty.ColorRGB
	Background          ghostty.ColorRGB
	Cursor              ghostty.ColorRGB
	CursorText          ghostty.ColorRGB
	BoldColor           ghostty.ColorRGB
	SelectionBackground ghostty.ColorRGB
	SelectionForeground ghostty.ColorRGB
	Palette             [256]ghostty.ColorRGB
}

// DefaultTheme is the bundled "roost-dark" theme parsed at init. It is
// the runtime fallback when the user names an unknown theme. A parse
// failure here is a build error: the embedded file is broken.
var DefaultTheme = mustLoadTheme("roost-dark")

// LoadTheme parses a bundled ghostty-format theme file by name. Names
// match the embedded filenames (e.g. "Dracula+", "Catppuccin Mocha").
// Returns an error if the theme isn't bundled or fails to parse.
func LoadTheme(name string) (Theme, error) {
	f, err := bundledThemes.Open("themes/" + name)
	if err != nil {
		return Theme{}, fmt.Errorf("theme %q: %w", name, err)
	}
	defer f.Close()
	th, err := parseTheme(f)
	if err != nil {
		return Theme{}, fmt.Errorf("theme %q: %w", name, err)
	}
	return th, nil
}

// BundledThemeNames returns the list of embedded theme names, sorted.
// Used by docs / debug output.
func BundledThemeNames() []string {
	entries, err := bundledThemes.ReadDir("themes")
	if err != nil {
		return nil
	}
	names := make([]string, 0, len(entries))
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		names = append(names, e.Name())
	}
	return names
}

func mustLoadTheme(name string) Theme {
	th, err := LoadTheme(name)
	if err != nil {
		panic(fmt.Errorf("bundled %s", err))
	}
	return th
}

// parseTheme reads a ghostty-format theme file. Format: `key = value`
// with `#` comments, identical to roost's main config syntax. Unknown
// keys are ignored (matches ghostty's tolerant loader); bad values
// produce errors. Indices 16-255 of the palette are not user-controlled
// in theme files — fillPalette240 writes the standard 6×6×6 cube and
// 24-step gray ramp after parsing.
func parseTheme(r io.Reader) (Theme, error) {
	var th Theme
	var sawBackground, sawForeground, sawCursor bool
	var sawCursorText, sawBoldColor bool
	var sawSelectionBackground, sawSelectionForeground bool

	sc := bufio.NewScanner(r)
	lineNum := 0
	for sc.Scan() {
		lineNum++
		line := strings.TrimSpace(sc.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		key, val, ok := splitThemeKV(line)
		if !ok {
			return Theme{}, fmt.Errorf("line %d: expected key = value", lineNum)
		}
		switch key {
		case "background":
			c, err := parseHexColor(val)
			if err != nil {
				return Theme{}, fmt.Errorf("line %d: background: %w", lineNum, err)
			}
			th.Background = c
			sawBackground = true
		case "foreground":
			c, err := parseHexColor(val)
			if err != nil {
				return Theme{}, fmt.Errorf("line %d: foreground: %w", lineNum, err)
			}
			th.Foreground = c
			sawForeground = true
		case "cursor-color":
			c, err := parseHexColor(val)
			if err != nil {
				return Theme{}, fmt.Errorf("line %d: cursor-color: %w", lineNum, err)
			}
			th.Cursor = c
			sawCursor = true
		case "cursor-text":
			c, err := parseHexColor(val)
			if err != nil {
				return Theme{}, fmt.Errorf("line %d: cursor-text: %w", lineNum, err)
			}
			th.CursorText = c
			sawCursorText = true
		case "bold-color":
			c, err := parseHexColor(val)
			if err != nil {
				return Theme{}, fmt.Errorf("line %d: bold-color: %w", lineNum, err)
			}
			th.BoldColor = c
			sawBoldColor = true
		case "selection-background":
			c, err := parseHexColor(val)
			if err != nil {
				return Theme{}, fmt.Errorf("line %d: selection-background: %w", lineNum, err)
			}
			th.SelectionBackground = c
			sawSelectionBackground = true
		case "selection-foreground":
			c, err := parseHexColor(val)
			if err != nil {
				return Theme{}, fmt.Errorf("line %d: selection-foreground: %w", lineNum, err)
			}
			th.SelectionForeground = c
			sawSelectionForeground = true
		case "palette":
			idx, c, err := parsePaletteEntry(val)
			if err != nil {
				return Theme{}, fmt.Errorf("line %d: palette: %w", lineNum, err)
			}
			// 16-255 are computed; ignore any explicit overrides so we
			// don't end up with a half-customized palette.
			if idx < 16 {
				th.Palette[idx] = c
			}
		}
		// Unknown keys are silently ignored.
	}
	if err := sc.Err(); err != nil {
		return Theme{}, fmt.Errorf("read: %w", err)
	}

	// Required fields. Without these the theme would render unreadable
	// (zero-value black on black). Bundled themes always set both;
	// failing fast catches a broken theme file at LoadTheme time.
	if !sawBackground {
		return Theme{}, fmt.Errorf("missing required key: background")
	}
	if !sawForeground {
		return Theme{}, fmt.Errorf("missing required key: foreground")
	}

	// Optional fields — fall back to documented defaults so every
	// Theme value is renderable without a nil check. Presence is
	// tracked by bool, not by zero-value comparison: an explicit
	// "selection-background = #000000" must round-trip as black, not
	// be treated as unset.
	if !sawCursor {
		th.Cursor = th.Foreground
	}
	if !sawCursorText {
		th.CursorText = th.Background
	}
	if !sawBoldColor {
		th.BoldColor = th.Foreground
	}
	if !sawSelectionBackground {
		th.SelectionBackground = th.Foreground
	}
	if !sawSelectionForeground {
		th.SelectionForeground = th.Background
	}

	fillPalette240(&th.Palette)
	return th, nil
}

// fillPalette240 writes indices 16-255: the standard xterm 6×6×6 RGB
// cube (16-231) and 24-step gray ramp (232-255). Algorithm matches
// ../ghostty/src/terminal/color.zig:7-45.
func fillPalette240(p *[256]ghostty.ColorRGB) {
	i := 16
	for r := 0; r < 6; r++ {
		for g := 0; g < 6; g++ {
			for b := 0; b < 6; b++ {
				p[i] = ghostty.ColorRGB{
					R: cubeAxis(r),
					G: cubeAxis(g),
					B: cubeAxis(b),
				}
				i++
			}
		}
	}
	for n := 0; n < 24; n++ {
		v := uint8(n*10 + 8)
		p[232+n] = ghostty.ColorRGB{R: v, G: v, B: v}
	}
}

func cubeAxis(n int) uint8 {
	if n == 0 {
		return 0
	}
	return uint8(n*40 + 55)
}

// parseHexColor accepts "#RRGGBB" (case-insensitive).
func parseHexColor(s string) (ghostty.ColorRGB, error) {
	s = strings.TrimSpace(s)
	if len(s) != 7 || s[0] != '#' {
		return ghostty.ColorRGB{}, fmt.Errorf("expected #RRGGBB, got %q", s)
	}
	r, err := strconv.ParseUint(s[1:3], 16, 8)
	if err != nil {
		return ghostty.ColorRGB{}, fmt.Errorf("bad red in %q: %w", s, err)
	}
	g, err := strconv.ParseUint(s[3:5], 16, 8)
	if err != nil {
		return ghostty.ColorRGB{}, fmt.Errorf("bad green in %q: %w", s, err)
	}
	b, err := strconv.ParseUint(s[5:7], 16, 8)
	if err != nil {
		return ghostty.ColorRGB{}, fmt.Errorf("bad blue in %q: %w", s, err)
	}
	return ghostty.ColorRGB{R: uint8(r), G: uint8(g), B: uint8(b)}, nil
}

// parsePaletteEntry parses a `palette` value: "N=#RRGGBB" where N is
// 0-255. Returns the index and color.
func parsePaletteEntry(val string) (int, ghostty.ColorRGB, error) {
	eq := strings.IndexByte(val, '=')
	if eq < 0 {
		return 0, ghostty.ColorRGB{}, fmt.Errorf("expected N=#RRGGBB, got %q", val)
	}
	idx, err := strconv.Atoi(strings.TrimSpace(val[:eq]))
	if err != nil {
		return 0, ghostty.ColorRGB{}, fmt.Errorf("index in %q: %w", val, err)
	}
	if idx < 0 || idx > 255 {
		return 0, ghostty.ColorRGB{}, fmt.Errorf("index out of range [0,255]: %d", idx)
	}
	c, err := parseHexColor(strings.TrimSpace(val[eq+1:]))
	if err != nil {
		return 0, ghostty.ColorRGB{}, err
	}
	return idx, c, nil
}

// splitThemeKV mirrors config.splitKV: trim whitespace around `=`,
// strip an optional pair of surrounding double quotes from the value.
func splitThemeKV(line string) (key, val string, ok bool) {
	eq := strings.IndexByte(line, '=')
	if eq < 0 {
		return "", "", false
	}
	key = strings.TrimSpace(line[:eq])
	val = strings.TrimSpace(line[eq+1:])
	if len(val) >= 2 && val[0] == '"' && val[len(val)-1] == '"' {
		val = val[1 : len(val)-1]
	}
	return key, val, key != ""
}

// DefaultDeviceAttrs is what libghostty advertises when programs probe
// the terminal type via CSI c / CSI > c / CSI = c. Conservative
// xterm-256color-ish values: VT220 conformance, ANSI color, no sixel.
// Programs that gate on these will enable the modern feature set.
var DefaultDeviceAttrs = ghostty.DeviceAttrs{
	// DA1: \e[?62;22c — VT220 + ANSI_COLOR.
	PrimaryConformance: 62, // VT220
	PrimaryFeatures:    []uint16{22 /* ANSI_COLOR */},

	// DA2: \e[>0;276;0c — VT100 device, version 276 (matches xterm.js
	// parity placeholder used by similar terminals), no ROM cartridge.
	SecondaryDeviceType:      0,
	SecondaryFirmwareVersion: 276,
	SecondaryRomCartridge:    0,

	// DA3: programs that ask for this mostly check it exists; the value
	// is rendered as 8 hex digits (DECRPTUI). Zero is fine.
	TertiaryUnitID: 0,
}
