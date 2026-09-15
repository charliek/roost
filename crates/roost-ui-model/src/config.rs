//! Toolkit-neutral `~/.config/roost/config.conf` parser.
//!
//! Mirrors `mac/Sources/Roost/Config.swift`'s parse shape and value
//! normalization: same lenient-line parsing (blank lines and
//! `#`-comments dropped), same forward-compat (unknown keys silently
//! ignored), same raw-vs-unquoted split per key. The recognized-key
//! sets are not identical — `link-modifier` is Rust-only, `tab-min-width`
//! / `tab-max-width` are Mac-only.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use roost_ipc::LocalBackendMode;

use crate::custom_command::{self, CustomCommand};
use crate::keybind::{self, AccelMods};
use crate::provider::{self, Provider};
use crate::word_selection::DEFAULT_EXTRA_WORD_CHARS;

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
    /// Consumed by both Rust UIs; the Swift UI's modifier is fixed to Cmd.
    pub link_modifier: Option<AccelMods>,

    /// `show-sidebar-agents` — whether the sidebar renders one row
    /// per agent-owned tab under its project (plan 007). Defaults to
    /// `true`. Toggled at runtime via the `toggle_sidebar_agents`
    /// keybind / palette row / Mac View menu item, which writes the
    /// live value straight back here through `set_key`.
    pub show_sidebar_agents: bool,

    /// `agent-hooks` — which coding agents Roost is allowed to wire its
    /// hook entries into, or whether the user has been asked at all
    /// (plan 064). Defaults to [`AgentHooks::Ask`]: an unconfigured
    /// client writes nothing into another product's config file before
    /// the user has confirmed which agents it may touch.
    pub agent_hooks: AgentHooks,

    /// `local-backend` — where the UI's own tabs run (plan 063 §D1).
    /// Defaults to [`LocalBackend::InProcess`], which an unparseable
    /// value also resolves to.
    pub local_backend: LocalBackend,

    /// Whether a `local-backend` line was present at all — parseable or
    /// not. The fresh-install default keys on the key never having been
    /// written, so a typo must not read as "never configured" and
    /// silently move a user's tabs to a session.
    pub local_backend_key_present: bool,
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
            show_sidebar_agents: true,
            agent_hooks: AgentHooks::default(),
            local_backend: LocalBackend::default(),
            local_backend_key_present: false,
        }
    }
}

/// Two-state `local-backend` policy (plan 063 §D1) — where the UI's own
/// tabs run.
///
/// * `InProcess` (default) — PTYs in the UI process, as they have
///   always been (DL-4). They die with the app and no other client can
///   attach to them.
/// * `Session` — the local tabs live in a `roost-session` daemon on
///   this machine, so they survive quit and any same-UID client can
///   subscribe, attach and type.
///
/// [`roost_ipc::LocalBackendMode`] is the same two states in a crate
/// `roost-engine` can see; the `From` below is the only conversion.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LocalBackend {
    #[default]
    InProcess,
    Session,
}

impl LocalBackend {
    /// Parse a config value. Unlike its switch-shaped neighbours this
    /// takes only the two documented spellings — there is no
    /// boolean-ish reading of "in-process" that a user would expect to
    /// work. Any other value returns `None` so the caller can warn and
    /// keep the default.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "in-process" => Some(Self::InProcess),
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}

impl From<LocalBackend> for LocalBackendMode {
    fn from(value: LocalBackend) -> Self {
        match value {
            LocalBackend::InProcess => Self::InProcess,
            LocalBackend::Session => Self::Session,
        }
    }
}

/// The agents Roost knows how to wire, canonical spelling, in the order
/// [`AgentHooks::to_config_value`] serialises them and the order the
/// consent dialog (plan 064) lists its rows. This is
/// `roost_agent_install::ALL_AGENTS` order — this crate cannot depend on
/// `roost-agent-install` (it would pull the install engine into every
/// consumer of `roost-ui-model`, including the CLI's config-only paths),
/// so the two tables are kept in step by
/// `crates/roost-cli/src/agent_install.rs`'s
/// `the_config_name_table_matches_the_agent_inventory` parity test —
/// that crate is the one place both are already linked.
pub const AGENT_NAMES: [&str; 5] = ["claude", "codex", "grok", "cursor", "opencode"];

/// Reserved words that are never agent names: the two documented
/// spellings (`off`/`false`/`no`) plus the retired ones this key used to
/// accept (`auto`/`on`/`true`/`yes`, plan 046). A reserved word is only
/// legal as the *entire* single-token value — see [`AgentHooks::parse`].
const RESERVED_WORDS: [&str; 7] = ["off", "false", "no", "auto", "on", "true", "yes"];

/// Three-state `agent-hooks` policy (plan 064; supersedes plan 046's
/// two-state `Auto`/`Off`).
///
/// * `Allow(names)` — the agents the user has explicitly consented to,
///   canonical names in [`AGENT_NAMES`] order. Every present one not
///   listed is left alone; nothing is ever removed just for being
///   absent from the list.
/// * `Off` — the UIs wire nothing at startup. It does not *remove*
///   anything on its own; an explicit `roostctl agent ensure` reads the
///   same key and takes Roost's entries back out, which is what the
///   startup toast points at alongside `agent uninstall --all`.
/// * `Ask` (default) — the key is absent, empty, or unparseable: nobody
///   has answered the consent dialog yet, so nothing is written into
///   another product's config file and nothing already wired is
///   touched. This is the state a fresh install starts in.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AgentHooks {
    Allow(Vec<String>),
    Off,
    #[default]
    Ask,
}

impl AgentHooks {
    /// Parse a config value.
    ///
    /// `None` means "could not be resolved at all" — the caller warns
    /// and falls back to [`AgentHooks::Ask`], same shape as every other
    /// switch-like key in this parser. An **empty** value is not in that
    /// group: it is `Some(Ask)`, so the caller does not warn — a fresh
    /// install with the key never written must not look like a mistake.
    ///
    /// The value is split on `,` and lowercased; each token that names
    /// an agent in [`AGENT_NAMES`] is kept (order-independent, reordered
    /// into `AGENT_NAMES` order on the way out, duplicates collapsed).
    /// `off`/`false`/`no` as the *entire* single-token value means
    /// `Off`. A reserved word ([`RESERVED_WORDS`]) anywhere else in the
    /// value — alone as one of the retired spellings, or beside a name —
    /// makes the whole value unparseable: `auto, claude` is exactly as
    /// ambiguous as `auto` alone, since the user's two possible
    /// intentions (the old default, or an allow-list that happens to
    /// start with a stray word) cannot be told apart.
    ///
    /// A value with at least one recognised name and some unrecognised
    /// ones (`claude, banana`) still resolves — to the recognised subset
    /// — but warns once from here, since the caller's `None` path never
    /// runs for it.
    pub fn parse(s: &str) -> Option<AgentHooks> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Some(AgentHooks::Ask);
        }
        let lower = trimmed.to_ascii_lowercase();
        let mut tokens: Vec<String> = Vec::new();
        for tok in lower.split(',') {
            let tok = tok.trim().to_string();
            if !tok.is_empty() && !tokens.contains(&tok) {
                tokens.push(tok);
            }
        }
        if tokens.is_empty() {
            return None;
        }
        // Deliberately *after* the de-duplication above: `off, off` is
        // somebody saying `off` twice, not an ambiguous mixed value, and
        // it resolves to `Off`. What the reserved-word rule below rejects
        // is a reserved word standing beside something that is not it.
        if tokens.len() == 1 && matches!(tokens[0].as_str(), "off" | "false" | "no") {
            return Some(AgentHooks::Off);
        }
        if tokens.iter().any(|t| RESERVED_WORDS.contains(&t.as_str())) {
            return None;
        }
        let mut known: Vec<String> = Vec::new();
        let mut unrecognised: Vec<String> = Vec::new();
        for tok in &tokens {
            if AGENT_NAMES.contains(&tok.as_str()) {
                known.push(tok.clone());
            } else {
                unrecognised.push(tok.clone());
            }
        }
        if known.is_empty() {
            return None;
        }
        if !unrecognised.is_empty() {
            tracing::warn!(
                value = trimmed,
                unrecognised = unrecognised.join(", "),
                "agent-hooks: unrecognised agent name(s); allowing the rest"
            );
        }
        let ordered: Vec<String> = AGENT_NAMES
            .iter()
            .copied()
            .filter(|name| known.iter().any(|k| k.as_str() == *name))
            .map(|name| name.to_string())
            .collect();
        Some(AgentHooks::Allow(ordered))
    }

    /// The `config.conf` value for this state, in [`AGENT_NAMES`] order.
    /// `Ask` has none: it is the *absence* of a decision, not a value —
    /// it is never written until the user answers the consent dialog.
    pub fn to_config_value(&self) -> Option<String> {
        match self {
            AgentHooks::Allow(names) => {
                // Re-ordered defensively rather than joined as-is: a
                // caller that hand-builds a `Vec` (a test, or a future
                // dialog that lets the user reorder rows) must still
                // serialise in canonical order.
                let ordered: Vec<&str> = AGENT_NAMES
                    .iter()
                    .copied()
                    .filter(|name| names.iter().any(|n| n.as_str() == *name))
                    .collect();
                Some(ordered.join(", "))
            }
            AgentHooks::Off => Some("off".to_string()),
            AgentHooks::Ask => None,
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

/// Parse a plain boolean config value (`true | yes` / `false | no`).
/// Mirrors `ClipboardWrite::parse` / `CopyOnSelect::parse`'s
/// `Option`-returning shape so every boolean key in the parse loop
/// below shares one `if let Some(v) = ... else { warn }` pattern.
fn parse_bool_like(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}

/// Strip one matched quote pair. Mirrors `unquote` in
/// `mac/Sources/Roost/Config.swift`: no recursion (`""x""` → `"x"`), and
/// no re-trim afterward, so `word-break-chars = " -"` keeps its leading
/// space.
fn unquote(s: &str) -> &str {
    let t = s.trim();
    let mut chars = t.chars();
    match (chars.next(), chars.next_back()) {
        (Some(q @ ('"' | '\'')), Some(last)) if last == q => chars.as_str(),
        _ => t,
    }
}

impl RoostConfig {
    pub fn load_default() -> Self {
        let Some(path) = default_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        let mut cfg = match fs::read_to_string(path) {
            Ok(content) => Self::parse(&content),
            Err(_) => Self::default(),
        };
        // Discovered providers append after any `provider =` config
        // entries, so config order wins and the dir fills in the rest.
        cfg.providers
            .extend(discover_providers(&providers_dir(path)));
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
            // `raw_value` keeps quotes intact for the quote-aware
            // `command` / `provider` tokenizers and for `keybind`, whose
            // trigger is taken verbatim; every scalar key uses `value`.
            let raw_value = value.trim();
            let value = unquote(raw_value);
            match key {
                "theme" => cfg.theme_name = Some(value.to_string()),
                "font-family" => cfg.font_family = Some(value.to_string()),
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
                    // now looks like `<trigger> = <action>`. An empty
                    // trigger or action is malformed — dropped, like
                    // every other malformed line (mirrors Config.swift).
                    if let Some((trigger, action)) = raw_value.split_once('=') {
                        let (trigger, action) = (trigger.trim(), action.trim());
                        if !trigger.is_empty() && !action.is_empty() {
                            cfg.keybinds.push((trigger.to_string(), action.to_string()));
                        }
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
                "show-sidebar-agents" => {
                    if let Some(v) = parse_bool_like(value) {
                        cfg.show_sidebar_agents = v;
                    } else {
                        tracing::warn!(
                            value,
                            "unknown show-sidebar-agents value; falling back to default `true`"
                        );
                    }
                }
                "agent-hooks" => {
                    // Empty (`Some(Ask)`) is silent — a fresh install
                    // that never wrote the key is not a mistake. An
                    // unparseable value (`None`) is: it resolves to the
                    // same `Ask`, but warns, because it must never read
                    // as a quiet `off` or a quiet consent nobody gave.
                    // Last-wins like every other scalar here, so a later
                    // line that fails to parse returns to `Ask` rather
                    // than leaving an earlier line's decision in force.
                    cfg.agent_hooks = match AgentHooks::parse(value) {
                        Some(v) => v,
                        None => {
                            tracing::warn!(
                                value,
                                "unrecognised agent-hooks value; leaving agent hooks \
                                 unconfigured (ask)"
                            );
                            AgentHooks::Ask
                        }
                    };
                }
                "local-backend" => {
                    // Presence is recorded even when the value is
                    // rubbish: it is what tells the launch ladder
                    // "configured, badly" from "never configured", and
                    // only the latter may pick a default of its own.
                    cfg.local_backend_key_present = true;
                    cfg.local_backend = match LocalBackend::parse(value) {
                        Some(v) => v,
                        None => {
                            tracing::warn!(
                                value,
                                "unknown local-backend value; expected in-process|session, \
                                 falling back to default `in-process`"
                            );
                            LocalBackend::default()
                        }
                    };
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
                    if let Some(c) = custom_command::parse_command_line(raw_value) {
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
                    if let Some(p) = provider::parse_provider_line(raw_value) {
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
/// `font-family`, `font-size`, `show-sidebar-agents`).
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

/// Write a config file that holds only `key = value`, failing if one
/// already exists.
///
/// For the write a caller knows is a **create**, not an update — plan
/// 063 §D5's fresh-install `local-backend` line is the one such caller.
/// [`set_key`] cannot serve it: read-modify-write plus an atomic rename
/// prevents a *torn* file, not a *lost update*, and nothing locks this
/// path (`default_path()` has no profile component, so the per-profile
/// instance locks do not cover it). Another profile, instance or editor
/// creating `config.conf` between the caller's "no config" check and
/// the rename would have its file replaced wholesale — including an
/// explicit `local-backend = in-process` it had just written.
/// `create_new` (`O_CREAT|O_EXCL`) makes that race an `AlreadyExists`
/// the caller degrades on instead.
pub fn create_with_key(path: &Path, key: &str, value: &str) -> io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    // No tmp-file + rename: `create_new` is itself the atomic step, and
    // a rename would be exactly the clobber this exists to avoid.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(render_set_key("", key, value).as_bytes())?;
    file.sync_all()
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

    // Renaming onto the *link* would replace it with a regular file and
    // silently orphan the target — someone whose `config.conf` is a link
    // into a dotfiles repo would find Roost had stopped writing the file
    // their repo tracks. Since plan 064 that write can happen with nobody
    // at the keyboard (a connecting client raises this machine's
    // `agent-hooks`), so the link has to survive it.
    let path = follow_links(path);
    let path = path.as_path();
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

/// `path` with its own symlink chain followed, lexically.
///
/// Lexical, not [`fs::canonicalize`], for the reason
/// `roost_agent_install::write::follow_links` gives at length: a link
/// whose target does not exist yet is exactly the case `canonicalize`
/// cannot answer, and calling that "absent" is how the link gets
/// replaced. A cycle stops at the hop limit and the caller writes
/// through whatever it reached, which is no worse than not following at
/// all.
fn follow_links(path: &Path) -> PathBuf {
    const MAX_HOPS: usize = 16;
    let mut current = path.to_path_buf();
    for _ in 0..MAX_HOPS {
        let Ok(link) = fs::read_link(&current) else {
            break;
        };
        current = match current.parent() {
            Some(dir) if link.is_relative() => dir.join(link),
            _ => link,
        };
    }
    current
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
fn discover_providers(dir: &Path) -> Vec<Provider> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
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

/// Where `config.conf` lives: `$ROOST_CONFIG` when it is set and
/// non-empty, else `<home>/.config/roost/config.conf`.
///
/// **The one place that rule lives.** `roost_agent_install::Home`
/// resolves the same path against the root it was handed — a tempdir
/// under test, the real `$HOME` in production — and a second spelling of
/// the rule would eventually send the install engine's `agent-hooks`
/// write into a different file than the one the UI reads back.
///
/// `ROOST_CONFIG` overrides with an absolute file — used by the E2E
/// harness to drive the command launcher off a seeded config (mirrors
/// `ROOST_SOCKET` / `ROOST_BUNDLE_PROFILE`).
pub fn config_path_in(home: &Path, roost_config: Option<&OsStr>) -> PathBuf {
    match roost_config.filter(|raw| !raw.is_empty()) {
        Some(raw) => PathBuf::from(raw),
        None => home.join(".config/roost/config.conf"),
    }
}

fn default_path() -> Option<PathBuf> {
    let over = std::env::var_os("ROOST_CONFIG").filter(|raw| !raw.is_empty());
    // Only the fallback half of the rule needs `$HOME`, so a machine
    // without one still answers when the override is set.
    let home = match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home),
        None if over.is_some() => PathBuf::new(),
        None => return None,
    };
    Some(config_path_in(&home, over.as_deref()))
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

        let cfg = RoostConfig::load_from(&cfgpath);
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
    fn show_sidebar_agents_defaults_to_true() {
        let cfg = RoostConfig::parse("");
        assert!(cfg.show_sidebar_agents);
    }

    #[test]
    fn show_sidebar_agents_accepts_true_and_false() {
        assert!(RoostConfig::parse("show-sidebar-agents = true").show_sidebar_agents);
        assert!(RoostConfig::parse("show-sidebar-agents = yes").show_sidebar_agents);
        assert!(!RoostConfig::parse("show-sidebar-agents = false").show_sidebar_agents);
        assert!(!RoostConfig::parse("show-sidebar-agents = no").show_sidebar_agents);
    }

    // Quoted and CRLF forms must agree with the Swift mirror.
    #[test]
    fn show_sidebar_agents_accepts_quoted_and_crlf_values() {
        assert!(!RoostConfig::parse("show-sidebar-agents = \"false\"").show_sidebar_agents);
        assert!(!RoostConfig::parse("show-sidebar-agents = 'false'").show_sidebar_agents);
        assert!(!RoostConfig::parse("show-sidebar-agents = false\r\n").show_sidebar_agents);
        assert!(!RoostConfig::parse("show-sidebar-agents = \"false\"\r\n").show_sidebar_agents);
    }

    #[test]
    fn show_sidebar_agents_unknown_value_keeps_default() {
        let cfg = RoostConfig::parse("show-sidebar-agents = pancakes");
        assert!(cfg.show_sidebar_agents);
    }

    // ----- agent-hooks (plan 064 §3.1; supersedes plan 046 §3.6) -----
    // Mirrored 1:1 by `mac/Sources/Roost/Config.swift`'s
    // `ConfigAgentHooksTests`.

    fn allow(names: &[&str]) -> AgentHooks {
        AgentHooks::Allow(names.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn agent_hooks_defaults_to_ask() {
        let cfg = RoostConfig::parse("");
        assert_eq!(cfg.agent_hooks, AgentHooks::Ask);
    }

    #[test]
    fn agent_hooks_accepts_a_name_list_normalised_into_agent_names_order() {
        assert_eq!(
            RoostConfig::parse("agent-hooks = claude, codex").agent_hooks,
            allow(&["claude", "codex"])
        );
        // Mixed case, extra whitespace, and codex-first input all
        // normalise the same way: `AGENT_NAMES` order, not source order.
        assert_eq!(
            RoostConfig::parse(" agent-hooks = Codex , CLAUDE ").agent_hooks,
            allow(&["claude", "codex"])
        );
        assert_eq!(
            RoostConfig::parse("agent-hooks = \"claude,codex\"").agent_hooks,
            allow(&["claude", "codex"])
        );
    }

    #[test]
    fn agent_hooks_accepts_the_off_spellings() {
        for line in [
            "agent-hooks = off",
            "agent-hooks = false",
            "agent-hooks = no",
            "agent-hooks = OFF",
        ] {
            assert_eq!(
                RoostConfig::parse(line).agent_hooks,
                AgentHooks::Off,
                "{line}"
            );
        }
    }

    /// Absent or empty resolves to `Ask` — silently. A fresh install
    /// that never wrote the key is the common path, not a mistake, and
    /// must never warn.
    #[test]
    fn agent_hooks_absent_or_empty_is_ask_with_no_warning() {
        for body in ["", "agent-hooks =", "agent-hooks = \"\""] {
            assert_eq!(
                RoostConfig::parse(body).agent_hooks,
                AgentHooks::Ask,
                "{body:?}"
            );
        }
    }

    /// The retired switch spellings (plan 046's `auto`/`on`/`true`/
    /// `yes`) and plain garbage both resolve to `Ask`, and both warn —
    /// unlike the empty case above, these are values somebody actually
    /// wrote, so silence would hide a typo.
    #[test]
    fn agent_hooks_unrecognised_values_are_ask() {
        for body in ["auto", "on", "true", "yes", "banana"] {
            assert_eq!(
                RoostConfig::parse(&format!("agent-hooks = {body}")).agent_hooks,
                AgentHooks::Ask,
                "{body}"
            );
        }
    }

    /// A reserved word beside anything else — a name, another reserved
    /// word — makes the whole value ambiguous rather than "off with an
    /// extra": `off, claude` could mean either "turn off" or "allow
    /// claude", and guessing either way silently would be the second
    /// wrong guess this key has already made once (plan 046's `auto`).
    #[test]
    fn agent_hooks_a_reserved_word_mixed_with_anything_else_is_ask() {
        for body in ["off, claude", "auto, claude", "no, codex"] {
            assert_eq!(
                RoostConfig::parse(&format!("agent-hooks = {body}")).agent_hooks,
                AgentHooks::Ask,
                "{body}"
            );
        }
    }

    /// An unrecognised name beside a recognised one is not ambiguous the
    /// same way — the recognised name is the answer, and the unknown one
    /// is dropped with a warning naming it.
    #[test]
    fn agent_hooks_drops_unrecognised_names_and_keeps_the_rest() {
        assert_eq!(
            RoostConfig::parse("agent-hooks = claude, banana").agent_hooks,
            allow(&["claude"])
        );
    }

    #[test]
    fn agent_hooks_collapses_duplicates() {
        assert_eq!(
            RoostConfig::parse("agent-hooks = claude, claude").agent_hooks,
            allow(&["claude"])
        );
    }

    /// The key is last-wins like every other scalar here, including when
    /// the later line fails to parse: it returns to `Ask`, not to
    /// whatever an earlier line set. Mirrored in
    /// `ConfigAgentHooksTests.repeatedKeyReturnsToAsk`.
    #[test]
    fn agent_hooks_is_last_wins() {
        assert_eq!(
            RoostConfig::parse("agent-hooks = claude\nagent-hooks = off").agent_hooks,
            AgentHooks::Off
        );
        assert_eq!(
            RoostConfig::parse("agent-hooks = claude\nagent-hooks = banana").agent_hooks,
            AgentHooks::Ask,
            "the later unparseable line left the earlier `claude` in force"
        );
    }

    #[test]
    fn agent_hooks_to_config_value_round_trips() {
        assert_eq!(
            allow(&["cursor", "claude"]).to_config_value().as_deref(),
            Some("claude, cursor")
        );
        assert_eq!(AgentHooks::Off.to_config_value().as_deref(), Some("off"));
        assert_eq!(AgentHooks::Ask.to_config_value(), None);
    }

    /// The retired `agent-hooks-skip` key now parses as an ignored
    /// unknown key — `RoostConfig` no longer has a field for it, and it
    /// must not disturb `agent-hooks` on the same line set.
    #[test]
    fn agent_hooks_skip_is_an_ignored_unknown_key() {
        let cfg = RoostConfig::parse("agent-hooks-skip = codex\nagent-hooks = off");
        assert_eq!(cfg.agent_hooks, AgentHooks::Off);
    }

    // ----- local-backend (plan 063 §D1) ------------------------------

    #[test]
    fn local_backend_defaults_to_in_process_with_the_key_absent() {
        let cfg = RoostConfig::parse("theme = Dracula\n");
        assert_eq!(cfg.local_backend, LocalBackend::InProcess);
        assert!(!cfg.local_backend_key_present);
    }

    #[test]
    fn local_backend_accepts_its_two_documented_spellings() {
        for (body, want) in [
            ("local-backend = in-process", LocalBackend::InProcess),
            ("local-backend = session", LocalBackend::Session),
            ("local-backend = \"session\"", LocalBackend::Session),
            ("local-backend = SESSION\r\n", LocalBackend::Session),
        ] {
            let cfg = RoostConfig::parse(body);
            assert_eq!(cfg.local_backend, want, "{body:?}");
            assert!(cfg.local_backend_key_present, "{body:?}");
        }
    }

    /// A value nobody can parse still counts as *configured*. The
    /// fresh-install default writes the key only when it was never
    /// written, and a typo that read as "never configured" would move a
    /// user's local tabs onto a session behind their back.
    #[test]
    fn local_backend_unknown_value_is_in_process_but_still_counts_as_configured() {
        for body in [
            "local-backend = pancakes",
            "local-backend =",
            "local-backend = in_process",
            "local-backend = session\nlocal-backend = pancakes",
        ] {
            let cfg = RoostConfig::parse(body);
            assert_eq!(cfg.local_backend, LocalBackend::InProcess, "{body:?}");
            assert!(cfg.local_backend_key_present, "{body:?}");
        }
    }

    // ----- unquote semantic (mirrors Config.swift's `unquote`) -------

    #[test]
    fn theme_accepts_both_quote_kinds() {
        assert_eq!(
            RoostConfig::parse("theme = \"Dracula\"")
                .theme_name
                .as_deref(),
            Some("Dracula")
        );
        assert_eq!(
            RoostConfig::parse("theme = 'Dracula'")
                .theme_name
                .as_deref(),
            Some("Dracula")
        );
    }

    #[test]
    fn unquote_strips_exactly_one_pair() {
        // No recursion: the inner pair survives.
        assert_eq!(
            RoostConfig::parse("theme = \"\"x\"\"")
                .theme_name
                .as_deref(),
            Some("\"x\"")
        );
    }

    #[test]
    fn unquote_keeps_mismatched_pair_verbatim() {
        // Deliberately narrower than Swift's old set-trim: a mismatched
        // pair is not a quoted value, so both platforms now fail the
        // theme lookup identically.
        assert_eq!(
            RoostConfig::parse("theme = \"dark'").theme_name.as_deref(),
            Some("\"dark'")
        );
        assert_eq!(
            RoostConfig::parse("theme = \"dark").theme_name.as_deref(),
            Some("\"dark")
        );
    }

    #[test]
    fn unquote_matches_scalar_level_quotes() {
        // A combining mark right after the opening quote must not stop
        // the strip: both platforms compare unicode scalars, so the
        // quote is matched even when a grapheme-cluster view would
        // fuse it with the following mark.
        assert_eq!(
            RoostConfig::parse("theme = \"\u{0301}x\"")
                .theme_name
                .as_deref(),
            Some("\u{0301}x")
        );
    }

    #[test]
    fn bool_value_with_interior_cr_before_closing_quote_parses() {
        // `"false\r"` unquotes to `false\r`; the bool parser's trim
        // must strip that CR on both platforms.
        assert!(!RoostConfig::parse("show-sidebar-agents = \"false\r\"").show_sidebar_agents);
    }

    #[test]
    fn unquote_does_not_retrim_interior_padding() {
        assert_eq!(
            RoostConfig::parse("theme = \" Dracula \"")
                .theme_name
                .as_deref(),
            Some(" Dracula ")
        );
    }

    #[test]
    fn font_family_accepts_single_quotes() {
        assert_eq!(
            RoostConfig::parse("font-family = 'JetBrains Mono'")
                .font_family
                .as_deref(),
            Some("JetBrains Mono")
        );
    }

    #[test]
    fn font_size_accepts_quoted_value() {
        assert_eq!(
            RoostConfig::parse("font-size = \"14\"").font_size,
            Some(14.0)
        );
    }

    #[test]
    fn keybind_uses_the_raw_value() {
        // Pinned to the raw value on both platforms: a quoted trigger
        // stays verbatim rather than becoming a different binding.
        let cfg = RoostConfig::parse("keybind = \"ctrl+t\" = new_tab");
        assert_eq!(
            cfg.keybinds,
            vec![("\"ctrl+t\"".to_string(), "new_tab".to_string())]
        );
    }

    #[test]
    fn crlf_file_parses_every_key() {
        let cfg = RoostConfig::parse("theme = Dracula\r\nfont-size = 14\r\n");
        assert_eq!(cfg.theme_name.as_deref(), Some("Dracula"));
        assert_eq!(cfg.font_size, Some(14.0));
    }

    #[test]
    fn link_modifier_accepts_quoted_value() {
        assert_eq!(
            RoostConfig::parse("link-modifier = \"ctrl\"\n").link_modifier,
            Some(AccelMods::CTRL)
        );
    }

    #[test]
    fn unterminated_final_crlf_line_parses() {
        // `.lines()` leaves the bare `\r` on an unterminated final line;
        // the value trim has to finish the job.
        assert_eq!(
            RoostConfig::parse("font-size = 14\r\ntheme = Dracula\r")
                .theme_name
                .as_deref(),
            Some("Dracula")
        );
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
    fn word_break_chars_quoted_value_keeps_interior_space() {
        // Quoting is how a user expresses a leading space; unquoting
        // must not re-trim it away.
        let cfg = RoostConfig::parse("word-break-chars = \" -\"");
        assert_eq!(cfg.word_break_chars, " -");
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
        let cfg = RoostConfig::load_from(&path);
        assert_eq!(cfg.theme_name.as_deref(), Some("roost-dark"));
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.font_size, Some(15.0));
    }

    #[test]
    fn create_with_key_writes_the_one_line_and_makes_its_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/dir/config.conf");
        super::create_with_key(&path, "local-backend", "session").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "local-backend = session\n"
        );
    }

    /// The fresh-install write is a create, and a config that appeared
    /// since the "no config" check must survive it byte for byte — the
    /// lost update `set_key`'s rename would have caused.
    #[test]
    fn create_with_key_refuses_a_config_that_appeared_meanwhile() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.conf");
        let existing =
            "# written by the other instance\nlocal-backend = in-process\ntheme = roost-dark\n";
        fs::write(&path, existing).unwrap();

        let err = super::create_with_key(&path, "local-backend", "session")
            .expect_err("a create over an existing config must fail");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            existing,
            "the config on disk was modified by a write that reported failure"
        );
        // And nothing was left behind beside it (no orphan tmp file).
        let mut names: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["config.conf".to_string()]);
    }

    /// The contrast that gives the test above its teeth: `set_key`,
    /// which the fresh-install path used to call, happily replaces that
    /// same file. Pins why the create-only path exists rather than
    /// re-using the general writer.
    #[test]
    fn set_key_by_contrast_overwrites_what_appeared_meanwhile() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.conf");
        fs::write(&path, "local-backend = in-process\n").unwrap();
        super::set_key(&path, "local-backend", "session").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "local-backend = session\n"
        );
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
