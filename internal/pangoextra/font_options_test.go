package pangoextra

import (
	"testing"

	"github.com/diamondburned/gotk4/pkg/pango"
)

// TestSetFontOptionsNilContext verifies the early-out for nil contexts
// — the production caller derives the context from a GtkDrawingArea
// that may not be realized yet.
func TestSetFontOptionsNilContext(t *testing.T) {
	SetFontOptions(nil, FontOptions{
		Antialias:   AntialiasGray,
		HintStyle:   HintStyleNone,
		HintMetrics: HintMetricsOn,
	})
}

// TestSetFontOptionsRealContext exercises the cgo path end to end with
// a real PangoContext. The assertion is "doesn't crash" — there is no
// Go-side getter for cairo font options, and gotk4's
// pangocairo.ContextGetFontOptions has the same binding bug as the
// setter we're working around here.
func TestSetFontOptionsRealContext(t *testing.T) {
	cases := []struct {
		name string
		opts FontOptions
	}{
		{"all default", FontOptions{}},
		{"mac defaults", FontOptions{Antialias: AntialiasGray, HintStyle: HintStyleNone, HintMetrics: HintMetricsOn}},
		{"linux defaults", FontOptions{Antialias: AntialiasGray, HintStyle: HintStyleSlight, HintMetrics: HintMetricsOn}},
		{"subpixel + full", FontOptions{Antialias: AntialiasSubpixel, HintStyle: HintStyleFull, HintMetrics: HintMetricsOn}},
		{"no aa, metrics off", FontOptions{Antialias: AntialiasNone, HintStyle: HintStyleNone, HintMetrics: HintMetricsOff}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			ctx := pango.NewContext()
			if ctx == nil {
				t.Fatal("pango.NewContext returned nil")
			}
			SetFontOptions(ctx, tc.opts)
		})
	}
}
