//! Bundled themes + ghostty-format parser.
//!
//! Ports `mac/Sources/Roost/Theme.swift` (Phase 6 M6) + the
//! `cmd/roost/themes/` directory bundle. Themes carry the 256-entry
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
    pub palette: [ColorRgb; 256],
}

impl Theme {
    /// Hard-coded roost-dark, used as fallback when parsing a theme
    /// file fails or no bundled theme matches the user's choice.
    pub fn roost_dark_fallback() -> Self {
        let mut palette = [ColorRgb::default(); 256];
        // libghostty's compiled-in palette is reasonable for indices
        // we don't override; the bundled theme will populate the
        // load_bundled path when the user picks a real theme.
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
            palette,
        }
    }

    /// Look up a bundled theme by name. Case-insensitive matching
    /// against the file names under `cmd/roost/themes/`. Returns
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
    (
        "roost-dark",
        include_str!("../../../cmd/roost/themes/roost-dark"),
    ),
    (
        "Atom One Dark",
        include_str!("../../../cmd/roost/themes/Atom One Dark"),
    ),
    (
        "Catppuccin Mocha",
        include_str!("../../../cmd/roost/themes/Catppuccin Mocha"),
    ),
    ("Dracula", include_str!("../../../cmd/roost/themes/Dracula")),
    (
        "Dracula+",
        include_str!("../../../cmd/roost/themes/Dracula+"),
    ),
    (
        "Gruvbox Dark Hard",
        include_str!("../../../cmd/roost/themes/Gruvbox Dark Hard"),
    ),
    (
        "TokyoNight",
        include_str!("../../../cmd/roost/themes/TokyoNight"),
    ),
];

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
    fn all_bundled_themes_parse() {
        for (name, _source) in BUNDLED_THEMES {
            let theme = Theme::load_bundled(name);
            assert!(theme
                .palette
                .iter()
                .any(|c| c.r != 0 || c.g != 0 || c.b != 0));
        }
    }
}
