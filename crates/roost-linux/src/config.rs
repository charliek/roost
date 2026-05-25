//! ~/.config/roost/config.conf parser.
//!
//! Mirrors `mac/Sources/Roost/Config.swift` 1:1 in surface area:
//! same recognized keys (`theme`, `font-family`, `font-size`,
//! `keybind = <trigger> = <action>`), same lenient-line parsing
//! (blank lines and `#`-comments dropped), same forward-compat
//! (unknown keys silently ignored).

use std::fs;
use std::path::PathBuf;

use crate::custom_command::{self, CustomCommand};

#[derive(Debug, Default, Clone)]
pub struct RoostConfig {
    pub theme_name: Option<String>,
    pub font_family: Option<String>,
    pub font_size: Option<f64>,
    /// (trigger, action) pairs in source order; later entries
    /// override earlier ones in `keybind::canonicalize_bindings`.
    pub keybinds: Vec<(String, String)>,
    /// Launcher entries from repeated `command =` lines, in source
    /// order (= picker row order). A line missing `label`/`run` is
    /// skipped (see `custom_command::parse_command_line`).
    pub commands: Vec<CustomCommand>,
}

impl RoostConfig {
    pub fn load_default() -> Self {
        let Some(path) = default_path() else {
            return Self::default();
        };
        Self::load_from(path)
    }

    pub fn load_from(path: PathBuf) -> Self {
        let Ok(content) = fs::read_to_string(&path) else {
            return Self::default();
        };
        Self::parse(&content)
    }

    pub fn parse(content: &str) -> Self {
        let mut cfg = Self::default();
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
                "theme" => cfg.theme_name = Some(value.to_string()),
                "font-family" => cfg.font_family = Some(value.trim_matches('"').to_string()),
                "font-size" => {
                    if let Ok(n) = value.parse::<f64>() {
                        if n > 0.0 {
                            cfg.font_size = Some(n);
                        }
                    }
                }
                "keybind" => {
                    // Ghostty form: `keybind = <trigger> = <action>`.
                    // The first `=` was the outer split; the value
                    // now looks like `<trigger> = <action>`.
                    if let Some((trigger, action)) = value.split_once('=') {
                        cfg.keybinds
                            .push((trigger.trim().to_string(), action.trim().to_string()));
                    }
                }
                "command" => {
                    // Launcher entry: `command = label="…" run="…" …`.
                    // The value (everything after the first `=`) is a
                    // record of quote-aware `key="value"` tokens; a line
                    // missing label/run is skipped, not fatal.
                    if let Some(c) = custom_command::parse_command_line(value) {
                        cfg.commands.push(c);
                    } else {
                        tracing::warn!(
                            line = raw.trim(),
                            "skipping malformed `command =` line (needs label + run)"
                        );
                    }
                }
                _ => {
                    // Unknown key — forward-compat with future Roost
                    // versions or ghostty-format keys we don't need.
                }
            }
        }
        cfg
    }
}

fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/roost/config.conf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_fields() {
        let cfg = RoostConfig::parse(
            r#"
            # roost config
            theme = catppuccin-mocha
            font-family = "JetBrains Mono"
            font-size = 14
            keybind = ctrl+t = new_tab
            keybind = ctrl+shift+v = paste
            "#,
        );
        assert_eq!(cfg.theme_name.as_deref(), Some("catppuccin-mocha"));
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.font_size, Some(14.0));
        assert_eq!(cfg.keybinds.len(), 2);
        assert_eq!(cfg.keybinds[0].0, "ctrl+t");
        assert_eq!(cfg.keybinds[0].1, "new_tab");
    }

    #[test]
    fn unknown_keys_dropped() {
        let cfg = RoostConfig::parse("future-roost-key = something");
        assert!(cfg.theme_name.is_none());
    }

    #[test]
    fn invalid_font_size_ignored() {
        let cfg = RoostConfig::parse("font-size = abc");
        assert!(cfg.font_size.is_none());
        let cfg = RoostConfig::parse("font-size = -5");
        assert!(cfg.font_size.is_none());
    }

    #[test]
    fn parses_command_entries_in_order() {
        let cfg = RoostConfig::parse(
            r#"
            command = label="Claude" run="claude --resume"
            command = label="Build" run="make" hold=true
            "#,
        );
        assert_eq!(cfg.commands.len(), 2);
        assert_eq!(cfg.commands[0].label, "Claude");
        assert_eq!(cfg.commands[0].run, "claude --resume");
        assert_eq!(cfg.commands[1].label, "Build");
        assert!(cfg.commands[1].hold);
    }

    #[test]
    fn malformed_command_skipped_others_load() {
        let cfg = RoostConfig::parse(
            r#"
            command = label="Good" run="ls"
            command = label="NoRun"
            command = run="orphan"
            "#,
        );
        // Only the well-formed line survives.
        assert_eq!(cfg.commands.len(), 1);
        assert_eq!(cfg.commands[0].label, "Good");
    }
}
