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
    /// `copy-on-select` setting — controls what happens to a
    /// mouse-drag selection on release. Three states match Ghostty's
    /// vocabulary. Defaults to `True` on both platforms.
    pub copy_on_select: CopyOnSelect,

    /// `clipboard-write` policy — controls whether a program running
    /// in the terminal can write the host clipboard via OSC 52. Two
    /// states; defaults to `Allow` (matches Ghostty's default).
    /// Phase 2 will add `Ask` with a consent banner.
    pub clipboard_write: ClipboardWrite,
}

/// Two-state policy for OSC 52 program-initiated clipboard writes.
/// Matches the first two values of Ghostty's `clipboard-write`
/// (`allow | deny`); `ask` is deferred until the consent banner UI
/// lands. Default is `Allow`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardWrite {
    #[default]
    Allow,
    Deny,
}

impl ClipboardWrite {
    /// Parse a config value. Accepts `allow | true | yes` → Allow and
    /// `deny | false | no` → Deny. Any other value returns `None` so
    /// the caller can log + fall back to the default.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" | "true" | "yes" => Some(Self::Allow),
            "deny" | "false" | "no" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// Three-state `copy-on-select` config value matching Ghostty's
/// `Off | True | Clipboard` semantics.
///
/// * `Off` — never auto-copy; the user must press the explicit copy
///   shortcut (`⌘C` / Ctrl+Shift+C).
/// * `True` (default) — write the selection to the "selection
///   clipboard": PRIMARY on Linux, a named per-app `NSPasteboard` on
///   Mac. Middle-click pastes from that target. The system clipboard
///   (`⌘V` / Ctrl+Shift+V) is **not** touched.
/// * `Clipboard` — write the selection to both the selection
///   clipboard and the system clipboard. Drag-and-paste-into-another-
///   app works without an explicit copy step.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CopyOnSelect {
    Off,
    #[default]
    True,
    Clipboard,
}

impl CopyOnSelect {
    /// Parse a config value. Accepts the Ghostty-compatible spellings
    /// (`off | false | no` → Off, `true | yes` → True,
    /// `clipboard | both` → Clipboard); any other value returns `None`
    /// so the caller can fall back to the default and log.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "no" => Some(Self::Off),
            "true" | "yes" => Some(Self::True),
            "clipboard" | "both" => Some(Self::Clipboard),
            _ => None,
        }
    }
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
                "copy-on-select" => {
                    if let Some(v) = CopyOnSelect::parse(value) {
                        cfg.copy_on_select = v;
                    } else {
                        tracing::warn!(
                            value,
                            "unknown copy-on-select value; falling back to default `true`"
                        );
                    }
                }
                "clipboard-write" => {
                    if let Some(v) = ClipboardWrite::parse(value) {
                        cfg.clipboard_write = v;
                    } else {
                        tracing::warn!(
                            value,
                            "unknown clipboard-write value; falling back to default `allow`"
                        );
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
    // `ROOST_CONFIG` overrides the path with an absolute file — used by
    // the E2E harness to drive the command launcher off a seeded config
    // (mirrors `ROOST_SOCKET` / `ROOST_BUNDLE_PROFILE`). Empty is ignored.
    if let Some(raw) = std::env::var_os("ROOST_CONFIG") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
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
    fn copy_on_select_defaults_to_true() {
        let cfg = RoostConfig::parse("");
        assert_eq!(cfg.copy_on_select, CopyOnSelect::True);
    }

    #[test]
    fn copy_on_select_accepts_all_three_states() {
        assert_eq!(
            RoostConfig::parse("copy-on-select = off").copy_on_select,
            CopyOnSelect::Off
        );
        assert_eq!(
            RoostConfig::parse("copy-on-select = false").copy_on_select,
            CopyOnSelect::Off
        );
        assert_eq!(
            RoostConfig::parse("copy-on-select = true").copy_on_select,
            CopyOnSelect::True
        );
        assert_eq!(
            RoostConfig::parse("copy-on-select = clipboard").copy_on_select,
            CopyOnSelect::Clipboard
        );
        assert_eq!(
            RoostConfig::parse("copy-on-select = both").copy_on_select,
            CopyOnSelect::Clipboard
        );
    }

    #[test]
    fn copy_on_select_unknown_value_keeps_default() {
        let cfg = RoostConfig::parse("copy-on-select = pancakes");
        assert_eq!(cfg.copy_on_select, CopyOnSelect::True);
    }

    #[test]
    fn clipboard_write_defaults_to_allow() {
        let cfg = RoostConfig::parse("");
        assert_eq!(cfg.clipboard_write, ClipboardWrite::Allow);
    }

    #[test]
    fn clipboard_write_accepts_allow_and_deny() {
        assert_eq!(
            RoostConfig::parse("clipboard-write = allow").clipboard_write,
            ClipboardWrite::Allow
        );
        assert_eq!(
            RoostConfig::parse("clipboard-write = true").clipboard_write,
            ClipboardWrite::Allow
        );
        assert_eq!(
            RoostConfig::parse("clipboard-write = deny").clipboard_write,
            ClipboardWrite::Deny
        );
        assert_eq!(
            RoostConfig::parse("clipboard-write = false").clipboard_write,
            ClipboardWrite::Deny
        );
    }

    #[test]
    fn clipboard_write_unknown_value_keeps_default() {
        let cfg = RoostConfig::parse("clipboard-write = ask");
        // `ask` is a phase-2 value; parse currently rejects it so the
        // default (Allow) wins. This test pins the contract so phase 2
        // remembers to update the parser.
        assert_eq!(cfg.clipboard_write, ClipboardWrite::Allow);
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
