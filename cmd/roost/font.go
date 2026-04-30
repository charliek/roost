package main

import (
	"log/slog"
	"runtime"
	"strings"

	"github.com/diamondburned/gotk4/pkg/pango"
	"github.com/diamondburned/gotk4/pkg/pangocairo"

	"github.com/charliek/roost/internal/pangoextra"
)

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
