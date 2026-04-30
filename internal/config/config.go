package config

import (
	"bufio"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"strconv"
	"strings"
)

// Config is the parsed user-editable configuration. The file is a
// simple `key = value` format with `#` comments; `keybind` lines use
// Ghostty's `keybind = trigger=action` syntax. The format is
// intentionally tiny so it can grow organically without forcing a TOML
// dependency.
type Config struct {
	// FontFamily is a Pango family string. Comma-separated lists fall
	// through left-to-right, so the default is JetBrains Mono with
	// Monaco/monospace backstops.
	FontFamily string

	// FontSizePt is the font size in Pango points (typographic, not
	// pixels — Pango scales by display DPI internally).
	FontSizePt int

	// FontFamilyBold optionally overrides the family used for bold
	// text. Empty means "use FontFamily and let Pango synthesize bold."
	// Useful for pairing fonts (e.g. Iosevka regular + Berkeley Mono
	// Bold). Resolved through the same pickFontFamily helper as
	// FontFamily, so a comma-separated fallback list is allowed.
	FontFamilyBold string

	// FontFeatures is a list of OpenType feature tags applied at render
	// time (e.g. "-calt" to disable contextual ligatures, "+ss01" for a
	// stylistic set). Each `font_feature = ...` line in the config
	// appends one entry; the renderer joins them into a single Pango
	// font-features attribute.
	FontFeatures []string

	// HintMetrics, HintStyle, and Antialias control Cairo's glyph
	// rendering. Empty string means "use the platform default" — the
	// effective values per platform live in cmd/roost/font.go.
	//
	// HintMetrics ∈ {"on", "off", "default"}.
	// HintStyle   ∈ {"none", "slight", "medium", "full", "default"}.
	// Antialias   ∈ {"none", "gray", "subpixel", "default"}.
	HintMetrics string
	HintStyle   string
	Antialias   string

	// Theme is the name of a bundled color theme (e.g. "roost-dark",
	// "Dracula+"). Names match the filenames under cmd/roost/themes/,
	// which mirror ghostty's themes/ directory exactly. Unknown names
	// fall back to "roost-dark" with a logged warning.
	Theme string

	// Keybinds is the ordered sequence of `keybind = ...` lines from
	// the config file, applied on top of platform defaults at install
	// time. Order matters: later lines override earlier ones for the
	// same trigger. The special action "unbind" removes whatever the
	// trigger currently maps to (default or user override).
	Keybinds []Keybind
}

// Keybind is one `keybind = trigger=action` line. Trigger uses
// Ghostty's modifier-plus-key syntax (e.g. "super+shift+t"); Action
// is a snake_case action name from cmd/roost or the literal "unbind".
type Keybind struct {
	Trigger string
	Action  string
}

// Defaults returns the built-in Config used when no file exists.
func Defaults() Config {
	return Config{
		FontFamily: "JetBrains Mono, Monaco, monospace",
		FontSizePt: 12,
		Theme:      "roost-dark",
	}
}

// Load reads the config file at p.ConfigFile() and merges it on top of
// Defaults(). A missing file is not an error — it returns Defaults().
// Unknown keys are ignored (the file format is tolerant by design).
func (p Paths) Load() (Config, error) {
	cfg := Defaults()
	f, err := os.Open(p.ConfigFile())
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return cfg, nil
		}
		return cfg, err
	}
	defer f.Close()

	sc := bufio.NewScanner(f)
	lineNum := 0
	for sc.Scan() {
		lineNum++
		line := strings.TrimSpace(sc.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		key, val, ok := splitKV(line)
		if !ok {
			return cfg, fmt.Errorf("config: %s:%d: expected key = value", p.ConfigFile(), lineNum)
		}
		switch key {
		case "font_family":
			cfg.FontFamily = val
		case "font_size":
			n, perr := strconv.Atoi(val)
			if perr != nil {
				return cfg, fmt.Errorf("config: %s:%d: font_size: %w", p.ConfigFile(), lineNum, perr)
			}
			if n <= 0 {
				return cfg, fmt.Errorf("config: %s:%d: font_size must be > 0, got %d", p.ConfigFile(), lineNum, n)
			}
			cfg.FontSizePt = n
		case "font_family_bold":
			cfg.FontFamilyBold = val
		case "font_feature":
			if val == "" {
				return cfg, fmt.Errorf("config: %s:%d: font_feature: empty value", p.ConfigFile(), lineNum)
			}
			cfg.FontFeatures = append(cfg.FontFeatures, val)
		case "hint_metrics":
			if !validHintMetrics(val) {
				return cfg, fmt.Errorf("config: %s:%d: hint_metrics: %q not in {on, off, default}", p.ConfigFile(), lineNum, val)
			}
			cfg.HintMetrics = val
		case "hint_style":
			if !validHintStyle(val) {
				return cfg, fmt.Errorf("config: %s:%d: hint_style: %q not in {none, slight, medium, full, default}", p.ConfigFile(), lineNum, val)
			}
			cfg.HintStyle = val
		case "antialias":
			if !validAntialias(val) {
				return cfg, fmt.Errorf("config: %s:%d: antialias: %q not in {none, gray, subpixel, default}", p.ConfigFile(), lineNum, val)
			}
			cfg.Antialias = val
		case "keybind":
			kb, kerr := parseKeybind(val)
			if kerr != nil {
				return cfg, fmt.Errorf("config: %s:%d: keybind: %w", p.ConfigFile(), lineNum, kerr)
			}
			cfg.Keybinds = append(cfg.Keybinds, kb)
		case "theme":
			if val == "" {
				return cfg, fmt.Errorf("config: %s:%d: theme: empty value", p.ConfigFile(), lineNum)
			}
			cfg.Theme = val
		}
	}
	return cfg, sc.Err()
}

// parseKeybind splits a Ghostty-style `trigger=action` value into its
// two halves. Returns an error on missing inner `=`, empty trigger, or
// empty action.
func parseKeybind(raw string) (Keybind, error) {
	eq := strings.IndexByte(raw, '=')
	if eq < 0 {
		return Keybind{}, fmt.Errorf("expected trigger=action, got %q", raw)
	}
	trigger := strings.TrimSpace(raw[:eq])
	action := strings.TrimSpace(raw[eq+1:])
	if trigger == "" {
		return Keybind{}, fmt.Errorf("empty trigger in %q", raw)
	}
	if action == "" {
		return Keybind{}, fmt.Errorf("empty action in %q", raw)
	}
	return Keybind{Trigger: trigger, Action: action}, nil
}

func validHintMetrics(v string) bool {
	switch v {
	case "on", "off", "default":
		return true
	}
	return false
}

func validHintStyle(v string) bool {
	switch v {
	case "none", "slight", "medium", "full", "default":
		return true
	}
	return false
}

func validAntialias(v string) bool {
	switch v {
	case "none", "gray", "subpixel", "default":
		return true
	}
	return false
}

// splitKV parses one `key = value` line. Strips an optional pair of
// surrounding double quotes from the value so users can write either
// `font_family = JetBrains Mono` or `font_family = "JetBrains Mono"`.
func splitKV(line string) (key, val string, ok bool) {
	eq := strings.IndexByte(line, '=')
	if eq < 0 {
		return "", "", false
	}
	key = strings.TrimSpace(line[:eq])
	val = strings.TrimSpace(line[eq+1:])
	if len(val) >= 2 && val[0] == '"' && val[len(val)-1] == '"' {
		val = val[1 : len(val)-1]
	}
	return key, val, key != ""
}
