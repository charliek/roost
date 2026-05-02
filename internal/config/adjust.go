package config

import (
	"fmt"
	"math"
	"strconv"
	"strings"
)

// AdjustMode discriminates how an Adjust applies to a natural metric.
type AdjustMode int

const (
	// AdjustModeNone means "leave the natural metric unchanged."
	AdjustModeNone AdjustMode = iota
	// AdjustModePixels adds the signed value (in display pixels) to
	// the natural metric.
	AdjustModePixels
	// AdjustModePercent adds the natural metric scaled by the signed
	// percentage (e.g. 10 means +10%, -5 means -5%).
	AdjustModePercent
)

// Adjust is a Ghostty-style cell metric tweak: zero, an absolute pixel
// offset, or a percentage of the natural metric. Stored on Config so
// the application site (cmd/roost) can apply it once it knows the
// natural value.
type Adjust struct {
	Mode  AdjustMode
	Value float64
}

// ParseAdjust parses Ghostty's adjust-* value syntax:
//
//   - "" (empty) → no-op (AdjustModeNone)
//   - "2" or "2px" or "-3px" → AdjustModePixels with the signed integer
//   - "10%" or "-5.5%" → AdjustModePercent with the signed float
//
// Whitespace around the value is tolerated. Anything else is rejected
// with an error that quotes the input so the config error message
// points at the malformed value.
func ParseAdjust(s string) (Adjust, error) {
	s = strings.TrimSpace(s)
	if s == "" {
		return Adjust{Mode: AdjustModeNone}, nil
	}
	if rest, ok := strings.CutSuffix(s, "%"); ok {
		v, err := strconv.ParseFloat(strings.TrimSpace(rest), 64)
		if err != nil {
			return Adjust{}, fmt.Errorf("not a valid percent value: %q", s)
		}
		return Adjust{Mode: AdjustModePercent, Value: v}, nil
	}
	rest := strings.TrimSuffix(s, "px")
	v, err := strconv.Atoi(strings.TrimSpace(rest))
	if err != nil {
		return Adjust{}, fmt.Errorf("expected an integer (optionally suffixed with px) or a percentage, got %q", s)
	}
	return Adjust{Mode: AdjustModePixels, Value: float64(v)}, nil
}

// Apply returns natural with the adjustment baked in, clamped to a
// minimum of 1 so a too-aggressive negative adjust can't produce a
// zero-or-negative metric (which would crash downstream geometry math).
func (a Adjust) Apply(natural int) int {
	v := natural + a.Delta(natural)
	if v < 1 {
		v = 1
	}
	return v
}

// Delta returns just the signed offset that would be added to natural,
// without summing it in. Used at sites where the offset itself is the
// useful value (e.g. computing a glyph y-shift relative to the cell
// origin). For pixel mode the natural argument is ignored.
func (a Adjust) Delta(natural int) int {
	switch a.Mode {
	case AdjustModePixels:
		return int(a.Value)
	case AdjustModePercent:
		return int(math.Round(float64(natural) * a.Value / 100.0))
	default:
		return 0
	}
}
