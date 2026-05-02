package config

import (
	"strings"
	"testing"
)

func TestParseAdjustValid(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want Adjust
	}{
		{"empty", "", Adjust{Mode: AdjustModeNone}},
		{"whitespace only", "   ", Adjust{Mode: AdjustModeNone}},
		{"bare integer", "2", Adjust{Mode: AdjustModePixels, Value: 2}},
		{"px suffix", "2px", Adjust{Mode: AdjustModePixels, Value: 2}},
		{"negative bare", "-3", Adjust{Mode: AdjustModePixels, Value: -3}},
		{"negative px", "-3px", Adjust{Mode: AdjustModePixels, Value: -3}},
		{"zero px", "0px", Adjust{Mode: AdjustModePixels, Value: 0}},
		{"percent integer", "10%", Adjust{Mode: AdjustModePercent, Value: 10}},
		{"percent float", "12.5%", Adjust{Mode: AdjustModePercent, Value: 12.5}},
		{"percent negative", "-5%", Adjust{Mode: AdjustModePercent, Value: -5}},
		{"px with surrounding whitespace", "  4px ", Adjust{Mode: AdjustModePixels, Value: 4}},
		{"px with internal whitespace tolerated", "2 px", Adjust{Mode: AdjustModePixels, Value: 2}},
		{"percent with surrounding whitespace", "  -2.5  %", Adjust{Mode: AdjustModePercent, Value: -2.5}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := ParseAdjust(tc.in)
			if err != nil {
				t.Fatalf("ParseAdjust(%q) err = %v", tc.in, err)
			}
			if got != tc.want {
				t.Errorf("ParseAdjust(%q) = %+v, want %+v", tc.in, got, tc.want)
			}
		})
	}
}

func TestParseAdjustInvalid(t *testing.T) {
	cases := []string{
		"abc",
		"2.5",    // floats not allowed for pixel mode (px is integer)
		"2.5px",  // same
		"px",     // bare suffix
		"%",      // bare suffix
		"two",    // word
		"++2",    // double sign
		"2pt",    // wrong unit
		"10 %xx", // trailing junk
	}
	for _, in := range cases {
		t.Run(in, func(t *testing.T) {
			_, err := ParseAdjust(in)
			if err == nil {
				t.Fatalf("ParseAdjust(%q) accepted, want error", in)
			}
			if !strings.Contains(err.Error(), in) && in != "" {
				t.Errorf("ParseAdjust(%q) error %q should quote the input", in, err.Error())
			}
		})
	}
}

func TestAdjustApply(t *testing.T) {
	cases := []struct {
		name    string
		adj     Adjust
		natural int
		want    int
	}{
		{"none keeps natural", Adjust{}, 16, 16},
		{"pixels add", Adjust{Mode: AdjustModePixels, Value: 4}, 16, 20},
		{"pixels subtract", Adjust{Mode: AdjustModePixels, Value: -3}, 16, 13},
		{"percent grow", Adjust{Mode: AdjustModePercent, Value: 25}, 16, 20},
		{"percent shrink", Adjust{Mode: AdjustModePercent, Value: -25}, 16, 12},
		{"percent rounds nearest", Adjust{Mode: AdjustModePercent, Value: 10}, 17, 19}, // 17 + 1.7 → round to 2 → 19
		{"clamp prevents zero", Adjust{Mode: AdjustModePixels, Value: -100}, 8, 1},
		{"clamp prevents negative", Adjust{Mode: AdjustModePercent, Value: -200}, 8, 1},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := tc.adj.Apply(tc.natural); got != tc.want {
				t.Errorf("Apply(%d) over %+v = %d, want %d", tc.natural, tc.adj, got, tc.want)
			}
		})
	}
}

func TestAdjustDelta(t *testing.T) {
	// Delta returns just the signed offset; ignores natural for pixel mode.
	cases := []struct {
		name    string
		adj     Adjust
		natural int
		want    int
	}{
		{"none", Adjust{}, 16, 0},
		{"pixels positive", Adjust{Mode: AdjustModePixels, Value: 5}, 16, 5},
		{"pixels negative", Adjust{Mode: AdjustModePixels, Value: -2}, 16, -2},
		{"pixels ignores natural", Adjust{Mode: AdjustModePixels, Value: 3}, 9999, 3},
		{"percent of natural", Adjust{Mode: AdjustModePercent, Value: 50}, 10, 5},
		{"percent rounds", Adjust{Mode: AdjustModePercent, Value: 33}, 10, 3},                     // 3.3 rounds to 3
		{"percent half rounds away from zero", Adjust{Mode: AdjustModePercent, Value: 25}, 10, 3}, // 2.5 → 3 (math.Round)
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := tc.adj.Delta(tc.natural); got != tc.want {
				t.Errorf("Delta(%d) over %+v = %d, want %d", tc.natural, tc.adj, got, tc.want)
			}
		})
	}
}
