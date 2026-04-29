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
