package main

import (
	"runtime"
	"testing"

	"github.com/charliek/roost/internal/config"
	"github.com/charliek/roost/internal/pangoextra"
)

func TestBuildFontConfigDefaults(t *testing.T) {
	cfg := config.Defaults()
	fc := BuildFontConfig(cfg)

	if fc.Family != cfg.FontFamily {
		t.Errorf("Family: got %q want %q", fc.Family, cfg.FontFamily)
	}
	if fc.SizePt != cfg.FontSizePt {
		t.Errorf("SizePt: got %d want %d", fc.SizePt, cfg.FontSizePt)
	}
	if fc.FamilyBold != "" {
		t.Errorf("FamilyBold default: want empty, got %q", fc.FamilyBold)
	}
	if len(fc.Features) != 0 {
		t.Errorf("Features default: want empty, got %+v", fc.Features)
	}
	// Defaults: hint_metrics=on, antialias=gray, hint_style platform-dependent.
	if fc.Options.HintMetrics != pangoextra.HintMetricsOn {
		t.Errorf("HintMetrics default: want On, got %v", fc.Options.HintMetrics)
	}
	if fc.Options.Antialias != pangoextra.AntialiasGray {
		t.Errorf("Antialias default: want Gray, got %v", fc.Options.Antialias)
	}
	wantHintStyle := pangoextra.HintStyleSlight
	if runtime.GOOS == "darwin" {
		wantHintStyle = pangoextra.HintStyleNone
	}
	if fc.Options.HintStyle != wantHintStyle {
		t.Errorf("HintStyle default for %s: want %v, got %v", runtime.GOOS, wantHintStyle, fc.Options.HintStyle)
	}
}

func TestBuildFontConfigUserOverridesWin(t *testing.T) {
	cfg := config.Defaults()
	cfg.HintMetrics = "off"
	cfg.Antialias = "subpixel"
	cfg.HintStyle = "full"
	cfg.FontFamilyBold = "Berkeley Mono Bold"
	cfg.FontFeatures = []string{"-calt", "+ss01"}

	fc := BuildFontConfig(cfg)

	if fc.Options.HintMetrics != pangoextra.HintMetricsOff {
		t.Errorf("HintMetrics override: got %v", fc.Options.HintMetrics)
	}
	if fc.Options.Antialias != pangoextra.AntialiasSubpixel {
		t.Errorf("Antialias override: got %v", fc.Options.Antialias)
	}
	if fc.Options.HintStyle != pangoextra.HintStyleFull {
		t.Errorf("HintStyle override: got %v", fc.Options.HintStyle)
	}
	if fc.FamilyBold != "Berkeley Mono Bold" {
		t.Errorf("FamilyBold: got %q", fc.FamilyBold)
	}
	if got := fc.JoinedFeatures(); got != "-calt,+ss01" {
		t.Errorf("JoinedFeatures: got %q", got)
	}
}

func TestBuildFontConfigEmptyOverridesKeepDefaults(t *testing.T) {
	// "default" and "" both mean "leave the platform default in place".
	for _, tc := range []struct {
		name string
		val  string
	}{
		{"empty string", ""},
		{"explicit default", "default"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			cfg := config.Defaults()
			cfg.HintMetrics = tc.val
			cfg.Antialias = tc.val
			cfg.HintStyle = tc.val

			defaults := BuildFontConfig(config.Defaults())
			fc := BuildFontConfig(cfg)
			if fc.Options != defaults.Options {
				t.Errorf("expected platform defaults to win for %q, got %+v", tc.val, fc.Options)
			}
		})
	}
}

func TestBuildFontConfigCarriesAdjusters(t *testing.T) {
	// Use values that differ from Defaults() so the assertions prove
	// the wiring carried the user's overrides, not just the defaults.
	cfg := config.Defaults()
	cfg.AdjustCellWidth = config.Adjust{Mode: config.AdjustModePixels, Value: 5}
	cfg.AdjustCellHeight = config.Adjust{Mode: config.AdjustModePercent, Value: 10}
	cfg.AdjustFontBaseline = config.Adjust{Mode: config.AdjustModePixels, Value: -1}
	cfg.FontThicken = true

	fc := BuildFontConfig(cfg)

	if fc.AdjustCellWidth != cfg.AdjustCellWidth {
		t.Errorf("AdjustCellWidth not carried: %+v", fc.AdjustCellWidth)
	}
	if fc.AdjustCellHeight != cfg.AdjustCellHeight {
		t.Errorf("AdjustCellHeight not carried: %+v", fc.AdjustCellHeight)
	}
	if fc.AdjustFontBaseline != cfg.AdjustFontBaseline {
		t.Errorf("AdjustFontBaseline not carried: %+v", fc.AdjustFontBaseline)
	}
	if !fc.FontThicken {
		t.Errorf("FontThicken not carried")
	}
}

func TestJoinedFeaturesEmpty(t *testing.T) {
	fc := FontConfig{}
	if got := fc.JoinedFeatures(); got != "" {
		t.Errorf("empty Features should yield empty string, got %q", got)
	}
}
