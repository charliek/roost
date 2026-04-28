package main

import "github.com/charliek/roost/internal/ghostty"

// Theme is the set of colors Roost installs on every new terminal.
//
// Designed so a future LoadGhosttyConf(path string) (Theme, error) can
// drop in alongside DefaultTheme without touching call sites: keep the
// type a public value type, keep DefaultTheme a var (not a const), and
// keep Terminal.SetTheme accepting unpacked args so adding fields here
// is not an API break.
type Theme struct {
	Foreground          ghostty.ColorRGB
	Background          ghostty.ColorRGB
	Cursor              ghostty.ColorRGB
	SelectionBackground ghostty.ColorRGB
	SelectionForeground ghostty.ColorRGB
	Palette             [256]ghostty.ColorRGB
}

// DefaultTheme is Roost's built-in dark theme. The 16 named ANSI entries
// mirror cmux's dark palette (Sources/GhosttyConfig.swift in cmux); the
// remaining 240 entries (16–231 RGB cube, 232–255 gray ramp) are
// generated to match libghostty-vt's built-in defaults so unmodified
// indices behave identically to a stock terminal.
var DefaultTheme = Theme{
	Foreground:          rgb(0xFF, 0xFF, 0xFF),
	Background:          rgb(0x1E, 0x1E, 0x1E),
	Cursor:              rgb(0x98, 0x98, 0x9D),
	SelectionBackground: rgb(0x3F, 0x63, 0x8B),
	SelectionForeground: rgb(0xFF, 0xFF, 0xFF),
	Palette:             buildPalette(),
}

func rgb(r, g, b uint8) ghostty.ColorRGB { return ghostty.ColorRGB{R: r, G: g, B: b} }

// buildPalette returns the 256-color palette: cmux dark for indices 0–15,
// the standard xterm 6×6×6 cube for 16–231, and a 24-step gray ramp for
// 232–255. The 16-color block matches cmux; the 240-color block matches
// the algorithm in ../ghostty/src/terminal/color.zig:7-45.
func buildPalette() [256]ghostty.ColorRGB {
	var p [256]ghostty.ColorRGB

	// 0-7: standard ANSI
	p[0] = rgb(0x1A, 0x1A, 0x1A) // black
	p[1] = rgb(0xCC, 0x37, 0x2E) // red
	p[2] = rgb(0x26, 0xA4, 0x39) // green
	p[3] = rgb(0xCD, 0xAC, 0x08) // yellow
	p[4] = rgb(0x08, 0x69, 0xCB) // blue
	p[5] = rgb(0x96, 0x47, 0xBF) // magenta
	p[6] = rgb(0x47, 0x9E, 0xC2) // cyan
	p[7] = rgb(0x98, 0x98, 0x9D) // white
	// 8-15: bright variants
	p[8] = rgb(0x46, 0x46, 0x46)  // bright black (the gray Codex cards use)
	p[9] = rgb(0xFF, 0x45, 0x3A)  // bright red
	p[10] = rgb(0x32, 0xD7, 0x4B) // bright green
	p[11] = rgb(0xFF, 0xD6, 0x0A) // bright yellow
	p[12] = rgb(0x0A, 0x84, 0xFF) // bright blue
	p[13] = rgb(0xBF, 0x5A, 0xF2) // bright magenta
	p[14] = rgb(0x76, 0xD6, 0xFF) // bright cyan
	p[15] = rgb(0xFF, 0xFF, 0xFF) // bright white

	// 16-231: 6x6x6 RGB cube. Each axis takes 6 values; index 0 maps to 0,
	// indices 1-5 map to 55, 95, 135, 175, 215 (i.e. r*40 + 55).
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

	// 232-255: 24-step gray ramp. value = (n * 10) + 8 for n = 0..23.
	for n := 0; n < 24; n++ {
		v := uint8(n*10 + 8)
		p[232+n] = ghostty.ColorRGB{R: v, G: v, B: v}
	}

	return p
}

func cubeAxis(n int) uint8 {
	if n == 0 {
		return 0
	}
	return uint8(n*40 + 55)
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
