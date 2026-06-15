//! ~/.config/roost/config.conf parser.
//!
//! Mirrors `mac/Sources/Roost/Config.swift` 1:1 in surface area:
//! same recognized keys (`theme`, `font-family`, `font-size`,
//! `keybind = <trigger> = <action>`), same lenient-line parsing
//! (blank lines and `#`-comments dropped), same forward-compat
//! (unknown keys silently ignored).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::custom_command::{self, CustomCommand};
use crate::keybind::{self, AccelMods};
use crate::provider::{self, Provider};
use roost_linux::word_selection::DEFAULT_EXTRA_WORD_CHARS;

#[derive(Debug, Clone)]
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
    /// Dynamic, script-backed providers — `provider =` config lines (in
    /// source order) followed by executables discovered under the
    /// `providers/` dir next to the config file. Drive the custom palette
    /// (Cmd/Alt+Shift+E). See `crate::provider`.
    pub providers: Vec<Provider>,
    /// `copy-on-select` setting — controls what happens to a
    /// mouse-drag selection on release. Three states match Ghostty's
    /// vocabulary. Defaults to `True` on both platforms.
    pub copy_on_select: CopyOnSelect,

    /// `clipboard-write` policy — controls whether a program running
    /// in the terminal can write the host clipboard via OSC 52. Two
    /// states; defaults to `Allow` (matches Ghostty's default).
    /// Phase 2 will add `Ask` with a consent banner.
    pub clipboard_write: ClipboardWrite,

    /// `word-break-chars` setting — chars that count as word chars
    /// (beyond Unicode letters/digits) for double-click word
    /// expansion. Default matches Ghostty's `_-.+~/:@%`, keeping
    /// file paths + URLs whole on double-click. Despite the
    /// `-break-` name (kept for Ghostty compatibility) the value is
    /// the EXTRA word-char set, not the break-char set.
    pub word_break_chars: String,

    /// `link-modifier` — which held modifier reveals + opens a URL on
    /// hover/click (the underline + hand cursor + click-to-open).
    /// `None` means "use the platform default"
    /// ([`keybind::default_link_modifier`]: Cmd on macOS, Alt on
    /// Linux). Set `link-modifier = ctrl` for traditional Ctrl+click.
    /// GTK-only today — the Swift UI's modifier is fixed to Cmd.
    pub link_modifier: Option<AccelMods>,
}

impl Default for RoostConfig {
    fn default() -> Self {
        Self {
            theme_name: None,
            font_family: None,
            font_size: None,
            keybinds: Vec::new(),
            commands: Vec::new(),
            providers: Vec::new(),
            copy_on_select: CopyOnSelect::default(),
            clipboard_write: ClipboardWrite::default(),
            word_break_chars: DEFAULT_EXTRA_WORD_CHARS.to_string(),
            link_modifier: None,
        }
    }
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
        let mut cfg = match fs::read_to_string(&path) {
            Ok(content) => Self::parse(&content),
            Err(_) => Self::default(),
        };
        // Discovered providers append after any `provider =` config
        // entries, so config order wins and the dir fills in the rest.
        cfg.providers
            .extend(discover_providers(providers_dir(&path)));
        cfg
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
                "word-break-chars" => {
                    // Empty value is a deliberate user choice meaning
                    // "Unicode letters/digits only" — distinct from
                    // "missing key" (which falls back to the default).
                    cfg.word_break_chars = value.to_string();
                }
                "link-modifier" => {
                    if let Some(m) = keybind::parse_link_modifier(value) {
                        cfg.link_modifier = Some(m);
                    } else {
                        tracing::warn!(
                            value,
                            "unknown link-modifier value; expected ctrl|alt|super, \
                             keeping the platform default"
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
                "provider" => {
                    // Dynamic provider: `provider = label="…" run="…" …`.
                    // Same grammar as `command =`; a line missing
                    // label/run is skipped, not fatal.
                    if let Some(p) = provider::parse_provider_line(value) {
                        cfg.providers.push(p);
                    } else {
                        tracing::warn!(
                            line = raw.trim(),
                            "skipping malformed `provider =` line (needs label + run)"
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

/// Round-trip-safe edit of `~/.config/roost/config.conf`.
///
/// Replaces every line whose key (the text before the first `=`,
/// trimmed) equals `key`. The parser is "last-wins" on duplicates, so
/// replacing only the first occurrence would silently let a later
/// duplicate clobber the new value — replacing all keeps the file
/// honest. If no matching line exists, appends `<key> = <value>` at
/// the end (adding a trailing newline if the file didn't have one).
/// Comments and unrelated keys are preserved verbatim.
///
/// **NOT safe for multi-valued keys.** The `keybind = …` and
/// `command = …` entries are accumulated by the parser into vectors;
/// calling `set_key("keybind", …)` would collapse every keybind line
/// into one. Restrict callers to single-valued keys (`theme`,
/// `font-family`, `font-size`).
///
/// `value` is written verbatim — callers are responsible for adding
/// surrounding quotes when the value contains spaces (`font-family`
/// gets `"…"`; bare names like `theme = roost-dark` and numeric
/// `font-size = 14` do not). The write is atomic via tmp-file +
/// rename in the same directory. The parent directory is created if
/// missing.
pub fn set_key(path: &Path, key: &str, value: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let existing = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let new_contents = render_set_key(&existing, key, value);
    write_atomic(path, &new_contents)
}

/// Pure helper: compute the post-`set_key` file contents from the
/// existing contents. Split out so the round-trip tests can assert on
/// the exact bytes without touching the filesystem.
fn render_set_key(existing: &str, key: &str, value: &str) -> String {
    let new_line = format!("{key} = {value}");
    let mut lines: Vec<String> = if existing.is_empty() {
        // Treat an empty file as zero lines (not one empty line), so
        // an appended entry doesn't end up preceded by a blank.
        Vec::new()
    } else {
        let had_trailing_newline = existing.ends_with('\n');
        let mut v: Vec<String> = existing.split('\n').map(|s| s.to_string()).collect();
        // `split('\n')` on a trailing newline leaves a trailing empty
        // element; drop it so we can re-add a single newline at end.
        if had_trailing_newline {
            v.pop();
        }
        v
    };
    let mut replaced = false;
    for line in lines.iter_mut() {
        if line_key_matches(line, key) {
            // Preserve any leading indentation the user pretty-printed
            // with, so a hand-formatted `  theme = …` line stays
            // indented after a rewrite.
            let indent: String = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            *line = format!("{indent}{new_line}");
            replaced = true;
        }
    }
    if !replaced {
        lines.push(new_line);
    }
    let mut out = lines.join("\n");
    // Always end with a single newline so re-edits land on their own
    // line and `cat`-ing the file in a terminal doesn't dangle.
    out.push('\n');
    out
}

/// `true` when `line` is a non-comment `key = …` line whose key
/// matches `target` after trimming. Comment + blank lines (the parser
/// drops them) never count, so we don't accidentally edit a comment.
fn line_key_matches(line: &str, target: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return false;
    }
    let Some(eq) = trimmed.find('=') else {
        return false;
    };
    trimmed[..eq].trim_end() == target
}

fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Per-call nonce so two `set_key` calls from the same process
    // can't pick the same tmp filename and clobber each other's
    // write. Pid alone collides on rapid back-to-back commits (e.g.
    // theme.set immediately followed by font-family.set).
    static NONCE: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".to_string());
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{stem}.roost.tmp.{}.{}",
        std::process::id(),
        nonce
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Public so the UI can pass `&ROOST_CONFIG`-aware paths into
/// `set_key` without re-deriving them. Returns `None` when `$HOME` is
/// unset and `$ROOST_CONFIG` is empty — same fallback the loader uses.
pub fn config_path() -> Option<PathBuf> {
    default_path()
}

/// The `providers/` directory beside the config file (so the E2E
/// harness's `ROOST_CONFIG` override scopes discovery to its temp dir,
/// just like it scopes the launcher's `command =` entries).
fn providers_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("providers")
}

/// Discover executable provider scripts in `dir`, sorted by filename for
/// a stable row order. Non-files, non-executables, and dotfiles are
/// skipped; a missing dir yields an empty list. The first ~2 KiB of each
/// file is read for `# @roost.label:` / `# @roost.title:` metadata.
fn discover_providers(dir: PathBuf) -> Vec<Provider> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    let mut files: Vec<_> = entries.flatten().collect();
    files.sort_by_key(|e| e.file_name());
    for entry in files {
        let filename = entry.file_name().to_string_lossy().into_owned();
        if filename.starts_with('.') {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            continue;
        }
        let path = entry.path();
        let header = read_header(&path);
        out.push(provider::provider_from_file(
            &path.to_string_lossy(),
            &filename,
            &header,
        ));
    }
    out
}

/// Read the leading bytes of a script for header-comment metadata. Lossy
/// UTF-8 is fine — we only scan comment lines.
fn read_header(path: &Path) -> String {
    use std::io::Read;
    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    // Read at most the 2 KiB cap — a provider may be a large compiled
    // binary, so don't slurp the whole file just to scan a comment header.
    let mut buf = Vec::new();
    if file.take(2048).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
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
    fn link_modifier_unset_defaults_to_none() {
        let cfg = RoostConfig::parse("theme = Dracula\n");
        assert_eq!(cfg.link_modifier, None);
    }

    #[test]
    fn link_modifier_parses_override() {
        assert_eq!(
            RoostConfig::parse("link-modifier = ctrl\n").link_modifier,
            Some(AccelMods::CTRL)
        );
        assert_eq!(
            RoostConfig::parse("link-modifier = super\n").link_modifier,
            Some(AccelMods::SUPER)
        );
    }

    #[test]
    fn link_modifier_unknown_value_keeps_default() {
        // Unparseable value warns + leaves None (= platform default).
        assert_eq!(
            RoostConfig::parse("link-modifier = wat\n").link_modifier,
            None
        );
    }

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
    fn parses_provider_entries_in_order() {
        let cfg = RoostConfig::parse(
            r#"
            provider = label="Open shed" run="shed.sh"
            provider = label="Worktrees" run="wt.sh" timeout=8 limit=20
            "#,
        );
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].label, "Open shed");
        assert_eq!(cfg.providers[0].run, "shed.sh");
        assert_eq!(cfg.providers[1].timeout_secs, 8);
        assert_eq!(cfg.providers[1].limit, 20);
    }

    #[test]
    fn malformed_provider_skipped() {
        let cfg = RoostConfig::parse(r#"provider = label="NoRun""#);
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn discovers_executable_providers_from_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let cfgpath = tmp.path().join("config.conf");
        fs::write(
            &cfgpath,
            "theme = roost-dark\nprovider = label=\"Configured\" run=\"c.sh\"\n",
        )
        .unwrap();
        let pdir = tmp.path().join("providers");
        fs::create_dir_all(&pdir).unwrap();
        let script = pdir.join("shed.sh");
        fs::write(&script, "#!/bin/sh\n# @roost.label: Open shed\necho '{}'\n").unwrap();
        let mut perm = fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&script, perm).unwrap();
        // A non-executable file in the dir is ignored.
        fs::write(pdir.join("notes.txt"), "ignore me").unwrap();

        let cfg = RoostConfig::load_from(cfgpath);
        // Config provider first (source order), discovered script after.
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.providers[0].label, "Configured");
        assert_eq!(cfg.providers[1].label, "Open shed"); // from header metadata
                                                         // Discovered providers keep the raw path and exec directly;
                                                         // config entries are shell-interpreted.
        assert_eq!(cfg.providers[1].run, script.to_string_lossy());
        assert!(!cfg.providers[1].shell_interpret);
        assert!(cfg.providers[0].shell_interpret);
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
    fn word_break_chars_defaults_to_ghostty_set() {
        let cfg = RoostConfig::parse("");
        assert_eq!(cfg.word_break_chars, "_-.+~/:@%");
    }

    #[test]
    fn word_break_chars_accepts_override() {
        let cfg = RoostConfig::parse("word-break-chars = _-");
        assert_eq!(cfg.word_break_chars, "_-");
    }

    #[test]
    fn word_break_chars_empty_value_disables_extras() {
        // Explicit empty value → "Unicode letters/digits only".
        let cfg = RoostConfig::parse("word-break-chars = ");
        assert_eq!(cfg.word_break_chars, "");
    }

    #[test]
    fn word_break_chars_mixed_with_other_keys() {
        let cfg = RoostConfig::parse(
            r#"
            copy-on-select = off
            word-break-chars = _-
            theme = catppuccin-mocha
            "#,
        );
        assert_eq!(cfg.word_break_chars, "_-");
        assert_eq!(cfg.copy_on_select, CopyOnSelect::Off);
        assert_eq!(cfg.theme_name.as_deref(), Some("catppuccin-mocha"));
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

    // ----- set_key round-trip ---------------------------------------

    #[test]
    fn set_key_replaces_existing_value_in_place() {
        let before = "theme = catppuccin-mocha\nfont-size = 14\n";
        let after = render_set_key(before, "theme", "roost-dark");
        // `theme` line updated; `font-size` untouched; trailing newline
        // preserved (no double newline).
        assert_eq!(after, "theme = roost-dark\nfont-size = 14\n");
    }

    #[test]
    fn set_key_appends_when_missing() {
        let before = "theme = roost-dark\n";
        let after = render_set_key(before, "font-family", "\"JetBrains Mono\"");
        assert_eq!(
            after,
            "theme = roost-dark\nfont-family = \"JetBrains Mono\"\n"
        );
    }

    #[test]
    fn set_key_appends_to_empty_file() {
        let after = render_set_key("", "theme", "roost-dark");
        assert_eq!(after, "theme = roost-dark\n");
    }

    #[test]
    fn set_key_appends_when_no_trailing_newline() {
        // The file ended without a newline (e.g. user hand-edited).
        // The new line still lands on its own row.
        let before = "theme = roost-dark";
        let after = render_set_key(before, "font-size", "14");
        assert_eq!(after, "theme = roost-dark\nfont-size = 14\n");
    }

    #[test]
    fn set_key_replaces_all_duplicates() {
        // The parser is "last-wins" on duplicates, so replacing only
        // the first occurrence would let a stale later line clobber
        // the new value. Every occurrence must be rewritten.
        let before = "theme = a\ntheme = b\nfont-size = 14\ntheme = c\n";
        let after = render_set_key(before, "theme", "roost-dark");
        assert_eq!(
            after,
            "theme = roost-dark\ntheme = roost-dark\nfont-size = 14\ntheme = roost-dark\n"
        );
    }

    #[test]
    fn set_key_preserves_comments_and_other_keys() {
        let before = "# my roost config\n\ntheme = old\n# inline note\nfont-size = 14\n";
        let after = render_set_key(before, "theme", "new");
        assert_eq!(
            after,
            "# my roost config\n\ntheme = new\n# inline note\nfont-size = 14\n"
        );
    }

    #[test]
    fn set_key_ignores_commented_lines() {
        // A `# theme = …` line shouldn't be treated as the canonical
        // setting; we should append rather than uncomment the user's
        // disabled entry.
        let before = "# theme = disabled\nfont-size = 14\n";
        let after = render_set_key(before, "theme", "roost-dark");
        assert_eq!(
            after,
            "# theme = disabled\nfont-size = 14\ntheme = roost-dark\n"
        );
    }

    #[test]
    fn set_key_handles_value_with_spaces_via_caller_quoting() {
        // `set_key` writes `value` verbatim; quoting (when the value
        // contains spaces) is the caller's responsibility. The parser
        // already strips matching surrounding quotes on read, so a
        // round-trip with `font-family = "JetBrains Mono"` re-parses
        // cleanly.
        let before = "";
        let after = render_set_key(before, "font-family", "\"JetBrains Mono\"");
        let cfg = RoostConfig::parse(&after);
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));
    }

    #[test]
    fn set_key_disk_round_trip_creates_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/dir/config.conf");
        super::set_key(&path, "theme", "roost-dark").unwrap();
        super::set_key(&path, "font-family", "\"JetBrains Mono\"").unwrap();
        super::set_key(&path, "font-size", "15").unwrap();
        let cfg = RoostConfig::load_from(path);
        assert_eq!(cfg.theme_name.as_deref(), Some("roost-dark"));
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.font_size, Some(15.0));
    }

    #[test]
    fn set_key_preserves_leading_whitespace_in_unrelated_lines() {
        // Indented unrelated lines should round-trip exactly (we only
        // rewrite the matched key's line).
        let before = "    # indented note\n  font-size = 14\ntheme = old\n";
        let after = render_set_key(before, "theme", "new");
        assert_eq!(
            after,
            "    # indented note\n  font-size = 14\ntheme = new\n"
        );
    }

    #[test]
    fn set_key_preserves_indent_on_matched_line() {
        // A hand-formatted `  theme = old` should keep its indent
        // after a rewrite — only the key=value text changes.
        let before = "  theme = old\nfont-size = 14\n";
        let after = render_set_key(before, "theme", "new");
        assert_eq!(after, "  theme = new\nfont-size = 14\n");
    }
}
