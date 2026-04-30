// Package pangoextra is a narrow cgo wrapper around the Pango/Cairo
// font option call that gotk4 doesn't expose cleanly.
//
// gotk4's pangocairo.ContextSetFontOptions binding crashes — it expects
// cairo.FontOptions to follow the gextras "record" struct convention,
// but the cairo package uses a raw native pointer. This package calls
// pango_cairo_context_set_font_options directly so Roost can control
// hint metrics, hint style, and antialiasing — the levers that make
// monospace cells look crisp on a cell-aligned grid.
//
// This is the second cgo location in Roost (after internal/ghostty).
// Adding a third should be done only with named justification in
// CLAUDE.md.
package pangoextra

// #cgo pkg-config: pangocairo
// #include <pango/pangocairo.h>
import "C"

import (
	"unsafe"

	"github.com/diamondburned/gotk4/pkg/pango"
)

// Antialias selects the cairo antialiasing mode applied during glyph
// rasterization. AntialiasDefault leaves the system default in place.
type Antialias int

const (
	AntialiasDefault Antialias = iota
	AntialiasNone
	AntialiasGray
	AntialiasSubpixel
)

// HintStyle selects how aggressively glyph outlines are fitted to the
// pixel grid. HintStyleDefault leaves the system default in place.
type HintStyle int

const (
	HintStyleDefault HintStyle = iota
	HintStyleNone
	HintStyleSlight
	HintStyleMedium
	HintStyleFull
)

// HintMetrics controls whether glyph metrics (advance widths, ascent,
// descent) are quantized to integer pixels. For a monospace terminal on
// a cell grid this should almost always be HintMetricsOn — without it,
// glyph advances carry sub-pixel fractions and text looks soft.
type HintMetrics int

const (
	HintMetricsDefault HintMetrics = iota
	HintMetricsOff
	HintMetricsOn
)

// FontOptions is the subset of cairo_font_options_t that Roost cares
// about. Zero values mean "leave the system default in place."
type FontOptions struct {
	Antialias   Antialias
	HintStyle   HintStyle
	HintMetrics HintMetrics
}

// SetFontOptions installs opts on ctx via
// pango_cairo_context_set_font_options. ctx must be a Pango context
// derived from a Cairo backend (e.g. gtk.Widget.PangoContext()); other
// contexts will accept the call but won't act on the options.
//
// Pango copies the cairo_font_options_t at call time, so the
// caller-owned options are freed before this function returns.
//
// SetFontOptions is a no-op when ctx is nil.
func SetFontOptions(ctx *pango.Context, opts FontOptions) {
	if ctx == nil {
		return
	}
	cOpts := C.cairo_font_options_create()
	defer C.cairo_font_options_destroy(cOpts)

	C.cairo_font_options_set_antialias(cOpts, cairoAntialias(opts.Antialias))
	C.cairo_font_options_set_hint_style(cOpts, cairoHintStyle(opts.HintStyle))
	C.cairo_font_options_set_hint_metrics(cOpts, cairoHintMetrics(opts.HintMetrics))

	cCtx := (*C.PangoContext)(unsafe.Pointer(ctx.Native()))
	C.pango_cairo_context_set_font_options(cCtx, cOpts)
}

func cairoAntialias(a Antialias) C.cairo_antialias_t {
	switch a {
	case AntialiasNone:
		return C.CAIRO_ANTIALIAS_NONE
	case AntialiasGray:
		return C.CAIRO_ANTIALIAS_GRAY
	case AntialiasSubpixel:
		return C.CAIRO_ANTIALIAS_SUBPIXEL
	default:
		return C.CAIRO_ANTIALIAS_DEFAULT
	}
}

func cairoHintStyle(h HintStyle) C.cairo_hint_style_t {
	switch h {
	case HintStyleNone:
		return C.CAIRO_HINT_STYLE_NONE
	case HintStyleSlight:
		return C.CAIRO_HINT_STYLE_SLIGHT
	case HintStyleMedium:
		return C.CAIRO_HINT_STYLE_MEDIUM
	case HintStyleFull:
		return C.CAIRO_HINT_STYLE_FULL
	default:
		return C.CAIRO_HINT_STYLE_DEFAULT
	}
}

func cairoHintMetrics(h HintMetrics) C.cairo_hint_metrics_t {
	switch h {
	case HintMetricsOff:
		return C.CAIRO_HINT_METRICS_OFF
	case HintMetricsOn:
		return C.CAIRO_HINT_METRICS_ON
	default:
		return C.CAIRO_HINT_METRICS_DEFAULT
	}
}
