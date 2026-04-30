package main

import (
	"log/slog"
	"runtime"
	"strings"

	"github.com/diamondburned/gotk4/pkg/pango"
	"github.com/diamondburned/gotk4/pkg/pangocairo"

	"github.com/charliek/roost/internal/config"
	"github.com/charliek/roost/internal/pangoextra"
)

// FontConfig is the per-session font setup: family + size + features +
// Cairo rendering options. Built once at App startup from the user
// config + platform defaults; passed by value into NewSession so a tab
// owns its own copy and can mutate SizePt at runtime (cmd+/-) without
// affecting other tabs or future ones.
type FontConfig struct {
	Family     string                 // resolved through pickFontFamily
	FamilyBold string                 // optional override; "" means synthesize bold from Family
	SizePt     int                    // current point size; mutable per-tab via AdjustFontSize
	Features   []string               // OpenType feature tags (e.g. "-calt", "+ss01")
	Options    pangoextra.FontOptions // Cairo hint/AA settings; user values override defaults
}

// BuildFontConfig assembles a FontConfig from the user config layered on
// top of the platform defaults. Empty config fields keep the defaults.
func BuildFontConfig(cfg config.Config) FontConfig {
	opts := defaultFontOptions()
	if v, ok := parseAntialias(cfg.Antialias); ok {
		opts.Antialias = v
	}
	if v, ok := parseHintStyle(cfg.HintStyle); ok {
		opts.HintStyle = v
	}
	if v, ok := parseHintMetrics(cfg.HintMetrics); ok {
		opts.HintMetrics = v
	}
	return FontConfig{
		Family:     cfg.FontFamily,
		FamilyBold: cfg.FontFamilyBold,
		SizePt:     cfg.FontSizePt,
		Features:   append([]string(nil), cfg.FontFeatures...),
		Options:    opts,
	}
}

// JoinedFeatures returns the OpenType features as a single
// comma-separated string ready for pango.NewAttrFontFeatures, or "" if
// there are none. Pango's font-features attribute accepts a single
// comma-separated list, not multiple attributes.
func (fc FontConfig) JoinedFeatures() string {
	return strings.Join(fc.Features, ",")
}

// parseAntialias / parseHintStyle / parseHintMetrics map a validated
// config string to a pangoextra enum. The boolean is false when the
// input is empty or "default" (in which case the caller should keep
// the platform default rather than overriding it).
func parseAntialias(s string) (pangoextra.Antialias, bool) {
	switch s {
	case "none":
		return pangoextra.AntialiasNone, true
	case "gray":
		return pangoextra.AntialiasGray, true
	case "subpixel":
		return pangoextra.AntialiasSubpixel, true
	}
	return 0, false
}

func parseHintStyle(s string) (pangoextra.HintStyle, bool) {
	switch s {
	case "none":
		return pangoextra.HintStyleNone, true
	case "slight":
		return pangoextra.HintStyleSlight, true
	case "medium":
		return pangoextra.HintStyleMedium, true
	case "full":
		return pangoextra.HintStyleFull, true
	}
	return 0, false
}

func parseHintMetrics(s string) (pangoextra.HintMetrics, bool) {
	switch s {
	case "on":
		return pangoextra.HintMetricsOn, true
	case "off":
		return pangoextra.HintMetricsOff, true
	}
	return 0, false
}

// pickFontFamily returns the first family from a comma-separated list
// that the system actually has installed. Falls back to "monospace"
// when none of the requested families exist so callers always get a
// usable font name even if the user typo'd the config.
//
// Why this exists: Pango's pango_font_description_set_family does
// accept a comma-separated list as of 1.46, but its match-and-fallback
// behavior on macOS is unreliable — passing an unknown family at the
// head of the list silently falls through to a *proportional* default
// (usually Verdana), which produces wide cells with narrow glyphs and
// huge inter-character gaps. Resolving the family ourselves before
// SetFamily side-steps that.
func pickFontFamily(want string) string {
	candidates := splitFamilies(want)

	fontMap := pangocairo.FontMapGetDefault()
	if fontMap == nil {
		// No font map yet — caller is too early. Return the first
		// candidate so SetFamily still gets something; this path
		// shouldn't fire in practice because pickFontFamily runs
		// after gtk_init.
		if len(candidates) > 0 {
			return candidates[0]
		}
		return "monospace"
	}
	available := installedFamilies(fontMap)

	for _, c := range candidates {
		if available[strings.ToLower(c)] {
			slog.Debug("font", "picked", c, "requested", want)
			return c
		}
	}
	slog.Warn("font: no requested family installed, falling back to monospace",
		"requested", want)
	return "monospace"
}

func splitFamilies(s string) []string {
	parts := strings.Split(s, ",")
	out := make([]string, 0, len(parts))
	for _, p := range parts {
		p = strings.TrimSpace(p)
		if p != "" {
			out = append(out, p)
		}
	}
	return out
}

// defaultFontOptions returns the platform's recommended Cairo font
// options. hint_metrics=on is always set — it snaps glyph advance
// widths to integer pixels and is the single biggest contributor to
// crisp monospace cells. macOS native rendering is grayscale AA without
// hinting (Apple's font designs aren't built for it); the typical
// FreeType setup on Linux is grayscale AA with slight hinting.
func defaultFontOptions() pangoextra.FontOptions {
	opts := pangoextra.FontOptions{
		Antialias:   pangoextra.AntialiasGray,
		HintMetrics: pangoextra.HintMetricsOn,
	}
	if runtime.GOOS == "darwin" {
		opts.HintStyle = pangoextra.HintStyleNone
	} else {
		opts.HintStyle = pangoextra.HintStyleSlight
	}
	return opts
}

// installedFamilies returns a lower-cased set of every family the font
// map knows about. Looked up once at startup; the cost is a few ms.
func installedFamilies(fm pango.FontMapper) map[string]bool {
	// Cast through the concrete *pango.FontMap (the one type that
	// implements ListFamilies). pangocairo.FontMapGetDefault returns a
	// pango.FontMapper interface, but the underlying object is always
	// a FontMap.
	concrete, ok := fm.(*pango.FontMap)
	if !ok {
		return nil
	}
	out := map[string]bool{}
	for _, fam := range concrete.ListFamilies() {
		family, ok := fam.(*pango.FontFamily)
		if !ok {
			continue
		}
		out[strings.ToLower(family.Name())] = true
	}
	return out
}
