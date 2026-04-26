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

// Config is the parsed user-editable configuration. Today only font
// settings are surfaced; the file is intentionally tiny so it can grow
// organically without forcing a TOML library dependency yet.
type Config struct {
	// FontFamily is a Pango family string. Comma-separated lists fall
	// through left-to-right, so the default is JetBrains Mono with
	// Monaco/monospace backstops.
	FontFamily string

	// FontSizePt is the font size in Pango points (typographic, not
	// pixels — Pango scales by display DPI internally).
	FontSizePt int
}

// Defaults returns the built-in Config used when no file exists.
func Defaults() Config {
	return Config{
		FontFamily: "JetBrains Mono, Monaco, monospace",
		FontSizePt: 12,
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
			cfg.FontSizePt = n
		}
	}
	return cfg, sc.Err()
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
