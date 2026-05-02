package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeConfig(t *testing.T, body string) Paths {
	t.Helper()
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "config.conf"), []byte(body), 0o600); err != nil {
		t.Fatalf("write config: %v", err)
	}
	return Paths{ConfigDir: dir, DataDir: dir, RuntimeDir: dir}
}

func TestLoadDefaultsWhenMissing(t *testing.T) {
	p := Paths{ConfigDir: t.TempDir(), DataDir: t.TempDir(), RuntimeDir: t.TempDir()}
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.FontFamily == "" || cfg.FontSizePt == 0 {
		t.Fatalf("expected defaults, got %+v", cfg)
	}
	if cfg.Theme != "roost-dark" {
		t.Errorf("expected default theme roost-dark, got %q", cfg.Theme)
	}
	if len(cfg.Keybinds) != 0 {
		t.Errorf("expected no keybinds when file missing, got %+v", cfg.Keybinds)
	}
	// Cell adjusters default to +2px each — this gives roost a polished
	// look out of the box (Pango's natural cell metrics are tighter
	// than other mainstream terminals). Pinning the defaults here so
	// they don't quietly drift.
	wantW := Adjust{Mode: AdjustModePixels, Value: 2}
	wantH := Adjust{Mode: AdjustModePixels, Value: 2}
	if cfg.AdjustCellWidth != wantW {
		t.Errorf("default AdjustCellWidth: got %+v want %+v", cfg.AdjustCellWidth, wantW)
	}
	if cfg.AdjustCellHeight != wantH {
		t.Errorf("default AdjustCellHeight: got %+v want %+v", cfg.AdjustCellHeight, wantH)
	}
}

func TestLoadThemeKey(t *testing.T) {
	p := writeConfig(t, "theme = Dracula+\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.Theme != "Dracula+" {
		t.Fatalf("theme not applied: got %q", cfg.Theme)
	}
}

func TestLoadThemeWithSpaces(t *testing.T) {
	// Bundled theme names like "Catppuccin Mocha" have spaces. The
	// parser must preserve them — value is everything after the first
	// `=`, trimmed.
	p := writeConfig(t, "theme = Catppuccin Mocha\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.Theme != "Catppuccin Mocha" {
		t.Fatalf("theme not preserved: got %q", cfg.Theme)
	}
}

func TestLoadThemeQuoted(t *testing.T) {
	p := writeConfig(t, "theme = \"Gruvbox Dark Hard\"\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.Theme != "Gruvbox Dark Hard" {
		t.Fatalf("quoted theme value: got %q", cfg.Theme)
	}
}

func TestLoadThemeEmptyRejected(t *testing.T) {
	p := writeConfig(t, "theme = \n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for empty theme value")
	}
}

func TestLoadFontKeysStillWork(t *testing.T) {
	p := writeConfig(t, "font_family = Iosevka\nfont_size = 14\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.FontFamily != "Iosevka" || cfg.FontSizePt != 14 {
		t.Fatalf("font config not applied: %+v", cfg)
	}
}

func TestLoadKeybindBasic(t *testing.T) {
	p := writeConfig(t, "keybind = super+t = new_tab\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if len(cfg.Keybinds) != 1 ||
		cfg.Keybinds[0].Trigger != "super+t" ||
		cfg.Keybinds[0].Action != "new_tab" {
		t.Fatalf("keybind parse: %+v", cfg.Keybinds)
	}
}

func TestLoadKeybindWhitespaceTolerance(t *testing.T) {
	cases := []string{
		"keybind = super+t=new_tab\n",
		"keybind=super+t=new_tab\n",
		"keybind   =   super+t   =   new_tab\n",
	}
	for _, body := range cases {
		p := writeConfig(t, body)
		cfg, err := p.Load()
		if err != nil {
			t.Fatalf("Load %q: %v", body, err)
		}
		if len(cfg.Keybinds) != 1 ||
			cfg.Keybinds[0].Trigger != "super+t" ||
			cfg.Keybinds[0].Action != "new_tab" {
			t.Errorf("keybind parse %q: %+v", body, cfg.Keybinds)
		}
	}
}

func TestLoadKeybindMultipleAccumulate(t *testing.T) {
	p := writeConfig(t, ""+
		"keybind = super+t = new_tab\n"+
		"keybind = super+t = close_tab\n"+
		"keybind = super+w = unbind\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if len(cfg.Keybinds) != 3 {
		t.Fatalf("expected 3 keybinds, got %+v", cfg.Keybinds)
	}
	if cfg.Keybinds[2].Action != "unbind" {
		t.Errorf("unbind action not preserved: %+v", cfg.Keybinds[2])
	}
}

func TestLoadKeybindMalformed(t *testing.T) {
	p := writeConfig(t, "keybind = nonsense\n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for missing inner =")
	} else if !strings.Contains(err.Error(), "keybind") {
		t.Errorf("error doesn't mention keybind: %v", err)
	}
}

func TestLoadKeybindEmptyTrigger(t *testing.T) {
	p := writeConfig(t, "keybind =  = new_tab\n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for empty trigger")
	}
}

func TestLoadKeybindEmptyAction(t *testing.T) {
	p := writeConfig(t, "keybind = super+t = \n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for empty action")
	}
}

func TestLoadCommentsIgnored(t *testing.T) {
	p := writeConfig(t, ""+
		"# leading comment\n"+
		"font_size = 11\n"+
		"# another\n"+
		"keybind = super+t = new_tab\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.FontSizePt != 11 {
		t.Errorf("font_size after comments: %+v", cfg)
	}
	if len(cfg.Keybinds) != 1 {
		t.Errorf("keybind after comments: %+v", cfg.Keybinds)
	}
}

func TestLoadFontFamilyBold(t *testing.T) {
	p := writeConfig(t, "font_family_bold = Berkeley Mono Bold\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.FontFamilyBold != "Berkeley Mono Bold" {
		t.Fatalf("font_family_bold not applied: %q", cfg.FontFamilyBold)
	}
}

func TestLoadFontFeatureRepeatable(t *testing.T) {
	p := writeConfig(t, ""+
		"font_feature = -calt\n"+
		"font_feature = +ss01\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if len(cfg.FontFeatures) != 2 ||
		cfg.FontFeatures[0] != "-calt" ||
		cfg.FontFeatures[1] != "+ss01" {
		t.Fatalf("font_feature accumulation: %+v", cfg.FontFeatures)
	}
}

func TestLoadFontFeatureEmptyRejected(t *testing.T) {
	p := writeConfig(t, "font_feature = \n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for empty font_feature")
	}
}

func TestLoadHintAndAAValid(t *testing.T) {
	p := writeConfig(t, ""+
		"hint_metrics = on\n"+
		"hint_style = slight\n"+
		"antialias = subpixel\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.HintMetrics != "on" || cfg.HintStyle != "slight" || cfg.Antialias != "subpixel" {
		t.Fatalf("hint/aa not applied: %+v", cfg)
	}
}

func TestLoadHintAndAAEmptyAccepted(t *testing.T) {
	// Empty value means "use the platform default" per docs/reference/fonts.md.
	// Parser must accept it without error so the documented config syntax works.
	p := writeConfig(t, ""+
		"hint_metrics = \n"+
		"hint_style = \n"+
		"antialias = \n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.HintMetrics != "" || cfg.HintStyle != "" || cfg.Antialias != "" {
		t.Errorf("blank values should round-trip as empty strings: %+v", cfg)
	}
}

func TestLoadHintMetricsInvalid(t *testing.T) {
	p := writeConfig(t, "hint_metrics = sometimes\n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for invalid hint_metrics value")
	}
}

func TestLoadHintStyleInvalid(t *testing.T) {
	p := writeConfig(t, "hint_style = aggressive\n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for invalid hint_style value")
	}
}

func TestLoadAntialiasInvalid(t *testing.T) {
	p := writeConfig(t, "antialias = quantum\n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for invalid antialias value")
	}
}

func TestLoadAdjustKeysValid(t *testing.T) {
	p := writeConfig(t, ""+
		"adjust_cell_width = 2px\n"+
		"adjust_cell_height = 10%\n"+
		"adjust_font_baseline = -1\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.AdjustCellWidth != (Adjust{Mode: AdjustModePixels, Value: 2}) {
		t.Errorf("AdjustCellWidth: %+v", cfg.AdjustCellWidth)
	}
	if cfg.AdjustCellHeight != (Adjust{Mode: AdjustModePercent, Value: 10}) {
		t.Errorf("AdjustCellHeight: %+v", cfg.AdjustCellHeight)
	}
	if cfg.AdjustFontBaseline != (Adjust{Mode: AdjustModePixels, Value: -1}) {
		t.Errorf("AdjustFontBaseline: %+v", cfg.AdjustFontBaseline)
	}
}

func TestLoadAdjustEmptyOverridesDefault(t *testing.T) {
	// Defaults() sets AdjustCellWidth/Height to +2px. Writing an
	// explicit blank value lets the user opt out — ParseAdjust("")
	// returns AdjustModeNone and the case branch unconditionally
	// assigns it, so `adjust_cell_width =` wins over the default.
	p := writeConfig(t, ""+
		"adjust_cell_width = \n"+
		"adjust_cell_height = \n"+
		"adjust_font_baseline = \n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.AdjustCellWidth.Mode != AdjustModeNone ||
		cfg.AdjustCellHeight.Mode != AdjustModeNone ||
		cfg.AdjustFontBaseline.Mode != AdjustModeNone {
		t.Errorf("blank adjust values should override default to no-op, got %+v", cfg)
	}
}

func TestLoadAdjustInvalid(t *testing.T) {
	p := writeConfig(t, "adjust_cell_height = nonsense\n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for invalid adjust_cell_height value")
	} else if !strings.Contains(err.Error(), "adjust_cell_height") {
		t.Errorf("error doesn't mention adjust_cell_height: %v", err)
	}
}

func TestLoadFontThicken(t *testing.T) {
	cases := map[string]bool{
		"font_thicken = true\n":  true,
		"font_thicken = false\n": false,
		"font_thicken = 1\n":     true,
		"font_thicken = 0\n":     false,
	}
	for body, want := range cases {
		t.Run(body, func(t *testing.T) {
			p := writeConfig(t, body)
			cfg, err := p.Load()
			if err != nil {
				t.Fatalf("Load %q: %v", body, err)
			}
			if cfg.FontThicken != want {
				t.Errorf("Load %q: got FontThicken=%v want %v", body, cfg.FontThicken, want)
			}
		})
	}
}

func TestLoadFontThickenInvalid(t *testing.T) {
	p := writeConfig(t, "font_thicken = sometimes\n")
	if _, err := p.Load(); err == nil {
		t.Fatalf("expected error for invalid font_thicken value")
	}
}

// TestLoadKeybindTrailingHashNotStripped pins the parser's behavior
// when a `#` appears after the action — it is NOT treated as an inline
// comment, so it ends up as part of the action string. Documentation
// must avoid trailing `#` on `keybind` lines for this reason.
func TestLoadKeybindTrailingHashNotStripped(t *testing.T) {
	p := writeConfig(t, "keybind = super+t = new_tab # trailing\n")
	cfg, err := p.Load()
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if len(cfg.Keybinds) != 1 {
		t.Fatalf("expected 1 keybind, got %+v", cfg.Keybinds)
	}
	got := cfg.Keybinds[0].Action
	if got == "new_tab" {
		t.Errorf("trailing # appears to have been stripped — doc examples assume it is preserved; got Action=%q", got)
	}
	if !strings.Contains(got, "trailing") {
		t.Errorf("Action does not include the trailing comment text: %q", got)
	}
}
