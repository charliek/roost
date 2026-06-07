//! Bundled themes + ghostty-format parser.
//!
//! Ports `mac/Sources/Roost/Theme.swift` (Phase 6 M6). The theme files
//! live in `crates/roost-linux/src/resources/themes/` (the Rust
//! source-of-truth, alongside the vendored icons) and a byte-identical
//! copy in `mac/Sources/Roost/Resources/themes/` for the Swift bundle;
//! `make themes-check` guards the two against drift. Themes carry the
//! 256-entry
//! libghostty palette plus the chrome colors (foreground, background,
//! cursor, selection). Each theme file is embedded via `include_str!`
//! so the binary is self-contained — no `XDG_DATA_DIRS` lookup, no
//! ~/.config search for the bundled set. User-supplied theme files
//! live in `~/.config/roost/themes/<name>` and load via the same
//! parser (commit 11 includes the bundled set; user overrides land in
//! a follow-up sub-commit).

use roost_vt::ColorRgb;

/// 256-entry palette + chrome colors. Layout matches the bytes
/// `Terminal::set_color_palette` expects.
#[derive(Debug, Clone)]
#[allow(dead_code)] // selection_foreground reserved for commit 11+ selection contrast tuning.
pub struct Theme {
    pub background: ColorRgb,
    pub foreground: ColorRgb,
    pub cursor: ColorRgb,
    pub selection_background: ColorRgb,
    pub selection_foreground: ColorRgb,
    /// Ghostty `bold-color` accent: when `Some`, bold cells whose
    /// foreground is the default (no explicit SGR fg) render in this
    /// color. `None` leaves bold default-fg cells rendering in the
    /// canvas default — `resolve_cell_colors` already handles both
    /// branches, and keeping the field optional makes "theme didn't
    /// opt in" trivially visible in tests.
    pub bold_color: Option<ColorRgb>,
    pub palette: [ColorRgb; 256],
}

impl Theme {
    /// Hard-coded roost-dark, used as fallback when parsing a theme
    /// file fails or no bundled theme matches the user's choice.
    pub fn roost_dark_fallback() -> Self {
        // Start from the full standard xterm 256-color palette so the
        // 6×6×6 cube (16–231) + grayscale ramp (232–255) are correct;
        // the theme's own ANSI colors (0–7 here, 0–15 in theme files)
        // override on top. Without this base, indices 16–255 were a flat
        // placeholder and every `SGR 48;5;N` cell rendered the same wrong
        // color — `set_color_palette` pushes the full 256-entry array, so
        // it overwrote libghostty's correct compiled-in cube/ramp too.
        let mut palette = standard_xterm_256();
        palette[0] = ColorRgb::new(0x1a, 0x1a, 0x1a);
        palette[1] = ColorRgb::new(0xcc, 0x37, 0x2e);
        palette[2] = ColorRgb::new(0x26, 0xa4, 0x39);
        palette[3] = ColorRgb::new(0xcd, 0xac, 0x08);
        palette[4] = ColorRgb::new(0x08, 0x69, 0xcb);
        palette[5] = ColorRgb::new(0x96, 0x47, 0xbf);
        palette[6] = ColorRgb::new(0x47, 0x9e, 0xc2);
        palette[7] = ColorRgb::new(0x98, 0x98, 0x9d);
        Self {
            background: ColorRgb::new(0x1c, 0x1c, 0x1c),
            foreground: ColorRgb::new(0xe5, 0xe5, 0xe5),
            cursor: ColorRgb::new(0xbb, 0xbb, 0xbb),
            selection_background: ColorRgb::new(0x44, 0x4f, 0x69),
            selection_foreground: ColorRgb::new(0xff, 0xff, 0xff),
            bold_color: None,
            palette,
        }
    }

    /// Look up a bundled theme by name. Case-insensitive matching
    /// against the file names under `resources/themes/`. Returns
    /// the fallback if nothing matches so the UI always renders.
    pub fn load_bundled(name: &str) -> Self {
        for (theme_name, source) in BUNDLED_THEMES {
            if theme_name.eq_ignore_ascii_case(name) {
                if let Some(parsed) = parse_theme(source) {
                    return parsed;
                }
            }
        }
        Self::roost_dark_fallback()
    }

    /// Backwards-compat alias used by `TerminalView::with_theme` and
    /// the App bootstrap before a config-driven theme name is known.
    pub fn roost_dark() -> Self {
        Self::load_bundled("roost-dark")
    }

    /// Bundled theme names, **sorted** — feeds the command palette's
    /// "Select Theme…" list. Names are returned verbatim (so
    /// `roost-dark` stays lowercase) and match the `load_bundled` keys
    /// 1:1. Mirrors the Mac UI's `Theme.bundledNames()`.
    pub fn bundled_names() -> Vec<String> {
        let mut names: Vec<String> = BUNDLED_THEMES.iter().map(|(n, _)| n.to_string()).collect();
        names.sort();
        names
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::roost_dark()
    }
}

/// Bundled theme set: name → file contents, embedded via
/// `include_str!`. New themes can be dropped into the array below;
/// they're matched case-insensitively in `Theme::load_bundled`.
const BUNDLED_THEMES: &[(&str, &str)] = &[
    ("roost-dark", include_str!("resources/themes/roost-dark")),
    (
        "Atom One Dark",
        include_str!("resources/themes/Atom One Dark"),
    ),
    (
        "Catppuccin Mocha",
        include_str!("resources/themes/Catppuccin Mocha"),
    ),
    ("Dracula", include_str!("resources/themes/Dracula")),
    ("Dracula+", include_str!("resources/themes/Dracula+")),
    (
        "Gruvbox Dark Hard",
        include_str!("resources/themes/Gruvbox Dark Hard"),
    ),
    ("TokyoNight", include_str!("resources/themes/TokyoNight")),
    // Additional Ghostty-format themes (byte-identical to upstream).
    ("0x96f", include_str!("resources/themes/0x96f")),
    ("Atom", include_str!("resources/themes/Atom")),
    (
        "Atom One Light",
        include_str!("resources/themes/Atom One Light"),
    ),
    ("Ayu Light", include_str!("resources/themes/Ayu Light")),
    ("Ayu Mirage", include_str!("resources/themes/Ayu Mirage")),
    ("Nord", include_str!("resources/themes/Nord")),
    ("Rose Pine", include_str!("resources/themes/Rose Pine")),
    (
        "Solarized Dark Patched",
        include_str!("resources/themes/Solarized Dark Patched"),
    ),
    (
        "Catppuccin Frappe",
        include_str!("resources/themes/Catppuccin Frappe"),
    ),
    (
        "Catppuccin Macchiato",
        include_str!("resources/themes/Catppuccin Macchiato"),
    ),
    (
        "TokyoNight Storm",
        include_str!("resources/themes/TokyoNight Storm"),
    ),
    (
        "TokyoNight Night",
        include_str!("resources/themes/TokyoNight Night"),
    ),
    (
        "Gruvbox Dark",
        include_str!("resources/themes/Gruvbox Dark"),
    ),
    (
        "One Half Dark",
        include_str!("resources/themes/One Half Dark"),
    ),
    (
        "GitHub Dark Default",
        include_str!("resources/themes/GitHub Dark Default"),
    ),
    (
        "Everforest Dark Hard",
        include_str!("resources/themes/Everforest Dark Hard"),
    ),
    (
        "Kanagawa Wave",
        include_str!("resources/themes/Kanagawa Wave"),
    ),
];

/// The standard xterm 256-color palette: 16 ANSI colors (0–15), the
/// 6×6×6 color cube (16–231), and the 24-step grayscale ramp (232–255).
/// Used as the palette base so 256-color content (`SGR 48;5;N` /
/// `38;5;N` — vim/htop/lazygit, and opencode over SSH where COLORTERM is
/// unset) renders correctly even when a theme file only defines the 16
/// ANSI slots. Matches libghostty's and xterm's computed values.
fn standard_xterm_256() -> [ColorRgb; 256] {
    let mut p = [ColorRgb::default(); 256];
    // 0–15: standard ANSI (normal + bright).
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    for (i, &(r, g, b)) in ANSI.iter().enumerate() {
        p[i] = ColorRgb::new(r, g, b);
    }
    // 16–231: 6×6×6 color cube. Channel levels are 0, then 95 + 40·n.
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    for i in 0..216 {
        p[16 + i] = ColorRgb::new(LEVELS[(i / 36) % 6], LEVELS[(i / 6) % 6], LEVELS[i % 6]);
    }
    // 232–255: 24-step grayscale ramp, 8 + 10·n.
    for i in 0..24 {
        let v = 8 + (i as u8) * 10;
        p[232 + i] = ColorRgb::new(v, v, v);
    }
    p
}

fn parse_theme(content: &str) -> Option<Theme> {
    let mut t = Theme::roost_dark_fallback();
    let mut got_anything = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "palette" => {
                if let Some((idx, color)) = value.split_once('=') {
                    if let (Ok(i), Some(rgb)) =
                        (idx.trim().parse::<usize>(), parse_hex(color.trim()))
                    {
                        if i < 256 {
                            t.palette[i] = rgb;
                            got_anything = true;
                        }
                    }
                }
            }
            "background" => {
                if let Some(rgb) = parse_hex(value) {
                    t.background = rgb;
                    got_anything = true;
                }
            }
            "foreground" => {
                if let Some(rgb) = parse_hex(value) {
                    t.foreground = rgb;
                    got_anything = true;
                }
            }
            "cursor-color" => {
                if let Some(rgb) = parse_hex(value) {
                    t.cursor = rgb;
                    got_anything = true;
                }
            }
            "selection-background" => {
                if let Some(rgb) = parse_hex(value) {
                    t.selection_background = rgb;
                    got_anything = true;
                }
            }
            "selection-foreground" => {
                if let Some(rgb) = parse_hex(value) {
                    t.selection_foreground = rgb;
                    got_anything = true;
                }
            }
            "bold-color" => {
                if let Some(rgb) = parse_hex(value) {
                    t.bold_color = Some(rgb);
                    got_anything = true;
                }
            }
            _ => {}
        }
    }
    if got_anything {
        Some(t)
    } else {
        None
    }
}

fn parse_hex(s: &str) -> Option<ColorRgb> {
    let s = s.trim().strip_prefix('#')?;
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(ColorRgb::new(r, g, b))
    } else if s.len() == 3 {
        let r = u8::from_str_radix(&s[0..1], 16).ok()?;
        let g = u8::from_str_radix(&s[1..2], 16).ok()?;
        let b = u8::from_str_radix(&s[2..3], 16).ok()?;
        Some(ColorRgb::new(r * 17, g * 17, b * 17))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_roost_dark_parses() {
        let theme = Theme::load_bundled("roost-dark");
        // Background should be the value set in the file, not the
        // fallback color.
        assert_eq!(theme.background.r, 0x1e);
    }

    #[test]
    fn unknown_theme_falls_back() {
        let theme = Theme::load_bundled("does-not-exist");
        assert_eq!(theme.background.r, 0x1c);
    }

    #[test]
    fn hex_short_form_expands() {
        let rgb = parse_hex("#fff").unwrap();
        assert_eq!(rgb, ColorRgb::new(0xff, 0xff, 0xff));
    }

    #[test]
    fn bundled_names_sorted_and_verbatim() {
        let names = Theme::bundled_names();
        assert!(names.contains(&"roost-dark".to_string()));
        assert!(names.contains(&"Dracula".to_string()));
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "bundled_names must be sorted");
        // Every name round-trips through load_bundled (palette ids).
        for name in &names {
            let _ = Theme::load_bundled(name);
        }
    }

    #[test]
    fn all_bundled_themes_parse() {
        for (name, _source) in BUNDLED_THEMES {
            let theme = Theme::load_bundled(name);
            assert!(theme
                .palette
                .iter()
                .any(|c| c.r != 0 || c.g != 0 || c.b != 0));
        }
    }

    #[test]
    fn bundled_roost_dark_now_has_bold_color() {
        let theme = Theme::load_bundled("roost-dark");
        assert_eq!(theme.bold_color, Some(ColorRgb::new(0xff, 0xff, 0xff)));
    }

    #[test]
    fn theme_without_bold_color_has_none() {
        let snippet = "background = #1e1e1e\nforeground = #ffffff\n";
        let parsed = parse_theme(snippet).expect("snippet has parseable lines");
        assert!(parsed.bold_color.is_none());
    }

    #[test]
    fn parser_reads_bold_color_line() {
        let snippet = "foreground = #ffffff\nbold-color = #aabbcc\n";
        let parsed = parse_theme(snippet).expect("snippet has parseable lines");
        assert_eq!(parsed.bold_color, Some(ColorRgb::new(0xaa, 0xbb, 0xcc)));
    }

    #[test]
    fn standard_xterm_256_cube_and_grayscale() {
        let p = standard_xterm_256();
        // 6×6×6 cube corners.
        assert_eq!(p[16], ColorRgb::new(0, 0, 0), "cube black");
        assert_eq!(p[231], ColorRgb::new(255, 255, 255), "cube white");
        assert_eq!(p[21], ColorRgb::new(0, 0, 255), "cube blue");
        assert_eq!(p[196], ColorRgb::new(255, 0, 0), "cube red");
        assert_eq!(p[46], ColorRgb::new(0, 255, 0), "cube green");
        // Grayscale ramp ends.
        assert_eq!(p[232], ColorRgb::new(8, 8, 8), "gray ramp start");
        assert_eq!(p[255], ColorRgb::new(238, 238, 238), "gray ramp end");
        // ANSI base.
        assert_eq!(p[15], ColorRgb::new(255, 255, 255), "ansi white");
    }

    #[test]
    fn themes_populate_256_color_cube_not_placeholder() {
        // Regression: opencode over SSH (256-color, COLORTERM unset)
        // backgrounds with `48;5;232` (#080808). Pre-fix indices 16–255
        // were a flat placeholder so every 256-color cell rendered the
        // same wrong color. The cube/ramp must survive theme loading even
        // though theme files only define the 16 ANSI slots.
        for name in Theme::bundled_names() {
            let t = Theme::load_bundled(&name);
            assert_eq!(t.palette[232], ColorRgb::new(8, 8, 8), "{name}: gray 232");
            assert_eq!(
                t.palette[196],
                ColorRgb::new(255, 0, 0),
                "{name}: cube red 196"
            );
            assert_eq!(
                t.palette[21],
                ColorRgb::new(0, 0, 255),
                "{name}: cube blue 21"
            );
        }
    }
}
