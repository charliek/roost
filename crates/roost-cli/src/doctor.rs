//! `roostctl doctor` — read-only diagnosis of the Roost integration.
//!
//! Three scopes, never mixed (plan 003 §3.2): **process** facts describe
//! the shell that invoked doctor, **ui** facts describe the Roost
//! instance it reached, **tab** facts describe the selected tab. A
//! process fact may only judge a tab fact when the selected tab *is*
//! doctor's own tab.
//!
//! The split is [`collect`] (all the I/O, never returns `Err`) →
//! [`evaluate`] (all the judgement, no I/O) → the renderers. That
//! inverts CLAUDE.md's "errors are returned, not swallowed" inside
//! `collect` on purpose: a diagnostic that aborts on the first problem
//! cannot diagnose. The inversion stops at `collect`'s signature —
//! every failure lands in an [`Inputs`] field and is judged like any
//! other fact.
//!
//! Doctor reports and links; it never repairs, installs, or mutates.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use roost_agent::claude::{canonical_hook_event, CLAUDE_HOOK_EVENTS};
use roost_ipc::agent::{
    effective_lifecycle, is_live, suppress_raw_osc, AgentLifecycle, ShellState,
};
use roost_ipc::messages::{ops, IdentifyParams, IdentifyResult, Tab, TabListResult};
use roost_ipc::socket_state::{self, describe_file_type, SocketState};
use roost_ipc::target::{TargetError, TargetOrigin, TargetSelector};
use roost_ipc::{ClientError, IpcClient};

/// `IpcClient` has no read/write timeout of its own, so every leg of the
/// conversation gets one here — "Roost is hung" must render as a failed
/// check, not a hung doctor.
const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(2);
/// A timeout bounds time, not memory. Version banners are one line.
const OUTPUT_CAP: u64 = 8 * 1024;
/// `claude-settings.json` is six hook entries; anything past this is not
/// a settings file, and reading it unbounded is how a symlink to
/// `/dev/zero` turns a diagnostic into an OOM.
const SETTINGS_READ_CAP: u64 = 1024 * 1024;

const SCHEMA_VERSION: u32 = 2;

// ============================================================================
// Doc links (plan §3.11)
// ============================================================================

macro_rules! docs_base {
    () => {
        "https://charliek.github.io/roost/"
    };
}

/// One published-docs target. `url` is built from `page` + `anchor` by
/// [`doc`], so the link doctor prints and the anchor the test verifies
/// cannot drift apart.
/// `page` + `anchor` are read only by `doc_anchors_resolve`, which
/// resolves them against `docs/` and `mkdocs.yml`'s nav; `url` is what
/// production emits. Carrying all three on one row built by [`doc`] is
/// what makes the printed link and the verified anchor impossible to
/// drift apart — hence the struct-level allow.
#[derive(Debug, Clone, Copy)]
struct Doc {
    #[cfg_attr(not(test), allow(dead_code))]
    page: &'static str,
    #[cfg_attr(not(test), allow(dead_code))]
    anchor: &'static str,
    url: &'static str,
}

macro_rules! doc {
    ($page:literal, $anchor:literal) => {
        Doc {
            page: $page,
            anchor: $anchor,
            url: concat!(docs_base!(), $page, "/#", $anchor),
        }
    };
}

#[cfg(test)]
const DOCS_BASE: &str = docs_base!();

const EXIT_CODES_DOC: Doc = doc!("reference/cli", "exit-codes");

/// Check id → where to read more. Only `fail` and `warn` carry the link;
/// [`check`] attaches it, so a scored check with no row here is a red
/// test rather than a dead end for the user.
const DOC_TARGETS: &[(&str, Doc)] = &[
    ("env.tab_id", doc!("reference/cli", "environment")),
    ("env.socket", doc!("reference/cli", "environment")),
    ("ui.target", doc!("reference/cli", "environment")),
    ("ui.socket", doc!("reference/cli", "environment")),
    ("ui.identify", doc!("reference/cli", "identify")),
    ("ui.version", doc!("guides/claude-code", "verifying")),
    ("ui.agent_model", doc!("guides/claude-code", "verifying")),
    ("shell.login", doc!("guides/cwd-tracking", "how-it-loads")),
    (
        "shell.integration",
        doc!("guides/cwd-tracking", "how-it-loads"),
    ),
    (
        "shell.resources",
        doc!("guides/cwd-tracking", "how-it-loads"),
    ),
    (
        "shell.marks_observed",
        doc!("guides/cwd-tracking", "how-it-loads"),
    ),
    (
        "shell.marks_feature",
        doc!("guides/cwd-tracking", "feature-flags"),
    ),
    (
        "shell.marks_capability",
        doc!(
            "guides/cwd-tracking",
            "switching-macos-default-to-homebrew-bash"
        ),
    ),
    ("tab.selection", doc!("reference/cli", "environment")),
    (
        "tab.raw_osc",
        doc!("guides/notifications", "hook-session-osc-suppression"),
    ),
    (
        "claude.binary",
        doc!("guides/claude-code", "troubleshooting"),
    ),
    ("claude.settings", doc!("guides/claude-code", "install")),
    ("claude.hook_events", doc!("guides/claude-code", "install")),
    ("claude.hook_command", doc!("guides/claude-code", "install")),
    (
        "claude.observed",
        doc!("guides/claude-code", "troubleshooting"),
    ),
];

fn docs_for(check_id: &str) -> Option<&'static str> {
    DOC_TARGETS
        .iter()
        .find(|(id, _)| *id == check_id)
        .map(|(_, d)| d.url)
}

// ============================================================================
// Report
// ============================================================================

/// A verdict. `Skipped` is the *absence* of one — the subject was
/// absent, or the answer could not be determined — which is why an
/// observation may carry it while it can never carry `Ok`/`Warn`/`Fail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skipped,
}

impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skipped => "skipped",
        }
    }
}

/// The other axis (§3.6): does this entry assert something about the
/// user's setup, or is it a fact with no correct value? `tab.ownership`
/// reporting "none" is not worse than reporting "claude", so it observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Check,
    Observation,
}

impl Kind {
    fn as_str(&self) -> &'static str {
        match self {
            Kind::Check => "check",
            Kind::Observation => "observation",
        }
    }
}

/// Hand-written rather than `rename_all = "snake_case"` so the text and
/// JSON renderers cannot spell a status differently — the same
/// one-vocabulary rule §3.9 applies to redaction.
impl Serialize for Status {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl Serialize for Kind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// One report entry. Build it through [`check`], [`observation`] or
/// [`unavailable`] — never by literal — so §3.6's invariant (a check
/// always carries a verdict; an observation carries `None` or
/// `Some(Skipped)`) cannot be violated by accident.
///
/// `status` is `Option` with no `skip_serializing_if`: a `--json`
/// consumer must be able to read `"status": null` as "this is a fact",
/// and a dropped key would be indistinguishable from an older schema.
///
/// The fields are private for that reason: `mod tests` is a descendant
/// so it still reads them, but no sibling module can spell a `Check`
/// literal that the constructors would have refused.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    id: &'static str,
    title: &'static str,
    kind: Kind,
    status: Option<Status>,
    detail: String,
    docs_url: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Section {
    pub id: &'static str,
    pub title: &'static str,
    /// Which of the three scopes this section's checks describe.
    pub scope: &'static str,
    /// The section's rolled-up verdict (§3.8). `None` is an
    /// observational section with nothing adverse in it — a section that
    /// grades nothing, rendered `•` rather than a green tick.
    pub status: Option<Status>,
    /// The one line the default view prints for this section — always a
    /// copy of some entry's `detail` with its blank runs collapsed
    /// (see [`collapse_blanks`]), never new prose (§3.9).
    pub headline: String,
    pub checks: Vec<Check>,
}

/// One column per state an entry can be in, so
/// `ok + warn + fail + skipped + facts` is the whole inventory (§3.13).
/// `facts` counts entries carrying no status at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
    pub skipped: usize,
    pub facts: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub roostctl_version: String,
    pub sections: Vec<Section>,
}

impl Report {
    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for check in self.checks() {
            match check.status {
                Some(Status::Ok) => s.ok += 1,
                Some(Status::Warn) => s.warn += 1,
                Some(Status::Fail) => s.fail += 1,
                Some(Status::Skipped) => s.skipped += 1,
                None => s.facts += 1,
            }
        }
        s
    }

    fn checks(&self) -> impl Iterator<Item = &Check> {
        self.sections.iter().flat_map(|s| &s.checks)
    }

    /// The entries a user can act on, stated next to [`exit_code`] so
    /// "actionable" and "exit 1" cannot drift apart. `skipped` and the
    /// observations are excluded: neither asks anyone to do anything.
    ///
    /// [`exit_code`]: Report::exit_code
    fn issues(&self) -> impl Iterator<Item = &Check> {
        self.checks()
            .filter(|c| matches!(c.status, Some(Status::Fail | Status::Warn)))
    }

    /// 0 unless some check is `fail`. `warn`, `skipped` and the
    /// status-less observations never change it (plan §3.3).
    pub fn exit_code(&self) -> i32 {
        i32::from(self.checks().any(|c| c.status == Some(Status::Fail)))
    }
}

fn check(
    id: &'static str,
    title: &'static str,
    status: Status,
    detail: impl Into<String>,
) -> Check {
    Check {
        id,
        title,
        kind: Kind::Check,
        status: Some(status),
        detail: detail.into(),
        docs_url: match status {
            Status::Fail | Status::Warn => docs_for(id),
            Status::Ok | Status::Skipped => None,
        },
    }
}

/// A fact with no correct value (§3.7). Never carries a `docs_url`:
/// there is nothing here to go read about.
fn observation(id: &'static str, title: &'static str, detail: impl Into<String>) -> Check {
    Check {
        id,
        title,
        kind: Kind::Observation,
        status: None,
        detail: detail.into(),
        docs_url: None,
    }
}

/// How much a status weighs in a section roll-up. `Ok` scores 0 because
/// §3.8 rolls up the *adverse* states only — a section with nothing
/// adverse in it takes its marker from the section's own kind, not from
/// whether some entry happened to pass.
fn severity(status: Status) -> u8 {
    match status {
        Status::Ok => 0,
        Status::Skipped => 1,
        Status::Warn => 2,
        Status::Fail => 3,
    }
}

/// §3.8's ordering, asked once. `None` when nothing in the run is
/// adverse. Both roll-ups — the section's and the footer bullet's — fold
/// through here, so a change to the ordering cannot move one and leave
/// the other behind.
fn worst_adverse(statuses: impl Iterator<Item = Status>) -> Option<Status> {
    statuses
        .filter(|s| severity(*s) > 0)
        .max_by_key(|s| severity(*s))
}

/// Squeeze every run of blank-rendering scalars to a single space and
/// trim the ends.
///
/// This is the vector [`escape_controls`] cannot see: a space is not a
/// control character, so a long run of them inside a headline positions
/// whatever follows at a column of the attacker's choosing — including
/// the exact column a narrow terminal wraps a [`SUMMARY_W`] row, where
/// the wrapped remainder reads as a genuine `[✗] Selected tab` section
/// line. A headline is a one-line summary, so a *run* of blanks in one
/// has no legitimate purpose; collapsing costs nothing and takes the aim
/// away.
///
/// "Blank" is deliberately wider than Unicode `White_Space` (see
/// [`renders_blank`]): what makes a scalar usable as padding is that the
/// cell draws empty, not which category it landed in, and U+2800 BRAILLE
/// PATTERN BLANK (So) and the Hangul fillers (Lo) substitute for a space
/// 1:1 in every terminal font. The collapse cannot collide with the
/// escaping already applied — [`escape_controls`] rewrote a tab as the
/// two characters `\` and `t`, and neither is blank.
///
/// The residual, precisely: this closes the *invisible* padding, not
/// every route to a column. The budget that keeps a summary row to one
/// line counts **scalars, not display columns**
/// (`chars().count() <= SUMMARY_W`), so an East Asian Wide character —
/// visible, and two columns for one scalar — still lets an attacker move
/// the forged text while that check passes. Closing *that* needs real
/// display-width computation, i.e. the `unicode-width` crate; not taken,
/// because a vendored width table needs Unicode-version upkeep forever to
/// harden one diagnostic line, and the remaining attack has to pad with
/// glyphs the reader can see. What is gone is the aim an invisible run
/// bought.
fn collapse_blanks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for c in s.chars() {
        if renders_blank(c) {
            pending = !out.is_empty();
        } else {
            if pending {
                out.push(' ');
                pending = false;
            }
            out.push(c);
        }
    }
    out
}

/// Scalars that draw as an empty cell, for [`collapse_blanks`]: Unicode
/// `White_Space` — NBSP, U+2000–U+200A and the ideographic space all pad
/// as well as a space does — plus the blank-rendering scalars outside that
/// property. U+2028/U+2029 are in it too, and are line separators
/// `char::is_control` does not cover, so this is their last stop if a
/// detail ever reaches a headline unescaped.
fn renders_blank(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            // BRAILLE PATTERN BLANK — a braille cell with no dots raised.
            '\u{2800}'
            // HANGUL FILLER, CHOSEONG / JUNGSEONG FILLER, HALFWIDTH
            // HANGUL FILLER — placeholder jamo, drawn as nothing.
            | '\u{3164}' | '\u{115f}' | '\u{1160}' | '\u{ffa0}'
            // MONGOLIAN VOWEL SEPARATOR, which is Cf: a *detail* can never
            // carry it this far ([`escape_controls`] gets there first), so
            // this arm is for direct callers only.
            | '\u{180e}'
        )
}

/// Assemble a section, computing §3.8's roll-up and §3.9's headline here
/// — in `evaluate`, which already holds every derived fact — so the two
/// renderers cannot disagree and a test can assert both without
/// rendering.
///
/// `kind` is a **static property of the section**, not a fold over its
/// entries. `tab` is the only observational one: everything in it
/// reports a fact, and `tab.selection` is there to say *which* tab the
/// facts describe rather than to grade the setup, so an `ok` there must
/// not paint a green tick on a section that grades nothing.
///
/// The headline is always a copy of an existing entry's `detail`.
/// Synthesizing a fresh sentence would be a second source of truth for
/// facts the details already carry, and a new unredacted surface — a
/// `$SHELL` carrying a newline could forge a whole section line. Every
/// detail has already been through [`redact`]/[`escape_controls`], so
/// the headline inherits that for free — plus
/// [`collapse_blanks`], which the *detail* deliberately does not get:
/// `-v` and `--json` carry the value as it actually is, while the
/// one-line summary cannot afford printable padding.
fn section(
    id: &'static str,
    title: &'static str,
    scope: &'static str,
    kind: Kind,
    headline_id: &'static str,
    checks: Vec<Check>,
) -> Section {
    let worst = worst_adverse(checks.iter().filter_map(|c| c.status));
    let status = match (worst, kind) {
        (Some(s), _) => Some(s),
        (None, Kind::Check) => Some(Status::Ok),
        (None, Kind::Observation) => None,
    };
    // Adverse states name the worst entry, ties broken by position;
    // anything else names the section's pinned headline entry, which
    // `check_ids_and_titles_are_unique_and_stable` guarantees is present.
    let source = match status {
        Some(Status::Fail | Status::Warn | Status::Skipped) => {
            checks.iter().find(|c| c.status == status)
        }
        Some(Status::Ok) | None => checks.iter().find(|c| c.id == headline_id),
    };
    Section {
        id,
        title,
        scope,
        status,
        headline: source.map_or_else(String::new, |c| collapse_blanks(&c.detail)),
        checks,
    }
}

/// An observation whose subject could not be observed. Machine-readable
/// on purpose: `tab.ownership: null` ("nothing owns it") and
/// `tab.ownership: "skipped"` ("the UI predates the agent model") are
/// different findings, and a `--json` consumer must not have to regex
/// the prose to tell them apart.
fn unavailable(id: &'static str, title: &'static str, reason: impl Into<String>) -> Check {
    Check {
        status: Some(Status::Skipped),
        ..observation(id, title, reason)
    }
}

// ============================================================================
// Inputs — every field an already-resolved fact
// ============================================================================

/// Outcome of one bounded read-only subprocess (plan §3.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubprocessOutcome {
    Output(String),
    /// Not attempted. Carries no reason: the only thing that skips is
    /// `$SHELL --version` when `$SHELL` fails its guard, and
    /// `shell.login` already reports that from `shell_path` /
    /// `shell_usable` — a second copy here was written four times and
    /// read by nothing.
    Skipped,
    /// The binary isn't on PATH / doesn't exist.
    Missing,
    TimedOut,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketOutcome {
    Connected,
    Missing,
    NotASocket(String),
    /// The file outlived its listener.
    Stale,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketProbe {
    pub path: PathBuf,
    /// Set when doctor is reporting per profile because target
    /// resolution found nothing live (or too much).
    pub profile: Option<&'static str>,
    pub outcome: SocketOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetFailure {
    NoLiveTarget(Vec<PathBuf>),
    Ambiguous(Vec<String>),
    UnknownProfile(String),
    Path(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentifyFailure {
    /// The socket never opened, so `identify` was never sent.
    NoConnection(String),
    Timeout,
    Io(String),
    Protocol(String),
    Server(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsProbe {
    /// `$HOME` is unset or not UTF-8, so there is no location to look at.
    LocationUnknown,
    Absent,
    Unreadable(String),
    Unparseable(String),
    Parsed,
}

/// One `command` hook entry out of `claude-settings.json`, with the
/// filesystem questions about its argv[0] already answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    pub event: String,
    pub raw: String,
    /// `None` when the command's quoting doesn't parse.
    pub argv: Option<Vec<String>>,
    pub exe_executable: bool,
    pub exe_canonical: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Inputs {
    pub roostctl_version: String,
    pub roostctl_exe: Option<PathBuf>,
    pub now_unix: i64,

    pub env_tab_id: Option<String>,
    pub env_socket: Option<String>,
    pub env_shell_integration: Option<String>,
    pub env_shell_features: Option<String>,
    pub env_resources_dir: Option<String>,
    pub explicit_tab: Option<i64>,

    pub shell_path: Option<String>,
    pub shell_usable: bool,
    pub shell_version: SubprocessOutcome,
    pub parent_pid: u32,
    pub parent_comm: SubprocessOutcome,
    pub resources_script: Option<PathBuf>,
    pub resources_script_readable: bool,

    pub target_origin: TargetOrigin,
    pub target_candidates: Vec<PathBuf>,
    pub target: Result<PathBuf, TargetFailure>,
    pub sockets: Vec<SocketProbe>,

    pub identify: Result<IdentifyResult, IdentifyFailure>,
    /// The `tab.list` result as the server sent it. Old-server detection
    /// (plan §3.5) needs the raw keys; everything else re-decodes this
    /// into [`TabListResult`] inside [`evaluate`].
    pub tab_list: Result<serde_json::Value, String>,

    pub claude_version: SubprocessOutcome,
    /// `None` when `$HOME` gave doctor nowhere to look (F8).
    pub claude_settings_path: Option<PathBuf>,
    pub claude_settings: SettingsProbe,
    pub claude_hook_events: Vec<String>,
    pub claude_hook_commands: Vec<HookCommand>,
}

impl Default for Inputs {
    fn default() -> Self {
        Inputs {
            roostctl_version: env!("CARGO_PKG_VERSION").to_string(),
            roostctl_exe: None,
            now_unix: 0,
            env_tab_id: None,
            env_socket: None,
            env_shell_integration: None,
            env_shell_features: None,
            env_resources_dir: None,
            explicit_tab: None,
            shell_path: None,
            shell_usable: false,
            shell_version: SubprocessOutcome::Skipped,
            parent_pid: 0,
            parent_comm: SubprocessOutcome::Skipped,
            resources_script: None,
            resources_script_readable: false,
            target_origin: TargetOrigin::AutoDetect,
            target_candidates: Vec::new(),
            target: Err(TargetFailure::NoLiveTarget(Vec::new())),
            sockets: Vec::new(),
            identify: Err(IdentifyFailure::NoConnection("no socket".into())),
            tab_list: Err("no connection".into()),
            claude_version: SubprocessOutcome::Missing,
            claude_settings_path: None,
            claude_settings: SettingsProbe::LocationUnknown,
            claude_hook_events: Vec::new(),
            claude_hook_commands: Vec::new(),
        }
    }
}

// ============================================================================
// collect — ALL the I/O, never returns Err
// ============================================================================

pub async fn collect(selector: &TargetSelector, explicit_tab: Option<i64>) -> Inputs {
    let shell_path = non_empty_env("SHELL");
    let shell_usable = shell_path.as_deref().is_some_and(executable_regular_file);
    let parent_pid = std::os::unix::process::parent_id();

    // Two independent I/O phases, concurrently: the three bounded
    // subprocesses (§3.8) and the target → socket → IPC chain, which is
    // ordered internally but depends on none of them. Doctor is reached
    // for precisely when something is hung, so the wall clock must be
    // the slower of the two, not their sum.
    let (shell_version, parent_comm, claude_version, ui) = tokio::join!(
        shell_version(shell_path.as_deref(), shell_usable),
        parent_comm(parent_pid),
        capture_version("claude"),
        probe_ui(selector),
    );

    let claude_settings_path = crate::claude_settings_path().ok();
    let (claude_settings, claude_hook_events, claude_hook_commands) =
        read_claude_settings(claude_settings_path.as_deref());

    let env_resources_dir = non_empty_env("ROOST_RESOURCES_DIR");
    let resources_script = shipped_script_path(
        env_resources_dir.as_deref(),
        shell_family(shell_path.as_deref()),
    );
    // Regular-file first: `ROOST_RESOURCES_DIR` is environment-supplied,
    // and opening a FIFO for reading blocks until someone writes to it.
    let resources_script_readable = resources_script
        .as_deref()
        .is_some_and(|p| p.is_file() && std::fs::File::open(p).is_ok());

    Inputs {
        roostctl_version: env!("CARGO_PKG_VERSION").to_string(),
        roostctl_exe: crate::self_exe(),
        now_unix: unix_now(),
        env_tab_id: non_empty_env("ROOST_TAB_ID"),
        env_socket: non_empty_env("ROOST_SOCKET"),
        env_shell_integration: non_empty_env("ROOST_SHELL_INTEGRATION"),
        env_shell_features: non_empty_env("ROOST_SHELL_FEATURES"),
        env_resources_dir,
        explicit_tab,
        resources_script,
        resources_script_readable,
        shell_path,
        shell_usable,
        shell_version,
        parent_pid,
        parent_comm,
        target_origin: ui.origin,
        target_candidates: ui.candidates,
        target: ui.target,
        sockets: ui.sockets,
        identify: ui.identify,
        tab_list: ui.tab_list,
        claude_version,
        claude_settings_path,
        claude_settings,
        claude_hook_events,
        claude_hook_commands,
    }
}

const NO_SOCKET: &str = "target resolution found no socket to dial";

/// Everything `collect` learns by talking to a UI. Its steps are
/// genuinely ordered — you cannot dial a path you have not resolved —
/// so it is one future that runs *beside* the subprocesses.
struct UiProbe {
    origin: TargetOrigin,
    candidates: Vec<PathBuf>,
    target: Result<PathBuf, TargetFailure>,
    sockets: Vec<SocketProbe>,
    identify: Result<IdentifyResult, IdentifyFailure>,
    tab_list: Result<serde_json::Value, String>,
}

async fn probe_ui(selector: &TargetSelector) -> UiProbe {
    let diagnosis = selector.diagnose().await;
    let target = match diagnosis.resolved {
        Ok(t) => Ok(t.socket_path),
        Err(TargetError::Ambiguous { live }) => Err(TargetFailure::Ambiguous(
            live.0.iter().map(|kind| kind.as_str().to_owned()).collect(),
        )),
        Err(TargetError::NoLiveTarget { tried }) => Err(TargetFailure::NoLiveTarget(tried)),
        Err(TargetError::UnknownProfile(v)) => Err(TargetFailure::UnknownProfile(v)),
        Err(TargetError::Path(e)) => Err(TargetFailure::Path(e.to_string())),
    };

    // `resolve(probe_alive=false)` is a trap here: with nothing set on
    // macOS it returns the Mac path unconditionally, so with only the
    // GTK UI running doctor would report "socket missing" against a
    // path nobody uses. Classify the resolved path when there is one,
    // otherwise every candidate, per profile.
    let sockets = match &target {
        Ok(path) => vec![SocketProbe {
            path: path.clone(),
            profile: None,
            outcome: classify_socket(path).await,
        }],
        Err(TargetFailure::NoLiveTarget(_)) | Err(TargetFailure::Ambiguous(_)) => {
            let mut probes = Vec::with_capacity(diagnosis.candidates.len());
            for (kind, path) in &diagnosis.candidates {
                probes.push(SocketProbe {
                    path: path.clone(),
                    profile: Some(kind.as_str()),
                    outcome: classify_socket(path).await,
                });
            }
            probes
        }
        Err(_) => Vec::new(),
    };

    let (identify, tab_list) = match &target {
        Ok(path) => dial(path).await,
        Err(_) => (
            Err(IdentifyFailure::NoConnection(NO_SOCKET.into())),
            Err(NO_SOCKET.to_string()),
        ),
    };

    UiProbe {
        origin: diagnosis.origin,
        candidates: diagnosis.candidates.into_iter().map(|(_, p)| p).collect(),
        target,
        sockets,
        identify,
        tab_list,
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `$SHELL` is an environment-supplied path doctor is about to execute.
/// Absolute, existing, executable regular file — and spawned directly,
/// never PATH-resolved and never handed to a shell (plan §3.8).
fn executable_regular_file(path: &str) -> bool {
    let p = Path::new(path);
    p.is_absolute() && is_executable_regular_file(p)
}

fn is_executable_regular_file(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Resolve a command word the way a shell would: a word containing a
/// slash is used as a path, anything else is searched on `PATH`.
///
/// Doctor never runs these — it only reports whether Claude's hook
/// command *could* run. A bare `roostctl claude-hook SessionStart` works
/// whenever `roostctl` is on Claude's PATH (the Linux `.deb` installs
/// `/usr/bin/roostctl`), so rejecting non-absolute words outright would
/// fail a healthy install — exactly the false positive §3.3's
/// applicability rule exists to prevent.
fn resolve_program(word: &str) -> Option<PathBuf> {
    if word.contains('/') {
        let p = Path::new(word);
        return is_executable_regular_file(p).then(|| p.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(word))
        .find(|candidate| is_executable_regular_file(candidate))
}

/// Doctor must be safe to run blind, so it never opens a path it has not
/// first confirmed is a **regular file**: a FIFO where a config file
/// should be blocks `open` forever, and a symlink to `/dev/zero` reads
/// without end. Both become a diagnostic detail instead.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileRead {
    Text(String),
    Absent,
    /// Why doctor has nothing to show — a FIFO, an oversized file, a
    /// permission error. All of them are findings, none is a reason to
    /// stop the report.
    Error(String),
}

fn read_regular_file_capped(path: &Path, cap: u64) -> FileRead {
    use std::io::Read as _;

    // `metadata` follows symlinks on purpose — a symlinked settings file
    // is legitimate; a symlink *to a device* is what must be refused.
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileRead::Absent,
        Err(e) => return FileRead::Error(e.to_string()),
        Ok(meta) if !meta.is_file() => {
            return FileRead::Error(format!(
                "not a regular file ({})",
                describe_file_type(meta.file_type())
            ))
        }
        Ok(_) => {}
    }
    // One byte past the cap, so "exactly at the cap" and "truncated" are
    // distinguishable.
    let mut buf = Vec::new();
    match std::fs::File::open(path).and_then(|f| f.take(cap + 1).read_to_end(&mut buf)) {
        Err(e) => FileRead::Error(e.to_string()),
        Ok(_) if buf.len() as u64 > cap => {
            FileRead::Error(format!("larger than the {cap}-byte read cap"))
        }
        Ok(_) => match String::from_utf8(buf) {
            Ok(text) => FileRead::Text(text),
            Err(_) => FileRead::Error("not valid UTF-8".to_string()),
        },
    }
}

async fn shell_version(shell: Option<&str>, usable: bool) -> SubprocessOutcome {
    match (shell, usable) {
        (None, _) | (Some(_), false) => SubprocessOutcome::Skipped,
        (Some(path), true) => capture(Command::new(path), ["--version"]).await,
    }
}

/// Absolute path on purpose: `ps` is one of three subprocesses doctor
/// spawns, and a PATH-resolved one would let an attacker-controllable
/// `PATH` choose the binary. (`claude` stays PATH-resolved — reporting
/// what the user would actually run is the point — and `$SHELL` is
/// guarded by [`executable_regular_file`].)
async fn parent_comm(ppid: u32) -> SubprocessOutcome {
    capture(
        Command::new("/bin/ps"),
        ["-o", "comm=", "-p", &ppid.to_string()],
    )
    .await
}

async fn capture_version(program: &str) -> SubprocessOutcome {
    capture(Command::new(program), ["--version"]).await
}

/// Run a read-only command under every bound plan §3.8 pins: stdin from
/// `/dev/null` (some shells treat an unrecognized `--version` as a
/// script and block reading stdin), a capped read of each pipe, a
/// deadline — and, on that deadline, an explicit kill + reap.
///
/// `kill_on_drop` alone is documented as best-effort, and a diagnostic
/// that leaves a process behind is not read-only in any sense the user
/// cares about. Forked *grand*children still escape; killing the process
/// group is out of scope (plan §9).
async fn capture<I, S>(mut cmd: Command, args: I) -> SubprocessOutcome
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // These are version *parsers*, and every banner they parse is
        // translated: `bash --version` says "Version" under de_DE and
        // "versión" under es_ES, which silently degraded mark capability
        // to undetermined for every non-English-locale user. The C locale
        // is the one the parsers target.
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SubprocessOutcome::Missing,
        Err(e) => return SubprocessOutcome::Failed(e.to_string()),
    };

    match tokio::time::timeout(SUBPROCESS_TIMEOUT, drain_capped(&mut child)).await {
        Ok(Ok(text)) => SubprocessOutcome::Output(text),
        Ok(Err(e)) => SubprocessOutcome::Failed(e.to_string()),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            SubprocessOutcome::TimedOut
        }
    }
}

async fn drain_capped(child: &mut tokio::process::Child) -> std::io::Result<String> {
    let mut out = Vec::new();
    let mut err = Vec::new();
    // Drain both pipes concurrently — reading one to the end while the
    // other fills its buffer is a deadlock.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let read_out = async {
        if let Some(s) = stdout.as_mut() {
            let _ = s.take(OUTPUT_CAP).read_to_end(&mut out).await;
        }
    };
    let read_err = async {
        if let Some(s) = stderr.as_mut() {
            let _ = s.take(OUTPUT_CAP).read_to_end(&mut err).await;
        }
    };
    tokio::join!(read_out, read_err);
    child.wait().await?;
    // Some `--version` implementations answer on stderr.
    let bytes = if out.iter().any(|b| !b.is_ascii_whitespace()) {
        out
    } else {
        err
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Doctor's view of `socket_state::probe`. The classification itself —
/// including the rule that only `ECONNREFUSED` and an absent path mean
/// stale — lives in `roost_ipc::socket_state`, shared with the UI's
/// bind path so the two can't drift.
async fn classify_socket(path: &Path) -> SocketOutcome {
    match socket_state::probe(path, IPC_TIMEOUT).await {
        SocketState::Missing => SocketOutcome::Missing,
        SocketState::NotASocket(kind) => SocketOutcome::NotASocket(kind.to_string()),
        SocketState::Live => SocketOutcome::Connected,
        SocketState::Stale => SocketOutcome::Stale,
        SocketState::Indeterminate(why) => SocketOutcome::Error(why),
    }
}

type Dialed = (
    Result<IdentifyResult, IdentifyFailure>,
    Result<serde_json::Value, String>,
);

/// The only two ops doctor sends, both read-only, each under its own
/// deadline.
async fn dial(path: &Path) -> Dialed {
    let mut client = match tokio::time::timeout(IPC_TIMEOUT, IpcClient::connect(path)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            let msg = e.to_string();
            return (
                Err(IdentifyFailure::NoConnection(msg.clone())),
                Err(format!("no connection: {msg}")),
            );
        }
        Err(_) => {
            return (
                Err(IdentifyFailure::Timeout),
                Err("connect timed out".to_string()),
            )
        }
    };

    let identify = match tokio::time::timeout(
        IPC_TIMEOUT,
        client.identify(IdentifyParams {
            client_name: crate::CLIENT_NAME.into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
        }),
    )
    .await
    {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(classify_client_error(e)),
        Err(_) => Err(IdentifyFailure::Timeout),
    };

    // A timed-out `identify` was *cancelled*, not answered: the UI may
    // still write that response, and `IpcClient` matches responses by
    // monotonic id — so `tab.list` on this connection would read the
    // stale identify frame and report `IdMismatch`, blaming the tab
    // section for a failure that never happened. Dial again rather than
    // inherit a desynchronized stream.
    if matches!(identify, Err(IdentifyFailure::Timeout)) {
        client = match tokio::time::timeout(IPC_TIMEOUT, IpcClient::connect(path)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return (identify, Err(format!("no connection: {e}"))),
            Err(_) => return (identify, Err("connect timed out".to_string())),
        };
    }

    let tab_list = match tokio::time::timeout(
        IPC_TIMEOUT,
        client.call::<_, serde_json::Value>(ops::TAB_LIST, serde_json::json!({})),
    )
    .await
    {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("tab.list timed out".to_string()),
    };

    (identify, tab_list)
}

/// `ClientError` keeps `Protocol` (decode) apart from `Io` (transport)
/// deliberately — schema drift is not a dead wire. Preserve that here.
fn classify_client_error(e: ClientError) -> IdentifyFailure {
    match e {
        ClientError::Protocol(inner) => IdentifyFailure::Protocol(inner.to_string()),
        ClientError::Io(inner) => IdentifyFailure::Io(inner.to_string()),
        ClientError::Server { code, message } => {
            IdentifyFailure::Server(format!("{code} — {message}"))
        }
        other => IdentifyFailure::Io(other.to_string()),
    }
}

fn read_claude_settings(path: Option<&Path>) -> (SettingsProbe, Vec<String>, Vec<HookCommand>) {
    // An unavailable `$HOME` means the location is unknown, never a
    // CWD-relative path: doctor must not diagnose whatever
    // `.config/roost/claude-settings.json` happens to sit under the
    // directory it was run from.
    let Some(path) = path else {
        return (SettingsProbe::LocationUnknown, Vec::new(), Vec::new());
    };
    let raw = match read_regular_file_capped(path, SETTINGS_READ_CAP) {
        FileRead::Text(s) => s,
        FileRead::Absent => return (SettingsProbe::Absent, Vec::new(), Vec::new()),
        FileRead::Error(what) => return (SettingsProbe::Unreadable(what), Vec::new(), Vec::new()),
    };
    let doc: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return (
                SettingsProbe::Unparseable(e.to_string()),
                Vec::new(),
                Vec::new(),
            )
        }
    };
    let Some(hooks) = doc.get("hooks").and_then(|h| h.as_object()) else {
        return (
            SettingsProbe::Unparseable("no `hooks` object".into()),
            Vec::new(),
            Vec::new(),
        );
    };

    let events: Vec<String> = hooks.keys().cloned().collect();
    let mut commands = Vec::new();
    for (event, groups) in hooks {
        for group in groups.as_array().into_iter().flatten() {
            for entry in group
                .get("hooks")
                .and_then(|h| h.as_array())
                .into_iter()
                .flatten()
            {
                let Some(raw) = entry.get("command").and_then(|c| c.as_str()) else {
                    continue;
                };
                commands.push(resolve_hook_command(event, raw));
            }
        }
    }
    (SettingsProbe::Parsed, events, commands)
}

fn resolve_hook_command(event: &str, raw: &str) -> HookCommand {
    let argv = shell_split(raw);
    let resolved = argv
        .as_ref()
        .and_then(|a| a.first())
        .and_then(|word| resolve_program(word));
    HookCommand {
        event: event.to_string(),
        raw: raw.to_string(),
        argv,
        exe_executable: resolved.is_some(),
        // Canonicalized on both sides so a relocated `Roost.app` (or a
        // symlinked install) is not a false positive.
        exe_canonical: resolved.and_then(|p| std::fs::canonicalize(p).ok()),
    }
}

// ============================================================================
// Pure helpers shared by collect + evaluate
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellFamily {
    Zsh,
    Bash,
    Fish,
    Other,
    Unknown,
}

fn shell_family(shell_path: Option<&str>) -> ShellFamily {
    let Some(path) = shell_path else {
        return ShellFamily::Unknown;
    };
    match Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
    {
        "zsh" => ShellFamily::Zsh,
        "bash" => ShellFamily::Bash,
        "fish" => ShellFamily::Fish,
        "" => ShellFamily::Unknown,
        _ => ShellFamily::Other,
    }
}

/// The shipped integration script for this family, if one ships. No
/// `roost.fish` exists, which is why fish is `skipped` and not `fail`.
fn shipped_script_path(resources_dir: Option<&str>, family: ShellFamily) -> Option<PathBuf> {
    let leaf = match family {
        ShellFamily::Zsh => "roost.zsh",
        ShellFamily::Bash => "roost.bash",
        _ => return None,
    };
    Some(
        PathBuf::from(resources_dir?)
            .join("shell-integration")
            .join(leaf),
    )
}

/// `GNU bash, version 5.3.9(1)-release …` → `(5, 3)`.
///
/// Case-insensitive as a second line of defence behind [`capture`]'s
/// `LC_ALL=C`: German bash capitalizes the word.
fn bash_version(banner: &str) -> Option<(u32, u32)> {
    const KEY: &str = "version ";
    // ASCII-only lowercasing, so byte offsets stay valid in `banner`.
    let at = banner.to_ascii_lowercase().find(KEY)? + KEY.len();
    let rest = &banner[at..];
    let mut parts = rest.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default().trim()
}

/// POSIX-ish word split — enough to read back what `claude_install`
/// wrote, including its `'…'` quoting of paths with spaces. `None` when
/// the quoting doesn't close.
fn shell_split(input: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return None,
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => cur.push(chars.next()?),
                        Some(c) => cur.push(c),
                        None => return None,
                    }
                }
            }
            '\\' => {
                started = true;
                cur.push(chars.next()?);
            }
            c => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        out.push(cur);
    }
    Some(out)
}

// ============================================================================
// Redaction (plan §3.9) — shape-based, because the data is agent-supplied
// ============================================================================

const MAX_DISPLAY_CHARS: usize = 120;
const MAX_KEY_CHARS: usize = 40;
/// Metadata keys whose values are safe to print. Everything else prints
/// its length only — `session_title` is derived from the user's own
/// prompt, and any adapter can invent new keys.
const METADATA_VALUE_ALLOWLIST: [&str; 4] =
    ["model", "source", "background_tasks", "session_crons"];

/// Escape control characters so an agent-supplied string cannot inject
/// fake report lines or ANSI sequences into output a user pastes into an
/// issue.
///
/// Two categories beyond Cc, neither of which `char::is_control` sees,
/// both admitted by the same reading of plan 003 §3.9 — the threat model
/// is output a user *pastes* into an issue, so the renderers there count,
/// not just the terminal:
///
/// * **Zl/Zp** (U+2028/U+2029). Terminals don't break on them; plenty of
///   paste targets do, which is the forged-report-line vector this
///   escaping exists to close.
/// * **Cf** (format, see [`is_format`]). One step further and a policy
///   fix rather than a patched instance: a raw U+202E RIGHT-TO-LEFT
///   OVERRIDE reverses the rest of the line, and GitHub and every browser
///   honor it, so `$SHELL=<RLO>bat detceleS ]✗[` pastes as a convincing
///   `[✗] Selected tab`. U+2066–U+2069 do the same by isolate, and
///   U+200B–U+200F hide text outright. A format character has no
///   legitimate place in a diagnostic string, so the whole category goes —
///   which closes the class for *every* detail rather than only for the
///   headlines that also get [`collapse_blanks`].
fn escape_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') || is_format(c) => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// `General_Category=Cf`, as ranges. `char` exposes `is_control` (Cc) and
/// nothing else, and a Unicode-table crate is a large dependency to carry
/// for one diagnostic string, so the category is spelled out here:
/// Unicode 16.0's Cf, coalesced from a full scalar-by-scalar walk of
/// `UnicodeData.txt`.
///
/// Two ranges are widened past the real category to keep the table short.
/// U+2065 and U+E0002–U+E001F are unassigned holes inside a format block;
/// over-broad is the safe direction here, since escaping an unassigned
/// scalar costs a diagnostic nothing while a missed range costs it the
/// vector in [`escape_controls`].
const FORMAT_RANGES: [(u32, u32); 19] = [
    (0x00ad, 0x00ad),           // SOFT HYPHEN
    (0x0600, 0x0605),           // ARABIC NUMBER SIGN … ARABIC NUMBER MARK ABOVE
    (0x061c, 0x061c),           // ARABIC LETTER MARK
    (0x06dd, 0x06dd),           // ARABIC END OF AYAH
    (0x070f, 0x070f),           // SYRIAC ABBREVIATION MARK
    (0x0890, 0x0891),           // ARABIC POUND / PIASTRE MARK ABOVE
    (0x08e2, 0x08e2),           // ARABIC DISPUTED END OF AYAH
    (0x180e, 0x180e),           // MONGOLIAN VOWEL SEPARATOR
    (0x200b, 0x200f),           // ZWSP, ZWNJ, ZWJ, LRM, RLM
    (0x202a, 0x202e),           // LRE, RLE, PDF, LRO, RLO — the bidi overrides
    (0x2060, 0x206f),           // WORD JOINER … the deprecated format controls
    (0xfeff, 0xfeff),           // ZERO WIDTH NO-BREAK SPACE (BOM)
    (0xfff9, 0xfffb),           // INTERLINEAR ANNOTATION ANCHOR/SEPARATOR/TERMINATOR
    (0x0001_10bd, 0x0001_10bd), // KAITHI NUMBER SIGN
    (0x0001_10cd, 0x0001_10cd), // KAITHI NUMBER SIGN ABOVE
    (0x0001_3430, 0x0001_343f), // EGYPTIAN HIEROGLYPH format controls
    (0x0001_bca0, 0x0001_bca3), // SHORTHAND FORMAT letter overlap … up step
    (0x0001_d173, 0x0001_d17a), // MUSICAL SYMBOL BEGIN BEAM … END PHRASE
    (0x000e_0001, 0x000e_007f), // LANGUAGE TAG + the TAG characters
];

fn is_format(c: char) -> bool {
    let cp = c as u32;
    FORMAT_RANGES.iter().any(|(lo, hi)| cp >= *lo && cp <= *hi)
}

/// Escape, then cap for display — counted in characters, with the true
/// length shown so a truncation is never mistaken for the whole value.
fn redact(raw: &str) -> String {
    redact_to(raw, MAX_DISPLAY_CHARS)
}

/// Paths are external strings too — `$ROOST_SOCKET`, `--socket`, `$HOME`
/// and the hook commands all reach the renderer this way, and a newline
/// in one of them would otherwise forge a whole report line (§3.9).
fn redact_path(path: &Path) -> String {
    redact(&path.to_string_lossy())
}

fn redact_to(raw: &str, cap: usize) -> String {
    let escaped = escape_controls(raw);
    match cut_to_chars(&escaped, cap) {
        Some(head) => format!("{head}…({} chars)", raw.chars().count()),
        None => escaped,
    }
}

/// The one place a display string is cut, shared by the redaction cap
/// above and the summary view's column budget. Counted in
/// **characters** — a byte cut would split the `—` and `→` that details
/// routinely carry mid-scalar. `None` when `s` already fits, so a
/// caller's "this was cut" marker is added only when something was
/// actually dropped.
fn cut_to_chars(s: &str, cap: usize) -> Option<String> {
    (s.chars().count() > cap).then(|| s.chars().take(cap).collect())
}

/// `session_id` is an opaque token from an agent; print enough to reason
/// about the `(source, session_id)` matching rule, not enough to be a
/// credential in a paste.
fn fingerprint(session_id: &str) -> String {
    if session_id.is_empty() {
        return "<none>".to_string();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in session_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!(
        "fp:{:012x} ({} chars)",
        hash & 0x0000_ffff_ffff_ffff,
        session_id.chars().count()
    )
}

fn redact_metadata(metadata: &BTreeMap<String, String>) -> String {
    if metadata.is_empty() {
        return "none".to_string();
    }
    metadata
        .iter()
        .map(|(key, value)| {
            let shown = redact_to(key, MAX_KEY_CHARS);
            if METADATA_VALUE_ALLOWLIST.contains(&key.as_str()) {
                format!("{shown}={}", redact(value))
            } else {
                format!("{shown}=<{} chars>", value.chars().count())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ============================================================================
// evaluate — ALL the judgement, no I/O
// ============================================================================

/// How the inspected tab was chosen (plan §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    Flag(i64),
    Env(i64),
    Active(i64),
    None,
}

impl Selection {
    fn id(&self) -> Option<i64> {
        match self {
            Selection::Flag(id) | Selection::Env(id) | Selection::Active(id) => Some(*id),
            Selection::None => None,
        }
    }

    fn source(&self) -> &'static str {
        match self {
            Selection::Flag(_) => "--tab",
            Selection::Env(_) => "$ROOST_TAB_ID",
            Selection::Active(_) => "the UI's active tab",
            Selection::None => "nothing",
        }
    }
}

/// `$ROOST_TAB_ID` as a usable tab id, through the *same* parse the
/// Claude hook uses — doctor reporting `ok` on a value the hook silently
/// drops would be it lying about the exact failure it exists to catch.
fn env_tab_id(inputs: &Inputs) -> Option<i64> {
    inputs.env_tab_id.as_deref().and_then(crate::parse_tab_id)
}

/// Keyed on the two variables Roost injects that nothing else sets:
/// `ROOST_SHELL_INTEGRATION` and `ROOST_RESOURCES_DIR`.
///
/// `ROOST_SOCKET` and `ROOST_TAB_ID` are deliberately NOT part of this,
/// even though Roost injects them too. Both are *documented
/// user-settable targeting variables* (`docs/reference/cli.md`: "Set them
/// by hand only when invoking the CLI from outside a Roost tab (e.g. a CI
/// runner)") — so keying on them would make a CI runner doing exactly
/// that fail the whole process-scoped shell section on a healthy machine,
/// which is the failure §3.3's applicability rule exists to prevent. They
/// are *selection* inputs here, like `--tab`, not proof of a Roost PTY.
fn inside_roost_tab(inputs: &Inputs) -> bool {
    inputs.env_shell_integration.is_some() || inputs.env_resources_dir.is_some()
}

/// Plan §3.3's applicability sentence for the *process*-scoped checks:
/// their subject is absent, so they are skipped rather than failed —
/// still checks, just with nothing to judge. Written once — six checks
/// say it, and they must say it identically.
fn not_in_tab(id: &'static str, title: &'static str) -> Check {
    check(id, title, Status::Skipped, NOT_IN_TAB)
}

const NOT_IN_TAB: &str = "not running inside a Roost tab";

/// Plan §3.2's correlation rule: a *process*-scoped fact may judge a
/// *tab*-scoped one only when the selected tab is the tab doctor is
/// itself running in. Named once because both `shell.marks_observed` and
/// `claude.observed` gate on it, and two copies of the module's central
/// rule is one too many.
fn is_doctors_own_tab(inputs: &Inputs, selection: Selection) -> bool {
    matches!((selection.id(), env_tab_id(inputs)), (Some(a), Some(b)) if a == b)
}

/// Byte-for-byte what the shipped scripts do: `roost.bash:127-132` and
/// `roost.zsh:30-35` match `*",no-$1,"*` against the raw variable, so a
/// token with surrounding whitespace does **not** disable the feature
/// there. Trimming here would report `marks` disabled on a shell still
/// emitting them.
fn marks_opted_out(inputs: &Inputs) -> bool {
    inputs
        .env_shell_features
        .as_deref()
        .is_some_and(|f| f.split(',').any(|part| part == "no-marks"))
}

pub fn evaluate(inputs: &Inputs) -> Report {
    let tab_list = decode_tab_list(&inputs.tab_list);
    let tabs = match &tab_list {
        TabList::Decoded { tabs, .. } => Some(tabs),
        _ => None,
    };

    let selection = select_tab(inputs);
    let selected = selection.id().and_then(|id| {
        tabs.and_then(|t| t.projects.iter().flat_map(|p| &p.tabs).find(|t| t.id == id))
    });
    let model = agent_model(&tab_list, selection);
    let capability = marks_capability(inputs);
    let can_mark = mark_capability(inputs);

    Report {
        schema_version: SCHEMA_VERSION,
        roostctl_version: inputs.roostctl_version.clone(),
        sections: vec![
            section(
                "env",
                "Environment",
                "process",
                Kind::Check,
                "env.tab_id",
                env_checks(inputs),
            ),
            section(
                "ui",
                "Roost UI",
                "ui",
                Kind::Check,
                "ui.identify",
                ui_checks(inputs, &tab_list, model),
            ),
            section(
                "shell",
                "Shell integration",
                "process",
                Kind::Check,
                "shell.marks_observed",
                shell_checks(inputs, &capability, can_mark, selection, selected, model),
            ),
            section(
                "tab",
                "Selected tab",
                "tab",
                Kind::Observation,
                "tab.derived",
                tab_checks(inputs, selection, selected, tabs.is_some(), model),
            ),
            section(
                "claude",
                "Claude Code",
                "process",
                Kind::Check,
                "claude.hook_events",
                claude_checks(inputs, selection, selected, tabs.is_some(), model),
            ),
        ],
    }
}

/// What came back from `tab.list`, decoded once.
///
/// The typed copy is what every check reads; the raw copy is the only
/// thing that can answer "does this server emit the agent axes at all"
/// (§3.5). They are produced together, from one response, so a shape the
/// typed decode rejects can never be laundered into "this UI has no
/// tabs" by the raw walk succeeding on its own.
enum TabList<'a> {
    Decoded {
        tabs: TabListResult,
        raw: Vec<&'a serde_json::Value>,
    },
    /// The server answered, but the answer is not a tab list.
    Malformed(String),
    /// The call never produced a response.
    Failed(&'a str),
}

fn decode_tab_list(response: &Result<serde_json::Value, String>) -> TabList<'_> {
    let value = match response {
        Ok(v) => v,
        Err(e) => return TabList::Failed(e),
    };
    match TabListResult::deserialize(value) {
        Ok(tabs) => TabList::Decoded {
            tabs,
            raw: raw_tab_objects(value),
        },
        Err(e) => TabList::Malformed(e.to_string()),
    }
}

/// Every tab object exactly as the server sent it — the only signal that
/// discriminates a pre-plan-002 server (plan §3.5). A borrowed view: the
/// callers only ever read keys off it. Only ever walked over a value the
/// typed decode already accepted.
fn raw_tab_objects(raw: &serde_json::Value) -> Vec<&serde_json::Value> {
    raw.get("projects")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("tabs").and_then(|t| t.as_array()))
        .flatten()
        .collect()
}

/// Whether this server emits the agent axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentModel {
    Present,
    /// The tab objects carry no agent axes — a pre-plan-002 server.
    Absent,
    /// Nothing to probe: no tabs, or `tab.list` never answered.
    Undetermined,
    /// The response was not a tab list, so nothing about it is knowable.
    Malformed,
}

fn select_tab(inputs: &Inputs) -> Selection {
    if let Some(id) = inputs.explicit_tab {
        return Selection::Flag(id);
    }
    if let Some(id) = env_tab_id(inputs) {
        return Selection::Env(id);
    }
    match inputs.identify.as_ref().map(|i| i.active_tab_id) {
        Ok(id) if id != 0 => Selection::Active(id),
        _ => Selection::None,
    }
}

/// Probe the selected tab's raw object (or any tab, when the selection
/// is not in the list) for **both** agent axes.
///
/// Both, not just `shell_state`: a tab carrying one and not the other
/// would otherwise pass, and the `tab` section would then print the
/// serde-filled default for the missing axis as if the server had said
/// it — the exact fabrication §3.5 exists to prevent.
fn agent_model(list: &TabList, selection: Selection) -> AgentModel {
    let TabList::Decoded { raw: raw_tabs, .. } = list else {
        return match list {
            TabList::Malformed(_) => AgentModel::Malformed,
            _ => AgentModel::Undetermined,
        };
    };
    let probe = selection
        .id()
        .and_then(|id| {
            // `Tab::id` goes over the wire as a string (`string_int64`).
            let want = id.to_string();
            raw_tabs
                .iter()
                .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(want.as_str()))
        })
        .or_else(|| raw_tabs.first());
    match probe {
        None => AgentModel::Undetermined,
        Some(t) if t.get("shell_state").is_some() && t.get("agent_lifecycle").is_some() => {
            AgentModel::Present
        }
        Some(_) => AgentModel::Absent,
    }
}

// ---------------------------------------------------------------- env

fn env_checks(inputs: &Inputs) -> Vec<Check> {
    // The variable is the subject here, not the Roost PTY: a value that
    // is set but unusable is broken wherever it came from — a CI runner
    // exporting `ROOST_TAB_ID=abc` gets the same silent no-op. Only the
    // *unset* case needs §3.3's applicability gate, since outside a tab
    // there is nothing that should have set it.
    let tab_id = match (&inputs.env_tab_id, env_tab_id(inputs)) {
        (_, Some(id)) => check(
            "env.tab_id",
            "ROOST_TAB_ID",
            Status::Ok,
            format!("ROOST_TAB_ID={id}"),
        ),
        (Some(raw), None) => check(
            "env.tab_id",
            "ROOST_TAB_ID",
            Status::Fail,
            format!(
                "ROOST_TAB_ID={} is not a positive integer; every per-tab command \
                 silently no-ops",
                redact(raw)
            ),
        ),
        (None, None) if inside_roost_tab(inputs) => check(
            "env.tab_id",
            "ROOST_TAB_ID",
            Status::Fail,
            "unset inside a Roost tab; every per-tab command silently no-ops",
        ),
        (None, None) => not_in_tab("env.tab_id", "ROOST_TAB_ID"),
    };

    // Set or unset, neither is the correct value: unset is the ordinary
    // shape outside a Roost tab, and set is the documented way to target
    // one from a CI runner. So it observes rather than judges (§3.7).
    //
    // The one status change here that *removes a positive verdict*: the set
    // arm was `ok` before the status/kind split, so this id migrates
    // `ok → null` — not `info → null` like the unset arm, whose `info` had
    // no spelling left to keep. Anyone exporting `ROOST_SOCKET` therefore
    // sees `summary.ok` drop by one, and a `jq` check for `status == "ok"`
    // on `env.socket` now fails where it used to pass.
    let socket = match &inputs.env_socket {
        Some(path) => observation(
            "env.socket",
            "ROOST_SOCKET",
            format!("ROOST_SOCKET={}", redact(path)),
        ),
        None => observation(
            "env.socket",
            "ROOST_SOCKET",
            "unset — target resolution falls through to --socket / --target / auto-detect",
        ),
    };

    vec![tab_id, socket]
}

// ----------------------------------------------------------------- ui

/// `a, b` over a path list — the shape every "which sockets" detail uses.
fn paths(list: &[PathBuf]) -> String {
    list.iter()
        .map(|p| redact_path(p))
        .collect::<Vec<_>>()
        .join(", ")
}

fn ui_checks(inputs: &Inputs, tab_list: &TabList, model: AgentModel) -> Vec<Check> {
    let target = {
        const ID: &str = "ui.target";
        const TITLE: &str = "Target resolution";
        let candidates = paths(&inputs.target_candidates);
        let (status, detail) = match &inputs.target {
            Ok(path) => (
                Status::Ok,
                format!(
                    "{} → {} (auto-detect would try: {candidates})",
                    inputs.target_origin.as_str(),
                    redact_path(path)
                ),
            ),
            Err(TargetFailure::NoLiveTarget(tried)) => (
                Status::Fail,
                format!("no Roost UI is listening (tried: {})", paths(tried)),
            ),
            Err(TargetFailure::Ambiguous(live)) => (
                Status::Fail,
                format!(
                    "multiple Roost UIs are running ({}; sockets: {candidates}); pass \
                     --target mac|gtk|iced or set \
                     ROOST_BUNDLE_PROFILE",
                    live.join(" + ")
                ),
            ),
            Err(TargetFailure::UnknownProfile(v)) => (
                Status::Fail,
                format!(
                    "ROOST_BUNDLE_PROFILE={} is not `mac`, `gtk`, or `iced`",
                    redact(v)
                ),
            ),
            Err(TargetFailure::Path(e)) => (
                Status::Fail,
                format!("path resolution failed: {}", redact(e)),
            ),
        };
        check(ID, TITLE, status, detail)
    };

    let socket = socket_check(inputs);

    let identify = {
        const ID: &str = "ui.identify";
        const TITLE: &str = "UI identity";
        let (status, detail) = match &inputs.identify {
            Ok(id) => (
                Status::Ok,
                format!(
                    "{} ({}) pid={} ui_version={} protocol_version={} active_tab={} socket={}",
                    redact(&id.app_label),
                    redact(&id.app_id),
                    id.pid,
                    redact(&id.ui_version),
                    id.protocol_version,
                    id.active_tab_id,
                    redact(&id.socket_path)
                ),
            ),
            Err(IdentifyFailure::NoConnection(msg)) => {
                (Status::Fail, format!("no connection: {}", redact(msg)))
            }
            Err(IdentifyFailure::Timeout) => (
                Status::Fail,
                "timed out after 2s — the UI accepted the connection but did not answer"
                    .to_string(),
            ),
            Err(IdentifyFailure::Io(msg)) => {
                (Status::Fail, format!("transport failure: {}", redact(msg)))
            }
            Err(IdentifyFailure::Protocol(msg)) => (
                Status::Fail,
                format!(
                    "protocol failure (client/server schema drift, not a dead wire): {}",
                    redact(msg)
                ),
            ),
            Err(IdentifyFailure::Server(msg)) => (
                Status::Fail,
                format!("the UI rejected `identify`: {}", redact(msg)),
            ),
        };
        check(ID, TITLE, status, detail)
    };

    let version = {
        const ID: &str = "ui.version";
        const TITLE: &str = "Version skew";
        let ours = &inputs.roostctl_version;
        let (status, detail) = match &inputs.identify {
            Ok(id) if id.ui_version == *ours => {
                (Status::Ok, format!("roostctl and the UI are both {ours}"))
            }
            Ok(id) => (
                Status::Warn,
                format!(
                    "roostctl {ours} against UI {} — restart Roost after upgrading",
                    redact(&id.ui_version)
                ),
            ),
            Err(_) => (
                Status::Skipped,
                format!("roostctl {ours} — no UI reached, nothing to compare"),
            ),
        };
        check(ID, TITLE, status, detail)
    };

    let agent_model = {
        const ID: &str = "ui.agent_model";
        const TITLE: &str = "Agent state model";
        let (status, detail) = match model {
            AgentModel::Present => (
                Status::Ok,
                "the UI reports the four-axis agent state model".to_string(),
            ),
            AgentModel::Absent => (
                Status::Fail,
                "this UI predates the four-axis agent model (its tab objects carry no \
                 `shell_state` / `agent_lifecycle`); restart Roost to pick up the current build"
                    .to_string(),
            ),
            // A response that does not decode is a protocol failure, not
            // an empty tab list — collapsing the two would report a
            // broken server as a healthy UI with nothing open.
            AgentModel::Malformed => (
                Status::Fail,
                match tab_list {
                    TabList::Malformed(e) => format!(
                        "the UI's tab.list response is not a tab list (client/server schema \
                         drift): {}",
                        redact(e)
                    ),
                    _ => "the UI's tab.list response is not a tab list".to_string(),
                },
            ),
            // Nothing to probe is undecidable either way — not a fault.
            AgentModel::Undetermined => (
                Status::Skipped,
                match tab_list {
                    TabList::Failed(e) => {
                        format!("undetermined — tab.list failed: {}", redact(e))
                    }
                    _ => "undetermined (no tabs to inspect)".to_string(),
                },
            ),
        };
        check(ID, TITLE, status, detail)
    };

    vec![target, socket, identify, version, agent_model]
}

fn socket_check(inputs: &Inputs) -> Check {
    if inputs.sockets.is_empty() {
        return check(
            "ui.socket",
            "Socket",
            Status::Fail,
            "no socket path to inspect — target resolution failed before a path existed",
        );
    }
    let mut worst = Status::Ok;
    let mut lines = Vec::with_capacity(inputs.sockets.len());
    for probe in &inputs.sockets {
        let label = match probe.profile {
            Some(p) => format!("{p} {}", redact_path(&probe.path)),
            None => redact_path(&probe.path),
        };
        let (status, note) = match &probe.outcome {
            SocketOutcome::Connected => (Status::Ok, "connected".to_string()),
            SocketOutcome::Missing => (
                Status::Fail,
                "missing — no Roost UI has bound this path".to_string(),
            ),
            SocketOutcome::NotASocket(kind) => (
                Status::Fail,
                format!("not a socket ({kind}) — something else owns this path"),
            ),
            SocketOutcome::Stale => (
                Status::Fail,
                "stale — the socket file outlived its listener; Roost crashed or was killed"
                    .to_string(),
            ),
            SocketOutcome::Error(e) => (Status::Fail, format!("unreachable: {}", redact(e))),
        };
        if status == Status::Fail {
            worst = Status::Fail;
        }
        lines.push(format!("{label}: {note}"));
    }
    check("ui.socket", "Socket", worst, lines.join("; "))
}

// --------------------------------------------------------------- shell

/// Can this shell emit the OSC 133 **command-start** mark? A fact, not a
/// status: `shell.marks_observed` needs the answer too, and reading it
/// back off `shell.marks_capability`'s `Status` would make one check
/// depend on another check's *presentation* rather than on the thing
/// both are reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkCapability {
    /// Both marks fire — zsh, or bash ≥ 4.4 (where the `C` mark's `PS0`
    /// is available).
    Both,
    /// Only the command-end mark fires, so the running dot never lights.
    EndOnly,
    /// No shipped integration for this family at all.
    None,
    /// Family or version could not be determined.
    Undetermined,
}

fn bash_version_of(inputs: &Inputs) -> Option<(u32, u32)> {
    match &inputs.shell_version {
        SubprocessOutcome::Output(text) => bash_version(first_line(text)),
        _ => None,
    }
}

fn mark_capability(inputs: &Inputs) -> MarkCapability {
    match shell_family(inputs.shell_path.as_deref()) {
        ShellFamily::Zsh => MarkCapability::Both,
        ShellFamily::Bash => match bash_version_of(inputs) {
            Some(v) if v >= (4, 4) => MarkCapability::Both,
            Some(_) => MarkCapability::EndOnly,
            None => MarkCapability::Undetermined,
        },
        ShellFamily::Fish | ShellFamily::Other => MarkCapability::None,
        ShellFamily::Unknown => MarkCapability::Undetermined,
    }
}

fn marks_capability(inputs: &Inputs) -> Check {
    const ID: &str = "shell.marks_capability";
    const TITLE: &str = "Mark capability";
    // Process-scoped, so §3.3's applicability rule governs it: outside a
    // Roost tab there is no integration to be capable of, and warning
    // that fish emits no Roost marks would diagnose an ordinary machine.
    if !inside_roost_tab(inputs) {
        return not_in_tab(ID, TITLE);
    }
    let family = shell_family(inputs.shell_path.as_deref());
    let bash = || {
        let (major, minor) = bash_version_of(inputs).unwrap_or_default();
        format!("bash {major}.{minor}")
    };
    let (status, detail) = match mark_capability(inputs) {
        MarkCapability::Both if family == ShellFamily::Bash => (
            Status::Ok,
            format!("{} supports PS0, so both OSC 133 marks fire", bash()),
        ),
        MarkCapability::Both => (
            Status::Ok,
            "zsh emits both OSC 133 marks via preexec/precmd".to_string(),
        ),
        MarkCapability::EndOnly => (
            Status::Warn,
            format!(
                "{} has no PS0 (needs ≥ 4.4), so only the command-end mark fires; the running \
                 dot will never light",
                bash()
            ),
        ),
        MarkCapability::None => (
            Status::Warn,
            format!(
                "no Roost integration ships for {}, so no OSC 133 marks are emitted",
                inputs
                    .shell_path
                    .as_deref()
                    .map_or_else(|| "this shell".to_string(), redact)
            ),
        ),
        MarkCapability::Undetermined if family == ShellFamily::Bash => (
            Status::Skipped,
            "bash, but its version could not be determined".to_string(),
        ),
        MarkCapability::Undetermined => (
            Status::Skipped,
            "shell family undetermined ($SHELL is not set)".to_string(),
        ),
    };
    check(ID, TITLE, status, detail)
}

/// `shell.current`'s caveat, fitted to the platform doctor is actually
/// running on: only Linux clips `comm` to the kernel's `TASK_COMM_LEN`,
/// so telling a macOS user about a Linux truncation is noise. Both arms
/// keep the "hint, never load-bearing" framing (§3.8).
const COMM_CAVEAT: &str = if cfg!(target_os = "linux") {
    " — best-effort: `ps` truncates comm to 16 characters, so treat it as a hint"
} else {
    " — best-effort, so treat it as a hint"
};

fn shell_checks(
    inputs: &Inputs,
    capability: &Check,
    can_mark: MarkCapability,
    selection: Selection,
    selected: Option<&Tab>,
    model: AgentModel,
) -> Vec<Check> {
    let in_tab = inside_roost_tab(inputs);
    let family = shell_family(inputs.shell_path.as_deref());
    // §3.3, for `shell.login`'s two unusable arms only: an ordinary
    // terminal with no usable `$SHELL` is not a Roost fault, so they
    // judge only inside a Roost tab. The rest of this process-scoped
    // section reaches the same policy through `not_in_tab`.
    let unusable_shell = if in_tab {
        Status::Warn
    } else {
        Status::Skipped
    };

    let login = match (&inputs.shell_path, inputs.shell_usable) {
        (None, _) => check(
            "shell.login",
            "Login shell",
            unusable_shell,
            "$SHELL is not set",
        ),
        (Some(path), false) => check(
            "shell.login",
            "Login shell",
            unusable_shell,
            format!(
                "$SHELL={} is not an absolute path to an executable regular file",
                redact(path)
            ),
        ),
        (Some(path), true) => {
            let version = match &inputs.shell_version {
                SubprocessOutcome::Output(text) if !first_line(text).is_empty() => {
                    redact(first_line(text))
                }
                SubprocessOutcome::TimedOut => "version unknown (--version timed out)".to_string(),
                _ => "version unknown".to_string(),
            };
            // Deliberately not gated on `in_tab`: a usable `$SHELL` is a
            // verdict about the machine, true whether or not doctor is
            // running inside a Roost tab.
            check(
                "shell.login",
                "Login shell",
                Status::Ok,
                format!("{} — {version}", redact(path)),
            )
        }
    };

    let current = match &inputs.parent_comm {
        SubprocessOutcome::Output(text) if !first_line(text).is_empty() => {
            // `comm` is a bare (Linux-truncated at 16 chars) name on
            // Linux and a full path on macOS, so match on the basename.
            let name = first_line(text).trim_start_matches('-');
            let leaf = Path::new(name)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(name);
            let note = if leaf.eq_ignore_ascii_case("roost") || leaf.starts_with("roost-") {
                " (the Roost UI itself, not a shell)"
            } else {
                ""
            };
            observation(
                "shell.current",
                "Current shell",
                format!(
                    "parent pid {} is `{}`{note}{COMM_CAVEAT}",
                    inputs.parent_pid,
                    redact(name)
                ),
            )
        }
        _ => observation(
            "shell.current",
            "Current shell",
            format!("parent pid {} not detected", inputs.parent_pid),
        ),
    };

    let integration = if !in_tab {
        not_in_tab("shell.integration", "Integration contract")
    } else if inputs.env_shell_integration.as_deref() == Some("1") {
        check(
            "shell.integration",
            "Integration contract",
            Status::Ok,
            "ROOST_SHELL_INTEGRATION=1",
        )
    } else {
        check(
            "shell.integration",
            "Integration contract",
            Status::Fail,
            format!(
                "ROOST_SHELL_INTEGRATION={} inside a Roost tab (expected `1`)",
                inputs
                    .env_shell_integration
                    .as_deref()
                    .map_or_else(|| "<unset>".to_string(), redact)
            ),
        )
    };

    let resources = if !in_tab {
        not_in_tab("shell.resources", "Shipped scripts")
    } else if matches!(family, ShellFamily::Fish | ShellFamily::Other) {
        check(
            "shell.resources",
            "Shipped scripts",
            Status::Skipped,
            "not applicable — no integration script ships for this shell",
        )
    } else if inputs.env_resources_dir.is_none() {
        check(
            "shell.resources",
            "Shipped scripts",
            Status::Fail,
            "ROOST_RESOURCES_DIR is unset, so neither auto-bootstrap can load",
        )
    } else {
        match (&inputs.resources_script, inputs.resources_script_readable) {
            (Some(path), true) => check(
                "shell.resources",
                "Shipped scripts",
                Status::Ok,
                format!("{} is readable", redact_path(path)),
            ),
            (Some(path), false) => check(
                "shell.resources",
                "Shipped scripts",
                Status::Fail,
                format!(
                    "{} is missing, unreadable, or not a regular file",
                    redact_path(path)
                ),
            ),
            (None, _) => check(
                "shell.resources",
                "Shipped scripts",
                Status::Skipped,
                "not applicable — shell family undetermined",
            ),
        }
    };

    let opted_out = marks_opted_out(inputs);
    let marks_feature = if !in_tab {
        not_in_tab("shell.marks_feature", "`marks` feature")
    } else if opted_out {
        check(
            "shell.marks_feature",
            "`marks` feature",
            Status::Skipped,
            "disabled by ROOST_SHELL_FEATURES=no-marks — an opt-out, not a fault",
        )
    } else {
        check(
            "shell.marks_feature",
            "`marks` feature",
            Status::Ok,
            format!(
                "enabled (ROOST_SHELL_FEATURES={})",
                inputs
                    .env_shell_features
                    .as_deref()
                    .map_or_else(|| "<default>".to_string(), redact)
            ),
        )
    };

    let observed = marks_observed(inputs, can_mark, selection, selected, model, opted_out);

    vec![
        login,
        current,
        integration,
        resources,
        marks_feature,
        capability.clone(),
        observed,
    ]
}

/// Plan §3.6's table, scored only when the correlation rule of §3.2
/// holds — the selected tab must be doctor's *own* tab, because a tab
/// sitting at a prompt is what a healthy idle tab looks like.
fn marks_observed(
    inputs: &Inputs,
    capability: MarkCapability,
    selection: Selection,
    selected: Option<&Tab>,
    model: AgentModel,
    opted_out: bool,
) -> Check {
    const ID: &str = "shell.marks_observed";
    const TITLE: &str = "Marks observed";

    if !inside_roost_tab(inputs) {
        return not_in_tab(ID, TITLE);
    }
    if opted_out {
        return check(
            ID,
            TITLE,
            Status::Skipped,
            "not scored — marks are opted out via ROOST_SHELL_FEATURES=no-marks",
        );
    }
    if model == AgentModel::Absent {
        return check(
            ID,
            TITLE,
            Status::Skipped,
            "not scored — the UI predates the agent state model, so its `shell_state` is a \
             client-side default rather than an observation",
        );
    }
    if !is_doctors_own_tab(inputs, selection) {
        return check(
            ID,
            TITLE,
            Status::Skipped,
            match selection {
                Selection::None => "not scored — no tab is selected".to_string(),
                _ => format!(
                    "not scored — the selected tab came from {} and is not the tab doctor is \
                     running in",
                    selection.source()
                ),
            },
        );
    }
    let Some(tab) = selected else {
        return check(
            ID,
            TITLE,
            Status::Skipped,
            "not scored — the selected tab was not returned by tab.list",
        );
    };

    match tab.shell_state {
        ShellState::Unknown => check(
            ID,
            TITLE,
            Status::Fail,
            "no OSC 133 mark has arrived since this PTY was created — the shell integration \
             is not loaded in this shell",
        ),
        ShellState::ForegroundProcess => check(
            ID,
            TITLE,
            Status::Ok,
            "the last mark was command-start, so both marks are flowing",
        ),
        ShellState::AtPrompt if capability == MarkCapability::Both => check(
            ID,
            TITLE,
            Status::Ok,
            "the last mark was a prompt/command-end mark — the healthy resting state. It \
             does not confirm the command-start mark: that fires microseconds before \
             doctor execs, so doctor does not wait for it",
        ),
        ShellState::AtPrompt => check(
            ID,
            TITLE,
            Status::Warn,
            "the last mark was a prompt/command-end mark and this shell's command-start \
             capability is unconfirmed — the running dot may never light",
        ),
    }
}

// ---------------------------------------------------------------- tab

fn tab_checks(
    inputs: &Inputs,
    selection: Selection,
    selected: Option<&Tab>,
    listed: bool,
    model: AgentModel,
) -> Vec<Check> {
    let selection_check = match (selection.id(), listed, selected) {
        (None, _, _) => check(
            "tab.selection",
            "Tab selection",
            Status::Skipped,
            "no tab selected — pass --tab, set ROOST_TAB_ID, or give the UI an active tab",
        ),
        (Some(id), false, _) => check(
            "tab.selection",
            "Tab selection",
            Status::Skipped,
            format!(
                "tab {id} (from {}) — tab.list unavailable",
                selection.source()
            ),
        ),
        (Some(id), true, Some(_)) => check(
            "tab.selection",
            "Tab selection",
            Status::Ok,
            format!("tab {id}, chosen by {}", selection.source()),
        ),
        (Some(id), true, None) => check(
            "tab.selection",
            "Tab selection",
            Status::Fail,
            format!(
                "tab {id} (from {}) is not in tab.list — it was closed, or belongs to a \
                 different Roost instance",
                selection.source()
            ),
        ),
    };

    let axis_unavailable = |id: &'static str, title: &'static str| {
        unavailable(id, title, unavailable_reason(model, listed))
    };

    let Some(tab) = selected.filter(|_| model != AgentModel::Absent) else {
        return vec![
            selection_check,
            axis_unavailable("tab.shell_state", "Shell axis"),
            axis_unavailable("tab.agent_lifecycle", "Agent axis"),
            axis_unavailable("tab.attention", "Attention"),
            axis_unavailable("tab.ownership", "Ownership"),
            axis_unavailable("tab.derived", "Derived state"),
            axis_unavailable("tab.raw_osc", "Raw OSC suppression"),
        ];
    };

    let state = tab.agent_state();
    let lifecycle = effective_lifecycle(&state);

    let ownership = match &tab.ownership {
        Some(owner) => observation(
            "tab.ownership",
            "Ownership",
            format!(
                "source={} session={} last_event={} detail={} metadata={}",
                redact(&owner.source),
                fingerprint(&owner.session_id),
                describe_age(inputs.now_unix, owner.last_event_at),
                if owner.detail.is_empty() {
                    "<none>".to_string()
                } else {
                    redact(&owner.detail)
                },
                redact_metadata(&owner.metadata),
            ),
        ),
        None => observation("tab.ownership", "Ownership", "none"),
    };

    vec![
        selection_check,
        observation(
            "tab.shell_state",
            "Shell axis",
            shell_state_name(tab.shell_state),
        ),
        observation(
            "tab.agent_lifecycle",
            "Agent axis",
            lifecycle_name(tab.agent_lifecycle),
        ),
        observation(
            "tab.attention",
            "Attention",
            if tab.has_notification {
                "a notification is pending on this tab"
            } else {
                "no notification pending"
            },
        ),
        ownership,
        observation(
            "tab.derived",
            "Derived state",
            format!(
                // The same spelling `roostctl tab list` and the wire use
                // — doctor must not invent a second vocabulary for the
                // legacy state.
                "state={} hook_active={} lifecycle={}{}",
                crate::format_state(tab.state),
                tab.hook_active,
                lifecycle_name(lifecycle),
                inactive_explanation(&state, lifecycle),
            ),
        ),
        observation(
            "tab.raw_osc",
            "Raw OSC suppression",
            if suppress_raw_osc(&state) {
                "raw OSC 9/99/777 notifications are suppressed while this agent drives the tab"
            } else {
                "raw OSC 9/99/777 notifications are delivered"
            },
        ),
    ]
}

/// Why the selected tab's axes are not observable. Shared with
/// `claude.observed`, which reads the same `ownership` field: the two
/// sections saying different things about one absent fact — `tab.ownership`
/// reporting `unavailable` while `claude.observed` asserts "no claude
/// ownership" six lines later — is the fabrication §3.5 exists to prevent.
fn unavailable_reason(model: AgentModel, listed: bool) -> &'static str {
    match model {
        AgentModel::Absent => "unavailable (server predates the agent state model)",
        AgentModel::Malformed => "unavailable (the UI's tab.list response did not decode)",
        _ if !listed => "unavailable (no tab.list from a running UI)",
        _ => "unavailable (no tab selected)",
    }
}

/// AC 4: when nothing is driving the tab, say which axis is empty rather
/// than leaving the user staring at an absent dot.
fn inactive_explanation(
    state: &roost_ipc::agent::AgentTabState,
    lifecycle: AgentLifecycle,
) -> String {
    if lifecycle != AgentLifecycle::Inactive {
        return String::new();
    }
    match (is_live(state), state.shell) {
        (false, ShellState::Unknown) => " — no agent owns this tab and no OSC 133 mark has \
             arrived since the PTY started"
            .to_string(),
        (false, _) => " — no agent owns this tab and the shell is sitting at a prompt".to_string(),
        (true, _) => {
            let source = state
                .ownership
                .as_ref()
                .map(|o| redact(&o.source))
                .unwrap_or_default();
            format!(
                " — `{source}` still owns this tab as a label, but its lifecycle is inactive \
                 (a prompt mark cleared it, or the agent finished)"
            )
        }
    }
}

fn shell_state_name(s: ShellState) -> &'static str {
    match s {
        ShellState::Unknown => "unknown — no OSC 133 mark since this PTY started",
        ShellState::AtPrompt => "at_prompt — last mark was a prompt/command-end mark",
        ShellState::ForegroundProcess => "foreground_process — last mark was command-start",
    }
}

fn lifecycle_name(l: AgentLifecycle) -> &'static str {
    match l {
        AgentLifecycle::Inactive => "inactive",
        AgentLifecycle::Working => "working",
        AgentLifecycle::Waiting => "waiting",
        AgentLifecycle::Finished => "finished",
        AgentLifecycle::Failed => "failed",
    }
}

fn describe_age(now: i64, at: i64) -> String {
    if at <= 0 {
        return "<never>".to_string();
    }
    let delta = now - at;
    if delta < 0 {
        return format!("{at} (in the future — clock skew)");
    }
    format!("{delta}s ago")
}

// -------------------------------------------------------------- claude

fn claude_checks(
    inputs: &Inputs,
    selection: Selection,
    selected: Option<&Tab>,
    listed: bool,
    model: AgentModel,
) -> Vec<Check> {
    // `Some(_)` only when the server actually sent the axis. A
    // pre-plan-002 server omits `ownership` entirely, and the serde
    // default is `None` — indistinguishable from a real "nobody owns
    // this tab", so the same guard `tab_checks` applies has to apply
    // here too.
    let owns = selected
        .filter(|_| model != AgentModel::Absent)
        .map(|t| t.ownership.as_ref().is_some_and(|o| o.source == "claude"));
    let claude_owns = owns == Some(true);
    // A settings file doctor could not even locate is not evidence that
    // Claude is configured, any more than an absent one is.
    let settings_present = !matches!(
        inputs.claude_settings,
        SettingsProbe::Absent | SettingsProbe::LocationUnknown
    );
    // Unknown ownership counts as "not owned" here on purpose: it keeps
    // the whole section `skipped` rather than inventing a verdict from a
    // fact the server never sent.
    let configured = !matches!(inputs.claude_version, SubprocessOutcome::Missing)
        || settings_present
        || claude_owns;

    if !configured {
        let detail = format!(
            "not configured — no `claude` on PATH, no settings file, and the selected tab's \
             ownership {}",
            match owns {
                Some(_) => "carries no claude source".to_string(),
                None => format!("is {}", unavailable_reason(model, listed)),
            }
        );
        return [
            ("claude.binary", "`claude` on PATH"),
            ("claude.settings", "Settings file"),
            ("claude.hook_events", "Registered events"),
            ("claude.hook_command", "Hook commands"),
            ("claude.observed", "Hooks reaching Roost"),
        ]
        .map(|(id, title)| check(id, title, Status::Skipped, detail.as_str()))
        .to_vec();
    }

    let binary = match &inputs.claude_version {
        SubprocessOutcome::Output(text) if !first_line(text).is_empty() => check(
            "claude.binary",
            "`claude` on PATH",
            Status::Ok,
            redact(first_line(text)),
        ),
        SubprocessOutcome::TimedOut => check(
            "claude.binary",
            "`claude` on PATH",
            Status::Warn,
            "version unknown — `claude --version` did not answer within 2s",
        ),
        SubprocessOutcome::Missing => check(
            "claude.binary",
            "`claude` on PATH",
            Status::Skipped,
            "not on PATH",
        ),
        SubprocessOutcome::Failed(e) => check(
            "claude.binary",
            "`claude` on PATH",
            Status::Warn,
            format!("version unknown: {}", redact(e)),
        ),
        _ => check(
            "claude.binary",
            "`claude` on PATH",
            Status::Warn,
            "version unknown",
        ),
    };

    let path = inputs
        .claude_settings_path
        .as_deref()
        .map_or_else(|| "<unknown>".to_string(), redact_path);
    let settings = match &inputs.claude_settings {
        SettingsProbe::LocationUnknown => check(
            "claude.settings",
            "Settings file",
            Status::Skipped,
            "$HOME is unset, so the settings file location is unknown",
        ),
        SettingsProbe::Parsed => check(
            "claude.settings",
            "Settings file",
            Status::Ok,
            format!("{path} parses"),
        ),
        SettingsProbe::Absent if claude_owns => check(
            "claude.settings",
            "Settings file",
            Status::Fail,
            format!(
                "{path} is absent, yet the selected tab is owned by `claude` — something is \
                 half-wired; run `roostctl claude install --force`"
            ),
        ),
        SettingsProbe::Absent => check(
            "claude.settings",
            "Settings file",
            Status::Skipped,
            format!("{path} is absent — run `roostctl claude install`"),
        ),
        SettingsProbe::Unreadable(e) => check(
            "claude.settings",
            "Settings file",
            Status::Fail,
            format!("{path} is unreadable: {}", redact(e)),
        ),
        SettingsProbe::Unparseable(e) => check(
            "claude.settings",
            "Settings file",
            Status::Fail,
            format!("{path} does not parse: {}", redact(e)),
        ),
    };

    let events = hook_events_check(inputs);
    let commands = hook_command_check(inputs);

    let observed = if claude_owns && is_doctors_own_tab(inputs, selection) {
        check(
            "claude.observed",
            "Hooks reaching Roost",
            Status::Ok,
            "a Claude hook claimed this tab since its PTY last reset. That is not proof \
             hooks are arriving right now: a prompt mark keeps the label while dropping the \
             lifecycle",
        )
    } else if claude_owns {
        check(
            "claude.observed",
            "Hooks reaching Roost",
            Status::Skipped,
            "the selected tab is owned by `claude`, but it is not the tab doctor is running in",
        )
    } else {
        check(
            "claude.observed",
            "Hooks reaching Roost",
            Status::Skipped,
            match owns {
                Some(_) => "no claude ownership on the selected tab".to_string(),
                None => format!(
                    "the selected tab's ownership is {}",
                    unavailable_reason(model, listed)
                ),
            },
        )
    };

    vec![binary, settings, events, commands, observed]
}

/// Why the two settings-derived checks have nothing to say. An absent
/// file and an unparseable one are different findings, and
/// `claude.settings` already carries the verdict for each.
fn unparsed_settings_reason(inputs: &Inputs) -> &'static str {
    match inputs.claude_settings {
        SettingsProbe::LocationUnknown => {
            "not checked — $HOME is unset, so the settings file could not be located"
        }
        SettingsProbe::Absent => "not checked — there is no settings file yet",
        SettingsProbe::Unreadable(_) => "not checked — the settings file is unreadable",
        _ => "not checked — the settings file did not parse",
    }
}

fn hook_events_check(inputs: &Inputs) -> Check {
    const ID: &str = "claude.hook_events";
    const TITLE: &str = "Registered events";
    if !matches!(inputs.claude_settings, SettingsProbe::Parsed) {
        return check(ID, TITLE, Status::Skipped, unparsed_settings_reason(inputs));
    }
    let missing: Vec<&str> = CLAUDE_HOOK_EVENTS
        .iter()
        .copied()
        .filter(|want| !inputs.claude_hook_events.iter().any(|got| got == want))
        .collect();
    let unknown: Vec<String> = inputs
        .claude_hook_events
        .iter()
        .filter(|got| !CLAUDE_HOOK_EVENTS.contains(&got.as_str()))
        .map(|got| redact(got))
        .collect();
    if missing.is_empty() {
        let extra = if unknown.is_empty() {
            String::new()
        } else {
            format!("; unrecognized keys ignored: {}", unknown.join(", "))
        };
        return check(
            ID,
            TITLE,
            Status::Ok,
            format!("all {} events registered{extra}", CLAUDE_HOOK_EVENTS.len()),
        );
    }
    check(
        ID,
        TITLE,
        Status::Fail,
        format!(
            "missing {}; run `roostctl claude install --force`",
            missing.join(", ")
        ),
    )
}

/// Fold `(event, reason)` pairs into one line per *distinct* reason.
///
/// A real install fails the same way on every event — a relocated
/// `Roost.app` makes all six commands resolve elsewhere — and printing
/// the identical sentence six times produced a ~500-character wall that
/// buried the one fact worth reading. Distinct reasons stay distinct;
/// only exact repeats collapse. First-appearance order is preserved so
/// the text tracks the settings file's own key order.
fn group_by_reason(entries: &[(String, String)], total: usize) -> String {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for (event, reason) in entries {
        match groups.iter_mut().find(|(seen, _)| *seen == reason) {
            Some((_, events)) => events.push(event),
            None => groups.push((reason, vec![event])),
        }
    }
    groups
        .into_iter()
        .map(|(reason, events)| match events.as_slice() {
            [only] => format!("{only}: {reason}"),
            many => format!(
                "{} of {total} commands ({}): {reason}",
                many.len(),
                many.join(", ")
            ),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn hook_command_check(inputs: &Inputs) -> Check {
    const ID: &str = "claude.hook_command";
    const TITLE: &str = "Hook commands";
    if !matches!(inputs.claude_settings, SettingsProbe::Parsed) {
        return check(ID, TITLE, Status::Skipped, unparsed_settings_reason(inputs));
    }
    if inputs.claude_hook_commands.is_empty() {
        return check(
            ID,
            TITLE,
            Status::Fail,
            "the settings file registers no command hooks",
        );
    }

    let total = inputs.claude_hook_commands.len();
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut warnings: Vec<(String, String)> = Vec::new();
    // Which canonical events a *usable* command actually reaches. Key
    // presence is not enough: `"StopFailure": []` keeps the key while
    // guaranteeing the event can never arrive (§3.12's whole point).
    let mut reached: Vec<&'static str> = Vec::new();
    for cmd in &inputs.claude_hook_commands {
        let event = redact(&cmd.event);
        let Some(argv) = &cmd.argv else {
            failures.push((event, "command quoting does not parse".to_string()));
            continue;
        };
        // The installer's shape is `<exe> claude-hook <token>`; the token
        // is where the legacy spellings live, so it — not the key — is
        // what needs the normalizer.
        let canonical = match argv.as_slice() {
            [_, sub, token] if sub == "claude-hook" => match canonical_hook_event(token) {
                Some(canonical) if canonical == cmd.event => canonical,
                Some(canonical) => {
                    failures.push((
                        event,
                        format!(
                            "trailing token `{}` normalizes to {canonical}, not its key",
                            redact(token)
                        ),
                    ));
                    continue;
                }
                None => {
                    failures.push((
                        event,
                        format!("trailing token `{}` is not a hook event", redact(token)),
                    ));
                    continue;
                }
            },
            _ => {
                failures.push((
                    event,
                    format!(
                        "expected `<roostctl> claude-hook <event>`, got `{}`",
                        redact(&cmd.raw)
                    ),
                ));
                continue;
            }
        };
        if !cmd.exe_executable {
            let word = argv.first().map_or_else(String::new, |a| redact(a));
            let reason = if word.contains('/') {
                format!("`{word}` is missing or not executable")
            } else {
                format!("`{word}` is not on PATH and is not an executable file")
            };
            failures.push((event, reason));
            continue;
        }
        // A command that runs the wrong binary still reaches the event —
        // it is a warning, not a hole in the coverage below.
        reached.push(canonical);
        if let (Some(theirs), Some(ours)) = (&cmd.exe_canonical, &inputs.roostctl_exe) {
            if theirs != ours {
                warnings.push((
                    event,
                    format!(
                        "resolves to {} rather than the running roostctl at {}",
                        redact_path(theirs),
                        redact_path(ours)
                    ),
                ));
            }
        }
    }

    let unreached: Vec<&str> = CLAUDE_HOOK_EVENTS
        .iter()
        .copied()
        .filter(|want| !reached.contains(want))
        .collect();

    let mut fatal = Vec::new();
    if !failures.is_empty() {
        fatal.push(group_by_reason(&failures, total));
    }
    if !unreached.is_empty() {
        fatal.push(format!(
            "no usable command for {}; run `roostctl claude install --force`",
            unreached.join(", ")
        ));
    }
    if !fatal.is_empty() {
        return check(ID, TITLE, Status::Fail, fatal.join("; "));
    }
    if !warnings.is_empty() {
        return check(ID, TITLE, Status::Warn, group_by_reason(&warnings, total));
    }
    check(
        ID,
        TITLE,
        Status::Ok,
        format!("all {total} hook commands point at this roostctl"),
    )
}

// ============================================================================
// Renderers
// ============================================================================

/// When to color. `Auto` is the only mode that probes anything; the
/// probes themselves live in `main.rs` so [`color_enabled`] stays a pure
/// function of four explicit inputs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

/// Whether to emit SGR escapes, as a pure function so every combination
/// is table-testable — the gating *precedence* is the part that is easy
/// to get wrong, and a crate would bury it behind process-global env
/// reads.
///
/// `NO_COLOR` disables when **present and non-empty**, per
/// <https://no-color.org/>: `NO_COLOR=` (empty) is not an opt-out.
pub fn color_enabled(
    mode: ColorMode,
    is_tty: bool,
    no_color: Option<&str>,
    term: Option<&str>,
) -> bool {
    match mode {
        ColorMode::Never => false,
        ColorMode::Always => true,
        ColorMode::Auto => {
            is_tty && !no_color.is_some_and(|v| !v.is_empty()) && term != Some("dumb")
        }
    }
}

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// How the text renderer decorates. `Default` is uncolored, so a test
/// that does not opt in is asserting on the bytes a pipe would see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

impl Style {
    /// Color goes *around* text already sanitized by
    /// [`escape_controls`], never inside it — the escapes this adds are
    /// the renderer's own, and every detail reaching it is escape-free.
    fn paint(self, sgr: &str, text: &str) -> String {
        if self.color {
            format!("{sgr}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

/// The marker for a rolled-up section or a footer row; see
/// [`Section::status`] for why `None` is neither a tick nor a cross.
fn glyph(status: Option<Status>) -> &'static str {
    match status {
        Some(Status::Ok) => "✓",
        Some(Status::Warn) => "!",
        Some(Status::Fail) => "✗",
        Some(Status::Skipped) => "–",
        None => "•",
    }
}

fn sgr(status: Option<Status>) -> &'static str {
    match status {
        Some(Status::Ok) => GREEN,
        Some(Status::Warn) => YELLOW,
        Some(Status::Fail) => RED,
        Some(Status::Skipped) | None => DIM,
    }
}

/// A status marker in its own colour — the [`glyph`]/[`sgr`] pairing that
/// every summary and footer row opens with.
fn marker(style: Style, status: Option<Status>) -> String {
    style.paint(sgr(status), glyph(status))
}

/// The dim `→ url` continuation both the full view and the footer append.
fn doc_link(style: Style, url: &str) -> String {
    style.paint(DIM, &format!("→ {url}"))
}

/// Rendered text. Plan 003's "no color, no glyphs" is deliberately
/// reversed (§3.12): it justified plain text with the e2e assertions,
/// and the e2e is `--json`-only, so nothing outside this module's own
/// tests reads the layout. Color is TTY-gated by [`color_enabled`] and
/// applied only around already-escaped strings; the `→`/`—` glyphs are
/// **not** gated, because they have been unconditional since #260 and an
/// ASCII fallback would be a promise the rest of the output breaks.
///
/// `verbose` picks the view: the default is one rolled-up line per
/// section (§3.10) — clipped to [`SUMMARY_W`] so "one line" survives a
/// narrow terminal — and `-v` is the full per-check report, where the
/// clipped detail is carried whole. Both end in the same footer, and
/// neither reaches `--json`.
pub fn render_text(report: &Report, style: Style, verbose: bool) -> String {
    let mut out = String::new();
    render_header(report, style, verbose, &mut out);
    if verbose {
        render_full(report, style, &mut out);
    } else {
        render_summary(report, style, &mut out);
    }
    render_footer(report, style, &mut out);
    out
}

/// The one line both views open on, shared for the same reason the footer
/// is: two renderers given two chances to spell the program's own name
/// will eventually spell it two ways. Only the `-v` pointer differs.
fn render_header(report: &Report, style: Style, verbose: bool, out: &mut String) {
    let _ = write!(
        out,
        "roostctl doctor — roostctl {}",
        report.roostctl_version
    );
    if !verbose {
        let _ = write!(
            out,
            " {}",
            style.paint(DIM, "(run `roostctl doctor -v` for the full report)")
        );
    }
    out.push('\n');
}

/// `[{glyph}] ` — the marker column every summary row opens with.
const MARKER_W: usize = 4;
/// Gap between the title column and the headline.
const TITLE_GAP: usize = 3;
/// The whole point of the summary is that it is scannable, and a row
/// that wraps is not — the `claude.hook_command` and `ui.target` details
/// run past 200 characters. A **fixed** budget rather than the
/// terminal's width: [`render_text`] is a pure function, and output that
/// changes with `$COLUMNS` is neither reproducible nor testable. The
/// full sentence is one `-v` away, which the header line names.
const SUMMARY_W: usize = 100;

/// A summary row's two data-dependent column widths: the title field,
/// widened past the longest title so the headlines line up, and whatever
/// the fixed budget leaves the headline. Derived rather than
/// hand-counted so the two cannot drift.
fn summary_columns(report: &Report) -> (usize, usize) {
    let title_w = report
        .sections
        .iter()
        .map(|s| s.title.chars().count())
        .max()
        .unwrap_or(0)
        + TITLE_GAP;
    (title_w, SUMMARY_W.saturating_sub(MARKER_W + title_w))
}

/// Fit an already-escaped string into `cap` columns, marking any cut
/// with `…` so a clipped headline is never mistaken for the whole
/// message. Cutting escaped text cannot forge an escape: the worst a cut
/// does is leave a partial `\u{00`, which is still ordinary text.
fn ellipsize(s: &str, cap: usize) -> Cow<'_, str> {
    // The marker is paid for out of `cap`, so a budget with no room for
    // it has no room for a clipped string either: emit nothing rather
    // than a bare `…` one column past the width the row promises.
    // Unreachable while the section titles are constants, but the
    // row-width contract should hold by construction, not by luck.
    if cap == 0 {
        return Cow::Borrowed("");
    }
    match cut_to_chars(s, cap) {
        // `String::pop` drops a whole character, and the marker spends
        // one of the budget's columns rather than overrunning it, so a
        // cut row is exactly `cap` characters wide.
        Some(mut head) => {
            head.pop();
            head.push('…');
            Cow::Owned(head)
        }
        None => Cow::Borrowed(s),
    }
}

fn render_summary(report: &Report, style: Style, out: &mut String) {
    // The marker is painted outside the padded field, or the escape
    // bytes would count toward the width.
    let (title_w, headline_w) = summary_columns(report);
    out.push('\n');
    for s in &report.sections {
        let _ = writeln!(
            out,
            "[{}] {:<title_w$}{}",
            marker(style, s.status),
            s.title,
            ellipsize(&s.headline, headline_w)
        );
    }
}

fn render_full(report: &Report, style: Style, out: &mut String) {
    // Status column, then the id column; the docs_url continuation line
    // indents past both so it hangs under its check. The column shows a
    // verdict, so it goes blank on `status: None` — not on `kind ==
    // Observation`, which still prints `skipped` when its subject could
    // not be observed. Width is sized for that longest word.
    const STATUS_W: usize = 7;
    const ID_W: usize = 24;

    for section in &report.sections {
        let _ = writeln!(out, "\n{} ({})", section.title, section.scope);
        for c in &section.checks {
            let word = format!("{:<STATUS_W$}", c.status.map_or("", |s| s.as_str()));
            let _ = writeln!(
                out,
                "  {} {:<ID_W$} {}",
                style.paint(sgr(c.status), &word),
                c.id,
                c.detail
            );
            if let Some(url) = c.docs_url {
                let _ = writeln!(
                    out,
                    "{:width$}{}",
                    "",
                    doc_link(style, url),
                    width = 3 + STATUS_W + ID_W + 1
                );
            }
        }
    }
}

/// The one line both views end on. It keeps the exit code and the
/// exit-codes link the pre-summary footer carried: that link is the only
/// in-band explanation for why doctor exits 1 when no UI is running.
/// Rows name **check ids**, not section titles — one section can hold
/// two different failures, and the id is the stable API a user greps and
/// pastes. The counts moved to `--json`.
fn render_footer(report: &Report, style: Style, out: &mut String) {
    let issues: Vec<&Check> = report.issues().collect();
    let worst = worst_adverse(issues.iter().filter_map(|c| c.status)).unwrap_or(Status::Ok);
    let bullet = style.paint(sgr(Some(worst)), "•");
    let link = style.paint(DIM, &format!("({})", EXIT_CODES_DOC.url));
    let exit = report.exit_code();

    if issues.is_empty() {
        let _ = writeln!(out, "\n{bullet} No issues found! — exit {exit} {link}");
        return;
    }
    let noun = if issues.len() == 1 { "issue" } else { "issues" };
    let _ = writeln!(
        out,
        "\n{bullet} {} {noun} found — exit {exit} {link}:",
        issues.len()
    );
    let id_w = issues
        .iter()
        .map(|c| c.id.chars().count())
        .max()
        .unwrap_or(0);
    for c in issues {
        let marker = marker(style, c.status);
        match c.docs_url {
            Some(url) => {
                let _ = writeln!(
                    out,
                    "    {marker} {:<id_w$}  {}",
                    c.id,
                    doc_link(style, url)
                );
            }
            None => {
                let _ = writeln!(out, "    {marker} {}", c.id);
            }
        }
    }
}

/// The JSON shape. `summary` and `exit_code` are methods on [`Report`]
/// rather than fields, but a `--json` consumer must not have to
/// re-derive "did anything fail" from the check list — that would be
/// exactly the second source of truth the text renderer already avoids.
#[derive(Serialize)]
struct ReportView<'a> {
    #[serde(flatten)]
    report: &'a Report,
    summary: Summary,
    exit_code: i32,
}

pub fn render_json(report: &Report) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&ReportView {
        report,
        summary: report.summary(),
        exit_code: report.exit_code(),
    })?)
}

/// Exactly the bytes doctor writes to stdout. The one place the two
/// renderers are chosen between, so `--json`'s independence from `-v`
/// and `--color` is a property a test can assert rather than a shape
/// `main.rs` happens to have.
pub fn render(report: &Report, json: bool, style: Style, verbose: bool) -> anyhow::Result<String> {
    if json {
        let mut out = render_json(report)?;
        out.push('\n');
        Ok(out)
    } else {
        Ok(render_text(report, style, verbose))
    }
}

// ============================================================================
// Tests — over synthetic `Inputs`: no UI, no filesystem, no subprocess.
//
// Inline because `roost-cli` is `[[bin]]`-only with no lib target, so an
// integration test cannot reach a private module. Matches every existing
// CLI test.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::agent::{AgentLifecycle, Ownership, ShellState};
    use roost_ipc::messages::TabState;

    // ---------------------------------------------------------- fixtures

    fn identify(active_tab_id: i64, ui_version: &str) -> IdentifyResult {
        IdentifyResult {
            socket_path: "/tmp/roost.sock".into(),
            pid: 4242,
            active_project_id: 1,
            active_tab_id,
            app_label: "Roost".into(),
            app_id: "ai.stridelabs.Roost".into(),
            ui_version: ui_version.into(),
            protocol_version: 1,
        }
    }

    fn tab(id: i64) -> Tab {
        Tab {
            id,
            project_id: 1,
            title: "zsh".into(),
            cwd: "/tmp".into(),
            state: TabState::None,
            has_notification: false,
            is_active: true,
            user_titled: false,
            position: 0,
            created_at: 1,
            last_active: 2,
            hook_active: false,
            shell_state: ShellState::AtPrompt,
            agent_lifecycle: AgentLifecycle::Inactive,
            ownership: None,
        }
    }

    fn tab_list(tabs: &[Tab]) -> serde_json::Value {
        serde_json::json!({
            "projects": [{
                "id": "1", "name": "roost", "cwd": "/tmp",
                "position": 0, "created_at": 0,
                "tabs": tabs,
            }]
        })
    }

    /// A `tab.list` as a **pre-plan-002** server encodes it: the agent
    /// axes are simply absent. Built as a raw `Value` on purpose — this
    /// is the compatibility seam, and a pre-digested `Inputs` would test
    /// the fixture rather than the detection.
    fn legacy_tab_list(ids: &[i64]) -> serde_json::Value {
        let tabs: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id.to_string(), "project_id": "1", "title": "zsh", "cwd": "/tmp",
                    "state": "none", "has_notification": false, "is_active": true,
                    "user_titled": false, "position": 0, "created_at": 1,
                    "last_active": 2, "hook_active": false,
                })
            })
            .collect();
        serde_json::json!({
            "projects": [{
                "id": "1", "name": "roost", "cwd": "/tmp",
                "position": 0, "created_at": 0, "tabs": tabs,
            }]
        })
    }

    /// A healthy zsh session inside tab 7 of a current UI.
    fn healthy() -> Inputs {
        Inputs {
            roostctl_exe: Some(PathBuf::from("/usr/local/bin/roostctl")),
            now_unix: 1_700_000_100,
            env_tab_id: Some("7".into()),
            env_socket: Some("/tmp/roost.sock".into()),
            env_shell_integration: Some("1".into()),
            env_shell_features: Some("cwd,title,marks,prompt,ssh-env".into()),
            env_resources_dir: Some("/opt/roost".into()),
            shell_path: Some("/bin/zsh".into()),
            shell_usable: true,
            shell_version: SubprocessOutcome::Output("zsh 5.9 (arm-apple-darwin24.0)".into()),
            parent_pid: 100,
            parent_comm: SubprocessOutcome::Output("-zsh\n".into()),
            resources_script: Some(PathBuf::from("/opt/roost/shell-integration/roost.zsh")),
            resources_script_readable: true,
            target_origin: TargetOrigin::SocketEnv,
            target_candidates: vec![PathBuf::from("/tmp/roost.sock")],
            target: Ok(PathBuf::from("/tmp/roost.sock")),
            sockets: vec![SocketProbe {
                path: PathBuf::from("/tmp/roost.sock"),
                profile: None,
                outcome: SocketOutcome::Connected,
            }],
            identify: Ok(identify(7, env!("CARGO_PKG_VERSION"))),
            tab_list: Ok(tab_list(&[tab(7)])),
            // $HOME resolved; Claude simply isn't installed here.
            claude_settings_path: Some(PathBuf::from(
                "/home/roost/.config/roost/claude-settings.json",
            )),
            claude_settings: SettingsProbe::Absent,
            ..Inputs::default()
        }
    }

    fn find<'a>(report: &'a Report, id: &str) -> &'a Check {
        report
            .sections
            .iter()
            .flat_map(|s| &s.checks)
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no check `{id}` in the report"))
    }

    fn status_of(report: &Report, id: &str) -> Option<Status> {
        find(report, id).status
    }

    /// The full per-check report, uncolored — `-v`, and the view every
    /// assertion that names a check id is written against. `Style`'s
    /// `Default` is the uncolored one, so this is also the byte stream a
    /// pipe sees.
    fn verbose_text(report: &Report) -> String {
        render_text(report, Style::default(), true)
    }

    /// The default view: one rolled-up line per section, uncolored.
    fn summary_text(report: &Report) -> String {
        render_text(report, Style::default(), false)
    }

    /// `assert_eq!(status_of(…), Some(Status::Skipped))` overflows the
    /// line at most call sites; this keeps each assertion on one line and
    /// puts the entry's own `detail` in the failure message. `status_of`
    /// stays for the sites that need `assert_ne!`, `None`, or their own
    /// loop-variable message.
    #[track_caller]
    fn assert_status(report: &Report, id: &str, want: Status) {
        let c = find(report, id);
        assert_eq!(c.status, Some(want), "{id}: {}", c.detail);
    }

    /// The fixed inventory (§3.7): 18 checks + 8 observations.
    const CHECK_COUNT: usize = 26;

    // ------------------------------------------------- applicability (AC 7)

    /// AC 7's first half: a machine with no Roost env vars and no Claude
    /// installed is not broken, so the two process-scoped sections carry
    /// **no adverse verdict at all** — not one `fail`, not one `warn` —
    /// and every check whose subject is absent says `skipped` rather than
    /// passing silently. The `ui` section is excluded on purpose: its
    /// whole job is to report that nothing is listening.
    ///
    /// Deliberately not "everything is skipped": `shell.login` and
    /// `claude.binary` are legitimately `ok` on a bare machine whose
    /// subjects are present and fine. This fixture has neither — the
    /// three `$SHELL`s are all unusable — so both land on the absent
    /// list below.
    #[test]
    fn a_bare_environment_never_scores_the_shell_or_claude_sections() {
        const ABSENT: &[&str] = &[
            "env.tab_id",
            "shell.login",
            "shell.integration",
            "shell.resources",
            "shell.marks_feature",
            "shell.marks_capability",
            "shell.marks_observed",
            "claude.binary",
            "claude.settings",
            "claude.hook_events",
            "claude.hook_command",
            "claude.observed",
        ];
        for shell in [None, Some("/opt/homebrew/bin/fish"), Some("relative/zsh")] {
            let inputs = Inputs {
                shell_path: shell.map(str::to_string),
                shell_usable: false,
                target: Err(TargetFailure::NoLiveTarget(vec![PathBuf::from(
                    "/nope/roost.sock",
                )])),
                ..Inputs::default()
            };
            let report = evaluate(&inputs);
            for section in report.sections.iter().filter(|s| s.id != "ui") {
                for c in &section.checks {
                    assert!(
                        !matches!(c.status, Some(Status::Warn | Status::Fail)),
                        "{} scored {:?} in a bare environment ({shell:?}): {}",
                        c.id,
                        c.status,
                        c.detail
                    );
                }
            }
            for id in ABSENT {
                assert_eq!(
                    status_of(&report, id),
                    Some(Status::Skipped),
                    "{id} ({shell:?}): {}",
                    find(&report, id).detail
                );
            }
        }
    }

    /// AC 7's second half, as corrected during C2: with a UI reachable in
    /// that same bare environment, the **whole report** exits 0.
    #[test]
    fn a_bare_environment_with_a_reachable_ui_exits_zero() {
        let inputs = Inputs {
            target: Ok(PathBuf::from("/tmp/roost.sock")),
            sockets: vec![SocketProbe {
                path: PathBuf::from("/tmp/roost.sock"),
                profile: None,
                outcome: SocketOutcome::Connected,
            }],
            identify: Ok(identify(0, env!("CARGO_PKG_VERSION"))),
            tab_list: Ok(tab_list(&[tab(7)])),
            ..Inputs::default()
        };
        let report = evaluate(&inputs);
        let failed: Vec<&str> = report
            .sections
            .iter()
            .flat_map(|s| &s.checks)
            .filter(|c| c.status == Some(Status::Fail))
            .map(|c| c.id)
            .collect();
        assert!(failed.is_empty(), "{failed:?}\n{}", verbose_text(&report));
        assert_eq!(report.exit_code(), 0, "{}", verbose_text(&report));
    }

    /// F6's regression: an ordinary non-Roost shell is never diagnosed,
    /// whatever it is or is not.
    #[test]
    fn the_shell_section_degrades_whole_outside_a_roost_tab() {
        let inputs = Inputs {
            shell_path: Some("/opt/homebrew/bin/fish".into()),
            shell_usable: true,
            shell_version: SubprocessOutcome::Output("fish, version 3.7.1".into()),
            ..Inputs::default()
        };
        let report = evaluate(&inputs);
        assert_status(&report, "shell.marks_capability", Status::Skipped);
        assert_status(&report, "shell.marks_observed", Status::Skipped);

        // …and the same shell inside a tab still warns, so the degrade is
        // applicability, not a hole.
        let in_tab = Inputs {
            shell_path: Some("/opt/homebrew/bin/fish".into()),
            shell_version: SubprocessOutcome::Output("fish, version 3.7.1".into()),
            resources_script: None,
            ..healthy()
        };
        assert_status(&evaluate(&in_tab), "shell.marks_capability", Status::Warn);
    }

    #[test]
    fn env_tab_id_fails_only_inside_a_tab() {
        // Inside a tab — ROOST_SHELL_INTEGRATION, which nothing but a
        // Roost PTY sets — an unset id is the silent no-op every per-tab
        // command hits.
        let inputs = Inputs {
            env_shell_integration: Some("1".into()),
            ..Inputs::default()
        };
        assert_status(&evaluate(&inputs), "env.tab_id", Status::Fail);

        // A set-but-unusable value is broken wherever it came from.
        for bad in ["0", "-3", "abc"] {
            for in_tab in [true, false] {
                let inputs = Inputs {
                    env_shell_integration: in_tab.then(|| "1".to_string()),
                    env_tab_id: Some(bad.to_string()),
                    ..Inputs::default()
                };
                assert_eq!(
                    status_of(&evaluate(&inputs), "env.tab_id"),
                    Some(Status::Fail),
                    "ROOST_TAB_ID={bad:?} in_tab={in_tab}"
                );
            }
        }

        // Outside a tab an unset id is an observation, not a fault.
        assert_status(&evaluate(&Inputs::default()), "env.tab_id", Status::Skipped);
    }

    /// `ROOST_SOCKET` and `ROOST_TAB_ID` are documented user-settable
    /// targeting variables — `docs/reference/cli.md` tells CI runners to
    /// set them by hand from *outside* a Roost tab. Treating either as
    /// proof of a Roost PTY turned that documented usage into three
    /// failures and exit 1 on a healthy machine.
    #[test]
    fn the_targeting_env_vars_do_not_make_the_process_a_roost_tab() {
        for inputs in [
            Inputs {
                env_socket: Some("/tmp/roost.sock".into()),
                ..Inputs::default()
            },
            Inputs {
                env_tab_id: Some("7".into()),
                ..Inputs::default()
            },
            Inputs {
                env_socket: Some("/tmp/roost.sock".into()),
                env_tab_id: Some("7".into()),
                ..Inputs::default()
            },
        ] {
            let report = evaluate(&inputs);
            let shell = report
                .sections
                .iter()
                .find(|s| s.id == "shell")
                .expect("shell section");
            let failed: Vec<&str> = shell
                .checks
                .iter()
                .filter(|c| c.status == Some(Status::Fail))
                .map(|c| c.id)
                .collect();
            assert!(failed.is_empty(), "{failed:?}\n{}", verbose_text(&report));
            assert_ne!(status_of(&report, "env.tab_id"), Some(Status::Fail));
        }
    }

    /// `--tab` selects which tab to inspect; it does not make the
    /// invoking shell a Roost shell, so the process-scoped checks stay
    /// `skipped`.
    #[test]
    fn an_explicit_tab_flag_does_not_make_the_process_a_roost_tab() {
        let inputs = Inputs {
            explicit_tab: Some(42),
            ..Inputs::default()
        };
        let report = evaluate(&inputs);
        assert_status(&report, "env.tab_id", Status::Skipped);
        assert_status(&report, "shell.integration", Status::Skipped);
    }

    // ------------------------------------------------------------- sockets

    #[test]
    fn the_four_socket_outcomes_are_distinct() {
        let cases = [
            (SocketOutcome::Connected, Status::Ok, "connected"),
            (SocketOutcome::Missing, Status::Fail, "missing"),
            (
                SocketOutcome::NotASocket("regular file".into()),
                Status::Fail,
                "not a socket",
            ),
            (SocketOutcome::Stale, Status::Fail, "stale"),
        ];
        let mut details = std::collections::HashSet::new();
        for (outcome, want, needle) in cases {
            let inputs = Inputs {
                sockets: vec![SocketProbe {
                    path: PathBuf::from("/tmp/roost.sock"),
                    profile: None,
                    outcome,
                }],
                ..Inputs::default()
            };
            let check = find(&evaluate(&inputs), "ui.socket").clone();
            assert_eq!(check.status, Some(want), "{needle}");
            assert!(check.detail.contains(needle), "{}", check.detail);
            assert!(
                details.insert(check.detail),
                "outcomes must read differently"
            );
        }
    }

    #[test]
    fn nothing_live_reports_per_profile() {
        let inputs = Inputs {
            target: Err(TargetFailure::NoLiveTarget(vec![
                PathBuf::from("/mac/roost.sock"),
                PathBuf::from("/gtk/roost.sock"),
            ])),
            sockets: vec![
                SocketProbe {
                    path: PathBuf::from("/mac/roost.sock"),
                    profile: Some("mac"),
                    outcome: SocketOutcome::Missing,
                },
                SocketProbe {
                    path: PathBuf::from("/gtk/roost.sock"),
                    profile: Some("gtk"),
                    outcome: SocketOutcome::Stale,
                },
            ],
            ..Inputs::default()
        };
        let report = evaluate(&inputs);
        let detail = &find(&report, "ui.socket").detail;
        assert!(detail.contains("mac /mac/roost.sock: missing"), "{detail}");
        assert!(detail.contains("gtk /gtk/roost.sock: stale"), "{detail}");
        assert_status(&report, "ui.target", Status::Fail);
    }

    #[test]
    fn ambiguity_names_only_the_live_profiles_and_all_selection_options() {
        let inputs = Inputs {
            target_candidates: vec![
                PathBuf::from("/mac/roost.sock"),
                PathBuf::from("/gtk/roost.sock"),
                PathBuf::from("/iced/roost.sock"),
            ],
            target: Err(TargetFailure::Ambiguous(vec!["gtk".into(), "iced".into()])),
            ..Inputs::default()
        };
        let report = evaluate(&inputs);
        let detail = &find(&report, "ui.target").detail;
        assert!(detail.contains("gtk + iced"), "{detail}");
        assert!(!detail.contains("mac + gtk"), "{detail}");
        assert!(detail.contains("--target mac|gtk|iced"), "{detail}");
    }

    // -------------------------------------------- old server / zero tabs

    #[test]
    fn an_old_server_fails_the_agent_model_and_blanks_the_tab_section() {
        let inputs = Inputs {
            env_tab_id: Some("7".into()),
            env_socket: Some("/tmp/roost.sock".into()),
            env_shell_integration: Some("1".into()),
            identify: Ok(identify(7, "0.0.15")),
            tab_list: Ok(legacy_tab_list(&[7])),
            ..healthy()
        };
        let report = evaluate(&inputs);
        assert_status(&report, "ui.agent_model", Status::Fail);
        for id in [
            "tab.shell_state",
            "tab.agent_lifecycle",
            "tab.attention",
            "tab.ownership",
            "tab.derived",
            "tab.raw_osc",
        ] {
            let c = find(&report, id);
            // An observation that could not be observed: `skipped` is
            // what makes "unavailable" machine-readable rather than a
            // sentence a consumer has to parse out of `detail`.
            assert_eq!(c.kind, Kind::Observation, "{id}");
            assert_eq!(c.status, Some(Status::Skipped), "{id}");
            assert!(
                c.detail.contains("predates the agent state model"),
                "{id}: {}",
                c.detail
            );
        }
        // Blaming the shell for the server's age would be wrong.
        assert_status(&report, "shell.marks_observed", Status::Skipped);
        // The selection itself is still knowable from a legacy tab.list.
        assert_status(&report, "tab.selection", Status::Ok);
        // `claude.observed` reads the same `ownership` field, so it has
        // to degrade with it: `tab.ownership` reporting `unavailable`
        // while `claude.observed` asserts "no claude ownership" six lines
        // later states as an observation something the server never sent.
        let observed = find(&report, "claude.observed");
        assert_eq!(observed.status, Some(Status::Skipped));
        assert!(
            observed.detail.contains("predates the agent state model"),
            "{}",
            observed.detail
        );
    }

    /// The same guard on the section's other exit: with nothing else
    /// configured the five `claude` checks share one "not configured"
    /// sentence, and its third clause must not claim an ownership fact
    /// the server never sent either.
    #[test]
    fn the_unconfigured_claude_sentence_does_not_invent_ownership() {
        let old_server = Inputs {
            tab_list: Ok(legacy_tab_list(&[7])),
            claude_version: SubprocessOutcome::Missing,
            ..healthy()
        };
        let report = evaluate(&old_server);
        for id in [
            "claude.binary",
            "claude.settings",
            "claude.hook_events",
            "claude.hook_command",
            "claude.observed",
        ] {
            let c = find(&report, id);
            assert_eq!(c.status, Some(Status::Skipped), "{id}");
            assert!(
                c.detail.contains("predates the agent state model"),
                "{id}: {}",
                c.detail
            );
        }

        // A current server that really did send `ownership: null` is a
        // genuine observation, and still reads as one.
        let current = Inputs {
            claude_version: SubprocessOutcome::Missing,
            ..healthy()
        };
        let report = evaluate(&current);
        let detail = &find(&report, "claude.observed").detail;
        assert!(detail.contains("no claude source"), "{detail}");
    }

    #[test]
    fn zero_tabs_leaves_the_agent_model_undetermined() {
        let inputs = Inputs {
            tab_list: Ok(tab_list(&[])),
            identify: Ok(identify(0, env!("CARGO_PKG_VERSION"))),
            ..healthy()
        };
        let report = evaluate(&inputs);
        let c = find(&report, "ui.agent_model");
        assert_eq!(c.status, Some(Status::Skipped));
        assert!(c.detail.contains("undetermined"), "{}", c.detail);
    }

    #[test]
    fn a_current_server_passes_the_agent_model() {
        assert_status(&evaluate(&healthy()), "ui.agent_model", Status::Ok);
    }

    /// A `tab.list` that does not decode is a protocol failure, not an
    /// empty tab list. Laundering it into `skipped: undetermined` reported a
    /// broken server as a healthy UI with nothing open — and exited 0.
    #[test]
    fn a_malformed_tab_list_is_a_protocol_failure_not_no_tabs() {
        for malformed in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({"projects": null}),
            serde_json::json!({"projects": [{"tabs": null}]}),
            serde_json::json!({"projects": "one"}),
        ] {
            let inputs = Inputs {
                tab_list: Ok(malformed.clone()),
                ..healthy()
            };
            let report = evaluate(&inputs);
            let c = find(&report, "ui.agent_model");
            assert_eq!(c.status, Some(Status::Fail), "{malformed}: {}", c.detail);
            assert!(!c.detail.contains("no tabs"), "{}", c.detail);
            assert_eq!(report.exit_code(), 1, "{malformed}");
            // …and the tab section says why rather than fabricating.
            let axis = find(&report, "tab.shell_state");
            assert_eq!(axis.status, Some(Status::Skipped));
            assert!(axis.detail.contains("did not decode"), "{}", axis.detail);
        }
        // A genuinely empty list stays `skipped` — the two must not merge.
        let empty = Inputs {
            tab_list: Ok(tab_list(&[])),
            identify: Ok(identify(0, env!("CARGO_PKG_VERSION"))),
            ..healthy()
        };
        assert_status(&evaluate(&empty), "ui.agent_model", Status::Skipped);
    }

    /// Both axes are probed, not just `shell_state`: a tab carrying one
    /// and not the other would otherwise pass, and the tab section would
    /// then print the missing axis's serde default as an observation.
    #[test]
    fn a_tab_missing_either_axis_fails_the_agent_model() {
        for present in ["shell_state", "agent_lifecycle"] {
            let mut raw = legacy_tab_list(&[7]);
            raw["projects"][0]["tabs"][0][present] = match present {
                "shell_state" => serde_json::json!("at_prompt"),
                _ => serde_json::json!("inactive"),
            };
            let inputs = Inputs {
                tab_list: Ok(raw),
                ..healthy()
            };
            let report = evaluate(&inputs);
            assert_eq!(
                status_of(&report, "ui.agent_model"),
                Some(Status::Fail),
                "only `{present}` present"
            );
            assert_status(&report, "tab.agent_lifecycle", Status::Skipped);
            assert!(find(&report, "tab.agent_lifecycle")
                .detail
                .contains("predates the agent state model"));
        }
    }

    // -------------------------------------------------------------- shell

    #[test]
    fn bash_3_2_warns_about_the_command_start_mark() {
        let inputs = Inputs {
            shell_path: Some("/bin/bash".into()),
            shell_version: SubprocessOutcome::Output(
                "GNU bash, version 3.2.57(1)-release (arm64-apple-darwin24)".into(),
            ),
            ..healthy()
        };
        let c = find(&evaluate(&inputs), "shell.marks_capability").clone();
        assert_eq!(c.status, Some(Status::Warn));
        assert!(c.detail.contains("3.2"), "{}", c.detail);
        assert!(c.docs_url.is_some());
    }

    #[test]
    fn modern_bash_and_zsh_support_both_marks() {
        for banner in [
            "GNU bash, version 5.3.9(1)-release (aarch64-apple-darwin24.4.0)",
            "GNU bash, version 4.4.0(1)-release",
        ] {
            let inputs = Inputs {
                shell_path: Some("/opt/homebrew/bin/bash".into()),
                shell_version: SubprocessOutcome::Output(banner.into()),
                ..healthy()
            };
            assert_eq!(
                status_of(&evaluate(&inputs), "shell.marks_capability"),
                Some(Status::Ok),
                "{banner}"
            );
        }
        assert_status(&evaluate(&healthy()), "shell.marks_capability", Status::Ok);
    }

    #[test]
    fn a_shell_with_no_shipped_integration_warns_and_skips_resources() {
        let inputs = Inputs {
            shell_path: Some("/opt/homebrew/bin/fish".into()),
            shell_version: SubprocessOutcome::Output("fish, version 3.7.1".into()),
            resources_script: None,
            resources_script_readable: false,
            ..healthy()
        };
        let report = evaluate(&inputs);
        assert_eq!(
            status_of(&report, "shell.marks_capability"),
            Some(Status::Warn),
            "{}",
            verbose_text(&report)
        );
        let resources = find(&report, "shell.resources");
        assert_eq!(resources.status, Some(Status::Skipped));
        assert!(resources.detail.contains("not applicable"));
    }

    #[test]
    fn a_missing_resources_dir_fails_inside_a_tab() {
        let inputs = Inputs {
            env_resources_dir: None,
            resources_script: None,
            resources_script_readable: false,
            ..healthy()
        };
        assert_status(&evaluate(&inputs), "shell.resources", Status::Fail);
    }

    #[test]
    fn an_unreadable_shipped_script_fails() {
        let inputs = Inputs {
            resources_script_readable: false,
            ..healthy()
        };
        assert_status(&evaluate(&inputs), "shell.resources", Status::Fail);
    }

    #[test]
    fn a_missing_shell_warns_but_does_not_fail() {
        let inputs = Inputs {
            shell_path: None,
            shell_usable: false,
            ..healthy()
        };
        let report = evaluate(&inputs);
        assert_status(&report, "shell.login", Status::Warn);
        assert_status(&report, "shell.marks_capability", Status::Skipped);
        assert!(find(&report, "shell.login").docs_url.is_some());
    }

    #[test]
    fn a_non_executable_shell_warns() {
        let inputs = Inputs {
            shell_path: Some("relative/zsh".into()),
            shell_usable: false,
            shell_version: SubprocessOutcome::Skipped,
            ..healthy()
        };
        assert_status(&evaluate(&inputs), "shell.login", Status::Warn);
    }

    #[test]
    fn shell_integration_fails_when_the_contract_is_broken_inside_a_tab() {
        let inputs = Inputs {
            env_shell_integration: None,
            ..healthy()
        };
        assert_status(&evaluate(&inputs), "shell.integration", Status::Fail);
    }

    // ---------------------------------------------------- marks observed

    #[test]
    fn marks_observed_fails_when_no_mark_has_ever_arrived() {
        let inputs = Inputs {
            tab_list: Ok(tab_list(&[Tab {
                shell_state: ShellState::Unknown,
                ..tab(7)
            }])),
            ..healthy()
        };
        let c = find(&evaluate(&inputs), "shell.marks_observed").clone();
        assert_eq!(c.status, Some(Status::Fail));
        assert!(c.docs_url.is_some());
    }

    #[test]
    fn at_prompt_with_a_capable_shell_is_ok_and_names_the_race() {
        let c = find(&evaluate(&healthy()), "shell.marks_observed").clone();
        assert_eq!(c.status, Some(Status::Ok));
        assert!(
            c.detail.contains("healthy resting state"),
            "the detail must not imply a fault: {}",
            c.detail
        );
        assert!(
            c.detail.contains("microseconds before"),
            "the detail must name the race: {}",
            c.detail
        );
    }

    #[test]
    fn at_prompt_without_a_capable_shell_warns() {
        let inputs = Inputs {
            shell_path: Some("/bin/bash".into()),
            shell_version: SubprocessOutcome::Output("GNU bash, version 3.2.57(1)-release".into()),
            ..healthy()
        };
        assert_status(&evaluate(&inputs), "shell.marks_observed", Status::Warn);
    }

    #[test]
    fn foreground_process_is_always_ok() {
        let inputs = Inputs {
            tab_list: Ok(tab_list(&[Tab {
                shell_state: ShellState::ForegroundProcess,
                ..tab(7)
            }])),
            ..healthy()
        };
        assert_status(&evaluate(&inputs), "shell.marks_observed", Status::Ok);
    }

    /// The correlation rule of §3.2: a tab sitting at a prompt is exactly
    /// what a healthy idle tab looks like, so judging someone else's tab
    /// against this shell would be wrong in the common case.
    #[test]
    fn marks_observed_is_never_scored_on_another_tab() {
        let inputs = Inputs {
            explicit_tab: Some(9),
            tab_list: Ok(tab_list(&[
                tab(7),
                Tab {
                    shell_state: ShellState::Unknown,
                    ..tab(9)
                },
            ])),
            ..healthy()
        };
        let report = evaluate(&inputs);
        assert_status(&report, "shell.marks_observed", Status::Skipped);
        assert_eq!(report.exit_code(), 0, "{}", verbose_text(&report));
    }

    #[test]
    fn no_marks_downgrades_both_marks_checks_to_skipped() {
        let inputs = Inputs {
            env_shell_features: Some("cwd,title,no-marks,prompt".into()),
            tab_list: Ok(tab_list(&[Tab {
                shell_state: ShellState::Unknown,
                ..tab(7)
            }])),
            ..healthy()
        };
        let report = evaluate(&inputs);
        assert_status(&report, "shell.marks_feature", Status::Skipped);
        assert_status(&report, "shell.marks_observed", Status::Skipped);
        assert_eq!(report.exit_code(), 0);
    }

    // ---------------------------------------------------------------- tab

    #[test]
    fn a_nonexistent_tab_fails_selection() {
        let inputs = Inputs {
            explicit_tab: Some(999),
            ..healthy()
        };
        let report = evaluate(&inputs);
        let c = find(&report, "tab.selection");
        assert_eq!(c.status, Some(Status::Fail));
        assert!(c.detail.contains("--tab"), "{}", c.detail);
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn selection_records_which_input_chose_the_tab() {
        for (inputs, needle) in [
            (
                Inputs {
                    explicit_tab: Some(7),
                    ..healthy()
                },
                "--tab",
            ),
            (healthy(), "$ROOST_TAB_ID"),
            (
                Inputs {
                    env_tab_id: None,
                    ..healthy()
                },
                "the UI's active tab",
            ),
        ] {
            let report = evaluate(&inputs);
            let c = find(&report, "tab.selection");
            assert_eq!(c.status, Some(Status::Ok), "{needle}");
            assert!(c.detail.contains(needle), "{}", c.detail);
        }
    }

    /// AC 8 — the `tab` section is observation, not verdict.
    #[test]
    fn a_failed_agent_lifecycle_does_not_exit_one() {
        let inputs = Inputs {
            tab_list: Ok(tab_list(&[Tab {
                state: TabState::NeedsInput,
                agent_lifecycle: AgentLifecycle::Failed,
                ownership: Some(Ownership {
                    source: "claude".into(),
                    session_id: "s-1".into(),
                    last_event_at: 1_700_000_000,
                    detail: "rate_limit".into(),
                    ..Ownership::default()
                }),
                ..tab(7)
            }])),
            claude_settings: SettingsProbe::Parsed,
            claude_hook_events: CLAUDE_HOOK_EVENTS.iter().map(|e| e.to_string()).collect(),
            claude_hook_commands: hook_commands(&["/usr/local/bin/roostctl"]),
            ..healthy()
        };
        let report = evaluate(&inputs);
        let c = find(&report, "tab.agent_lifecycle");
        // A resolved axis is a fact: `failed` is what the agent reported,
        // not a verdict on the user's setup.
        assert_eq!(c.kind, Kind::Observation);
        assert_eq!(c.status, None);
        assert!(c.detail.contains("failed"));
        assert_eq!(report.exit_code(), 0, "{}", verbose_text(&report));
    }

    /// AC 4 — when nothing drives the tab, say which axis is empty.
    #[test]
    fn the_derived_check_names_the_empty_axis() {
        let cases: [(Tab, &str); 3] = [
            (
                Tab {
                    shell_state: ShellState::Unknown,
                    ..tab(7)
                },
                "no OSC 133 mark has",
            ),
            (
                Tab {
                    shell_state: ShellState::AtPrompt,
                    ..tab(7)
                },
                "sitting at a prompt",
            ),
            (
                Tab {
                    shell_state: ShellState::AtPrompt,
                    agent_lifecycle: AgentLifecycle::Inactive,
                    ownership: Some(Ownership {
                        source: "claude".into(),
                        ..Ownership::default()
                    }),
                    ..tab(7)
                },
                "still owns this tab as a label",
            ),
        ];
        for (t, needle) in cases {
            let inputs = Inputs {
                tab_list: Ok(tab_list(&[t])),
                ..healthy()
            };
            let detail = find(&evaluate(&inputs), "tab.derived").detail.clone();
            assert!(detail.contains(needle), "{detail}");
        }
    }

    /// The legacy `state` must be spelled the way `roostctl tab list` and
    /// the wire spell it. Debug-formatting the enum here printed
    /// `NeedsInput` against everything else's `needs_input`.
    #[test]
    fn the_derived_state_uses_the_wire_vocabulary() {
        let inputs = Inputs {
            tab_list: Ok(tab_list(&[Tab {
                state: TabState::NeedsInput,
                ..tab(7)
            }])),
            ..healthy()
        };
        let detail = find(&evaluate(&inputs), "tab.derived").detail.clone();
        assert!(detail.contains("state=needs_input"), "{detail}");
    }

    #[test]
    fn raw_osc_suppression_is_reported_both_ways() {
        let driving = Inputs {
            tab_list: Ok(tab_list(&[Tab {
                agent_lifecycle: AgentLifecycle::Working,
                ownership: Some(Ownership {
                    source: "claude".into(),
                    ..Ownership::default()
                }),
                ..tab(7)
            }])),
            ..healthy()
        };
        assert!(find(&evaluate(&driving), "tab.raw_osc")
            .detail
            .contains("suppressed"));
        assert!(find(&evaluate(&healthy()), "tab.raw_osc")
            .detail
            .contains("delivered"));
    }

    // ------------------------------------------------------------ ui legs

    #[test]
    fn identify_can_succeed_while_tab_list_fails() {
        let inputs = Inputs {
            tab_list: Err("server returned error: internal — boom".into()),
            ..healthy()
        };
        let report = evaluate(&inputs);
        assert_status(&report, "ui.identify", Status::Ok);
        let model = find(&report, "ui.agent_model");
        assert_eq!(model.status, Some(Status::Skipped));
        assert!(model.detail.contains("tab.list failed"), "{}", model.detail);
        // A tab section with no list is unavailable, not fabricated.
        assert_status(&report, "tab.shell_state", Status::Skipped);
    }

    /// `client.rs` keeps `Protocol` and `Io` apart deliberately; doctor
    /// must not collapse them back together.
    #[test]
    fn protocol_and_io_failures_read_differently() {
        let protocol = Inputs {
            identify: Err(IdentifyFailure::Protocol("missing field `app_id`".into())),
            ..healthy()
        };
        let io = Inputs {
            identify: Err(IdentifyFailure::Io("broken pipe".into())),
            ..healthy()
        };
        let timeout = Inputs {
            identify: Err(IdentifyFailure::Timeout),
            ..healthy()
        };
        let p = find(&evaluate(&protocol), "ui.identify").detail.clone();
        let i = find(&evaluate(&io), "ui.identify").detail.clone();
        let t = find(&evaluate(&timeout), "ui.identify").detail.clone();
        assert!(p.contains("schema drift"), "{p}");
        assert!(i.contains("transport failure"), "{i}");
        assert!(t.contains("timed out"), "{t}");
        assert_ne!(p, i);
        assert_ne!(i, t);
        for inputs in [&protocol, &io, &timeout] {
            assert_status(&evaluate(inputs), "ui.identify", Status::Fail);
        }
    }

    #[test]
    fn version_skew_warns_without_failing() {
        let inputs = Inputs {
            identify: Ok(identify(7, "0.0.1")),
            ..healthy()
        };
        let report = evaluate(&inputs);
        let c = find(&report, "ui.version");
        assert_eq!(c.status, Some(Status::Warn));
        assert!(c.docs_url.is_some());
        assert_eq!(report.exit_code(), 0, "{}", verbose_text(&report));
    }

    // ------------------------------------------------------------- claude

    fn hook_commands(exes: &[&str]) -> Vec<HookCommand> {
        CLAUDE_HOOK_EVENTS
            .iter()
            .zip(exes.iter().cycle())
            .map(|(event, exe)| HookCommand {
                event: (*event).to_string(),
                raw: format!("{exe} claude-hook {event}"),
                argv: Some(vec![
                    (*exe).to_string(),
                    "claude-hook".into(),
                    (*event).to_string(),
                ]),
                exe_executable: true,
                exe_canonical: Some(PathBuf::from(*exe)),
            })
            .collect()
    }

    fn configured(commands: Vec<HookCommand>) -> Inputs {
        Inputs {
            claude_version: SubprocessOutcome::Output("2.1.220 (Claude Code)".into()),
            claude_settings: SettingsProbe::Parsed,
            claude_hook_events: CLAUDE_HOOK_EVENTS.iter().map(|e| e.to_string()).collect(),
            claude_hook_commands: commands,
            ..healthy()
        }
    }

    #[test]
    fn a_fully_installed_claude_passes() {
        let report = evaluate(&configured(hook_commands(&["/usr/local/bin/roostctl"])));
        for id in ["claude.binary", "claude.settings", "claude.hook_events"] {
            assert_status(&report, id, Status::Ok);
        }
        assert_status(&report, "claude.hook_command", Status::Ok);
    }

    /// The one token `EventKind::parse` used to reject. Every already
    /// installed settings file on disk carries it, so it must pass.
    #[test]
    fn the_legacy_prompt_submit_token_passes() {
        let mut commands = hook_commands(&["/usr/local/bin/roostctl"]);
        for cmd in &mut commands {
            if cmd.event == "UserPromptSubmit" {
                cmd.raw = "/usr/local/bin/roostctl claude-hook prompt-submit".into();
                cmd.argv = Some(vec![
                    "/usr/local/bin/roostctl".into(),
                    "claude-hook".into(),
                    "prompt-submit".into(),
                ]);
            }
        }
        assert_status(
            &evaluate(&configured(commands)),
            "claude.hook_command",
            Status::Ok,
        );
    }

    #[test]
    fn a_token_that_does_not_normalize_to_its_key_fails() {
        let mut commands = hook_commands(&["/usr/local/bin/roostctl"]);
        commands[0].argv = Some(vec![
            "/usr/local/bin/roostctl".into(),
            "claude-hook".into(),
            "Stop".into(),
        ]);
        let c = find(&evaluate(&configured(commands)), "claude.hook_command").clone();
        assert_eq!(c.status, Some(Status::Fail));
        assert!(c.detail.contains("not its key"), "{}", c.detail);
        assert!(c.docs_url.is_some());
    }

    #[test]
    fn a_missing_or_non_executable_hook_binary_fails() {
        let mut commands = hook_commands(&["/usr/local/bin/roostctl"]);
        commands[0].exe_executable = false;
        commands[0].exe_canonical = None;
        let c = find(&evaluate(&configured(commands)), "claude.hook_command").clone();
        assert_eq!(c.status, Some(Status::Fail));
        assert!(c.detail.contains("not executable"), "{}", c.detail);
    }

    #[test]
    fn a_hook_pointing_at_a_different_binary_warns() {
        let c = find(
            &evaluate(&configured(hook_commands(&["/opt/other/roostctl"]))),
            "claude.hook_command",
        )
        .clone();
        assert_eq!(c.status, Some(Status::Warn));
        assert!(c.detail.contains("/opt/other/roostctl"), "{}", c.detail);
    }

    /// The common shape — a relocated binary breaks every command the
    /// same way — must read as one sentence naming the count and the
    /// events, not as six near-identical ones.
    #[test]
    fn one_reason_shared_by_every_command_collapses_to_one_line() {
        let n = CLAUDE_HOOK_EVENTS.len();
        let c = find(
            &evaluate(&configured(hook_commands(&["/opt/other/roostctl"]))),
            "claude.hook_command",
        )
        .clone();
        assert_eq!(
            c.detail.matches("rather than the running roostctl").count(),
            1,
            "the shared reason should appear once: {}",
            c.detail
        );
        assert!(
            c.detail.starts_with(&format!("{n} of {n} commands (")),
            "{}",
            c.detail
        );
        for event in CLAUDE_HOOK_EVENTS {
            assert!(c.detail.contains(event), "{event} missing: {}", c.detail);
        }
    }

    /// …but two different faults must not be merged into one.
    #[test]
    fn distinct_reasons_stay_distinct() {
        let mut commands = hook_commands(&["/usr/local/bin/roostctl"]);
        commands[0].argv = None;
        commands[1].argv = Some(vec![
            "/usr/local/bin/roostctl".into(),
            "claude-hook".into(),
            "not-an-event".into(),
        ]);
        let c = find(&evaluate(&configured(commands)), "claude.hook_command").clone();
        assert_eq!(c.status, Some(Status::Fail));
        assert!(c.detail.contains("does not parse"), "{}", c.detail);
        assert!(c.detail.contains("is not a hook event"), "{}", c.detail);
        // Two events, two reasons — each named individually, no grouping.
        assert!(!c.detail.contains(" commands ("), "{}", c.detail);
    }

    #[test]
    fn unparseable_hook_quoting_fails() {
        let mut commands = hook_commands(&["/usr/local/bin/roostctl"]);
        commands[0].argv = None;
        commands[0].raw = "'/unterminated claude-hook SessionStart".into();
        assert_status(
            &evaluate(&configured(commands)),
            "claude.hook_command",
            Status::Fail,
        );
    }

    /// The keys can all be present and an event still be unreachable —
    /// `"StopFailure": []` is the shape. Key presence alone reported both
    /// checks `ok` on a settings file no `StopFailure` can ever traverse.
    #[test]
    fn an_event_with_no_usable_command_fails_even_with_every_key_present() {
        let commands: Vec<HookCommand> = hook_commands(&["/usr/local/bin/roostctl"])
            .into_iter()
            .filter(|c| c.event != "StopFailure")
            .collect();
        let report = evaluate(&configured(commands));
        // The keys really are all there.
        assert_status(&report, "claude.hook_events", Status::Ok);
        let c = find(&report, "claude.hook_command");
        assert_eq!(c.status, Some(Status::Fail), "{}", c.detail);
        assert!(c.detail.contains("StopFailure"), "{}", c.detail);
        assert!(c.detail.contains("--force"), "{}", c.detail);
        assert!(c.docs_url.is_some());
    }

    /// An event whose only command is broken is unreachable too — the
    /// per-command failure and the coverage hole are both reported.
    #[test]
    fn a_broken_command_leaves_its_event_unreached() {
        let mut commands = hook_commands(&["/usr/local/bin/roostctl"]);
        commands[0].exe_executable = false;
        commands[0].exe_canonical = None;
        let c = find(&evaluate(&configured(commands)), "claude.hook_command").clone();
        assert_eq!(c.status, Some(Status::Fail));
        assert!(c.detail.contains("not executable"), "{}", c.detail);
        assert!(
            c.detail.contains(CLAUDE_HOOK_EVENTS[0]),
            "the unreached event must be named: {}",
            c.detail
        );
    }

    #[test]
    fn a_settings_file_missing_an_event_names_it() {
        let inputs = Inputs {
            claude_hook_events: CLAUDE_HOOK_EVENTS
                .iter()
                .filter(|e| **e != "StopFailure")
                .map(|e| e.to_string())
                .collect(),
            ..configured(hook_commands(&["/usr/local/bin/roostctl"]))
        };
        let c = find(&evaluate(&inputs), "claude.hook_events").clone();
        assert_eq!(c.status, Some(Status::Fail));
        assert!(c.detail.contains("StopFailure"), "{}", c.detail);
        assert!(c.detail.contains("--force"), "{}", c.detail);
    }

    #[test]
    fn an_unparseable_settings_file_fails() {
        for probe in [
            SettingsProbe::Unparseable("expected value at line 1".into()),
            SettingsProbe::Unreadable("permission denied".into()),
        ] {
            let inputs = Inputs {
                claude_settings: probe,
                claude_hook_events: Vec::new(),
                claude_hook_commands: Vec::new(),
                claude_version: SubprocessOutcome::Output("2.1.220".into()),
                ..healthy()
            };
            let report = evaluate(&inputs);
            assert_status(&report, "claude.settings", Status::Fail);
            // Nothing parsed, so the two downstream checks are
            // `skipped` rather than piling on.
            assert_status(&report, "claude.hook_events", Status::Skipped);
            assert_status(&report, "claude.hook_command", Status::Skipped);
        }
    }

    /// Half-wired: the tab says Claude owns it, but there is no settings
    /// file for the hooks that would have said so.
    #[test]
    fn claude_ownership_without_settings_fails() {
        let inputs = Inputs {
            claude_version: SubprocessOutcome::Missing,
            claude_settings: SettingsProbe::Absent,
            tab_list: Ok(tab_list(&[Tab {
                ownership: Some(Ownership {
                    source: "claude".into(),
                    session_id: "s-1".into(),
                    ..Ownership::default()
                }),
                ..tab(7)
            }])),
            ..healthy()
        };
        let report = evaluate(&inputs);
        assert_status(&report, "claude.settings", Status::Fail);
        assert_status(&report, "claude.observed", Status::Ok);
    }

    #[test]
    fn a_slow_claude_binary_warns_while_an_absent_one_is_skipped() {
        let slow = Inputs {
            claude_version: SubprocessOutcome::TimedOut,
            claude_settings: SettingsProbe::Parsed,
            claude_hook_events: CLAUDE_HOOK_EVENTS.iter().map(|e| e.to_string()).collect(),
            claude_hook_commands: hook_commands(&["/usr/local/bin/roostctl"]),
            ..healthy()
        };
        let c = find(&evaluate(&slow), "claude.binary").clone();
        assert_eq!(c.status, Some(Status::Warn));
        assert!(c.detail.contains("version unknown"), "{}", c.detail);
        assert!(c.docs_url.is_some());

        let absent = Inputs {
            claude_version: SubprocessOutcome::Missing,
            ..slow
        };
        assert_status(&evaluate(&absent), "claude.binary", Status::Skipped);
    }

    #[test]
    fn claude_observed_is_not_scored_on_another_tab() {
        let inputs = Inputs {
            explicit_tab: Some(9),
            tab_list: Ok(tab_list(&[
                tab(7),
                Tab {
                    ownership: Some(Ownership {
                        source: "claude".into(),
                        ..Ownership::default()
                    }),
                    ..tab(9)
                },
            ])),
            ..configured(hook_commands(&["/usr/local/bin/roostctl"]))
        };
        let c = find(&evaluate(&inputs), "claude.observed").clone();
        assert_eq!(c.status, Some(Status::Skipped));
        assert!(c.detail.contains("not the tab doctor is running in"));
    }

    // ---------------------------------------------------------- redaction

    #[test]
    fn control_characters_are_escaped() {
        let raw = "line\nnext\r\x1b[31mred\x07";
        let out = redact(raw);
        assert!(!out.contains('\n'), "{out}");
        assert!(!out.contains('\x1b'), "{out}");
        assert!(out.contains("\\n"), "{out}");
        assert!(out.contains("\\u{001b}"), "{out}");
        assert!(out.contains("\\u{0007}"), "{out}");

        // U+2028/U+2029 are Zl/Zp, not `is_control()`, so they need their own
        // arm — a paste target that honors them would otherwise get a forged
        // report line out of a string this function claims to have neutered.
        let separators = redact("a\u{2028}b\u{2029}c");
        assert!(!separators.contains('\u{2028}'), "{separators}");
        assert!(!separators.contains('\u{2029}'), "{separators}");
        assert!(separators.contains("\\u{2028}"), "{separators}");
        assert!(separators.contains("\\u{2029}"), "{separators}");
    }

    /// The bidi half of §3.9's threat model, and the reason the escaping is
    /// stated as a *category* rather than a list of the characters someone
    /// happened to try: `Cf` is neither `is_control()` nor whitespace, so a
    /// raw U+202E RIGHT-TO-LEFT OVERRIDE used to reach the row verbatim.
    /// GitHub and every browser honor it, which is exactly the paste target
    /// the policy is written for — `$SHELL=<RLO>bat detceleS ]✗[` reads
    /// there as `[✗] Selected tab`.
    #[test]
    fn format_characters_are_escaped() {
        // Both endpoints of every range, so a mistyped bound cannot pass,
        // and both *neighbours* of every range, because escaping a category
        // must not quietly become escaping its neighbourhood — a legitimate
        // path has to stay readable in the output that exists to explain it.
        // Walked off the table rather than hand-listed so a range added
        // later is covered by construction.
        for (lo, hi) in FORMAT_RANGES {
            for cp in [lo, hi] {
                let c = char::from_u32(cp).unwrap();
                assert_eq!(redact(&format!("a{c}b")), format!("a\\u{{{cp:04x}}}b"));
            }
            // Skip neighbours that are not scalars at all: a future range
            // starting at U+0000 would underflow, and one bordering the
            // surrogate block has no neighbour to probe. Both would panic
            // instead of failing, which defeats the point of walking the
            // table rather than hand-listing it.
            for cp in [lo.checked_sub(1), hi.checked_add(1)].into_iter().flatten() {
                let Some(c) = char::from_u32(cp) else {
                    continue;
                };
                assert!(!is_format(c), "U+{cp:04X} is not a format character");
                // U+2029 borders the bidi range and is escaped by the Zl/Zp
                // arm, so only the neighbours no other arm claims can be
                // asserted to reach the output verbatim.
                if !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}') {
                    let raw = format!("a{c}b");
                    assert_eq!(redact(&raw), raw, "U+{cp:04X}");
                }
            }
        }
        // The ones that carry the attack, by name, so a table that drifted
        // into covering the wrong block still fails here.
        for c in [
            '\u{202e}', '\u{2066}', '\u{2069}', '\u{200b}', '\u{200e}', '\u{feff}', '\u{ad}',
        ] {
            assert!(is_format(c), "U+{:04X}", c as u32);
        }
        // The blank-rendering scalars belong to `collapse_blanks`, not
        // here — they are legitimate text in a detail, just not padding in
        // a headline.
        for c in ['\u{2800}', '\u{3164}', '\u{115f}', '\u{1160}', '\u{ffa0}'] {
            assert!(!is_format(c), "U+{:04X}", c as u32);
        }
        // Ordinary non-ASCII text, spelled the way a user would have it.
        for raw in ["/Users/münchen/日本語/bin/zsh", "café ✓ 🐦 — ok"] {
            assert_eq!(redact(raw), raw);
        }
        // Sorted, non-empty and disjoint, so `is_format`'s linear scan is a
        // table anyone can read against `UnicodeData.txt` rather than a pile.
        for pair in FORMAT_RANGES.windows(2) {
            assert!(pair[0].0 <= pair[0].1, "{pair:?}");
            assert!(pair[0].1 < pair[1].0, "{pair:?}");
        }

        // End to end, with the review's own repro: the reversal must not
        // survive into any of the three views.
        let report = evaluate(&Inputs {
            shell_path: Some("\u{202e}bat detceleS ]✗[".into()),
            shell_usable: false,
            env_shell_integration: None,
            env_resources_dir: None,
            ..healthy()
        });
        for text in [
            summary_text(&report),
            verbose_text(&report),
            render_json(&report).unwrap(),
        ] {
            assert!(
                !text.contains('\u{202e}'),
                "a raw RLO reached the output:\n{text}"
            );
            assert!(text.contains("\\u{202e}"), "{text}");
        }
    }

    #[test]
    fn long_strings_are_capped_in_characters_with_the_true_length() {
        let raw: String = "é".repeat(500);
        let out = redact(&raw);
        assert!(out.contains("…(500 chars)"), "{out}");
        assert!(out.chars().count() < 200, "cap not applied: {out}");
    }

    #[test]
    fn metadata_values_are_allowlisted_and_keys_are_capped() {
        let mut metadata = BTreeMap::new();
        metadata.insert("model".to_string(), "claude-opus-5".to_string());
        metadata.insert("session_title".to_string(), "fix the \x07 bug".to_string());
        metadata.insert("x".repeat(200), "whatever".to_string());
        let out = redact_metadata(&metadata);
        assert!(out.contains("model=claude-opus-5"), "{out}");
        assert!(
            out.contains("session_title=<13 chars>"),
            "an unlisted key must print its length only: {out}"
        );
        assert!(!out.contains("fix the"), "{out}");
        assert!(out.contains("…(200 chars)"), "keys are capped too: {out}");
        assert_eq!(redact_metadata(&BTreeMap::new()), "none");
    }

    #[test]
    fn session_ids_are_fingerprinted_not_printed() {
        let id = "3f2a5c1e-0000-4444-8888-abcdefabcdef";
        let fp = fingerprint(id);
        assert!(!fp.contains(id), "{fp}");
        assert!(fp.starts_with("fp:"), "{fp}");
        assert!(fp.contains("(36 chars)"), "{fp}");
        assert_eq!(fingerprint(id), fp, "the fingerprint must be stable");
        assert_ne!(fingerprint("other"), fp);
        assert_eq!(fingerprint(""), "<none>");
    }

    /// §3.9 says *every* externally sourced string, not just the agent
    /// fields. A newline in a socket path, an `identify` field or the
    /// settings path used to render as a whole extra report line — a
    /// convincing forged verdict in output a user pastes into an issue.
    #[test]
    fn every_external_string_is_escaped_not_just_the_agent_fields() {
        // Shaped like a real `-v` row — `{:<7}` status column, two-space
        // indent — so the assertion below is about escaping rather than
        // about the forgery missing by a space.
        const FORGE: &str = "\n  fail    injected.fake";
        // …and the same trick aimed at the default view, whose rows are
        // `[✗] Title  headline`. A section line is the *only* thing a
        // summary reader sees, so a forged one is worth more to an
        // attacker than a forged check row — and the row above would not
        // catch it.
        const SECTION_FORGE: &str = "\n[✗] Roost UI            everything is broken";

        let hostile = |forge: &str| {
            let sock = PathBuf::from(format!("/tmp/nope{forge}"));
            Inputs {
                target_origin: TargetOrigin::SocketFlag,
                target_candidates: vec![sock.clone()],
                target: Ok(sock.clone()),
                sockets: vec![SocketProbe {
                    path: sock.clone(),
                    profile: None,
                    outcome: SocketOutcome::Missing,
                }],
                identify: Ok(IdentifyResult {
                    socket_path: format!("/tmp/s{forge}"),
                    app_label: format!("Roost{forge}"),
                    app_id: format!("ai.stridelabs{forge}"),
                    ui_version: format!("9.9.9{forge}"),
                    ..identify(7, "9.9.9")
                }),
                resources_script: Some(PathBuf::from(format!("/opt/roost/roost.zsh{forge}"))),
                resources_script_readable: false,
                claude_settings_path: Some(PathBuf::from(format!("/home/u/settings.json{forge}"))),
                claude_settings: SettingsProbe::Parsed,
                claude_version: SubprocessOutcome::Output("2.1.220".into()),
                claude_hook_commands: {
                    let mut c = hook_commands(&["/usr/local/bin/roostctl"]);
                    c[0].exe_canonical = Some(PathBuf::from(format!("/other/roostctl{forge}")));
                    c
                },
                claude_hook_events: CLAUDE_HOOK_EVENTS.iter().map(|e| e.to_string()).collect(),
                ..healthy()
            }
        };

        for forge in [FORGE, SECTION_FORGE] {
            let report = evaluate(&hostile(forge));
            let summary = summary_text(&report);
            for text in [&verbose_text(&report), &summary] {
                assert!(
                    !text.contains(forge),
                    "a forged report line survived the renderer:\n{text}"
                );
                for line in text.lines() {
                    assert!(
                        !line.trim_start().starts_with(forge.trim_start()),
                        "forged line: {line:?}"
                    );
                }
            }
            // A summary reader sees section lines and nothing else, so
            // the count of them is the forgery budget.
            let rows = summary_rows(&summary);
            assert_eq!(rows.len(), report.sections.len());
            // The headline is a copy of an already-escaped detail, so it
            // inherits the escaping rather than adding a surface.
            for s in &report.sections {
                assert!(!s.headline.contains('\n'), "{}: {}", s.id, s.headline);
                assert!(!s.headline.contains('\x1b'), "{}: {}", s.id, s.headline);
            }
            // …and the summary's column budget cuts that escaped copy, so
            // the escaping has to survive the cut too: a clip can leave a
            // partial `\u{00`, which is still text, but it can never
            // reassemble a control character or a second line. This
            // fixture is long enough that the cut is actually exercised.
            assert!(
                rows.iter().any(|r| r.ends_with('…')),
                "the forged fixture no longer exercises the clip:\n{summary}"
            );
            for row in &rows {
                assert!(row.chars().count() <= SUMMARY_W, "{row:?}");
                assert!(!row.contains('\x1b'), "{row:?}");
            }
            // The value is still shown — escaped, not dropped.
            for id in [
                "ui.target",
                "ui.socket",
                "ui.identify",
                "claude.settings",
                "shell.resources",
                "claude.hook_command",
            ] {
                assert!(find(&report, id).detail.contains("\\n"), "{id}");
            }
        }
    }

    /// The third vector in the same battery, and the one the two above
    /// cannot reach: a blank cell is **not** a control character, so
    /// `escape_controls` passes a run of them through untouched and the
    /// row-counting assertions see one logical line. A long enough run
    /// positions the text after it at the exact column a narrow terminal
    /// wraps a `SUMMARY_W` row — codex's repro aims `[✗] Selected tab` at
    /// column 80 — and the wrapped remainder reads as a real section line.
    #[test]
    fn a_padded_headline_cannot_be_aimed_at_a_terminals_wrap_column() {
        // The blank definition first, because the escaping and the
        // collapse have to compose in exactly one direction.
        assert_eq!(collapse_blanks("  a   b  "), "a b");
        // Unicode `White_Space`, not just `' '`: NBSP, the en quad and the
        // ideographic space all pad, and U+2028 is a line separator that
        // `char::is_control` does not cover.
        assert_eq!(collapse_blanks("a\u{a0}\u{2003}\u{3000}b"), "a b");
        assert_eq!(collapse_blanks("a\u{2028}\u{2029}b"), "a b");
        // …and *wider* than `White_Space`, which is what the first round of
        // this fix got wrong: U+2800 BRAILLE PATTERN BLANK is `So` and the
        // Hangul fillers are `Lo`, so neither `char::is_whitespace` nor
        // `char::is_control` sees them, yet every terminal font draws them
        // as an empty cell — a 1:1 substitute for the space.
        assert_eq!(collapse_blanks("a\u{2800}\u{2800}\u{2800}b"), "a b");
        assert_eq!(collapse_blanks("a\u{3164}\u{115f}\u{1160}\u{ffa0}b"), "a b");
        assert_eq!(collapse_blanks("a\u{180e}b"), "a b");
        // …and what the escaping already rewrote is *text*: `\t` is a
        // backslash and a `t`, neither of which is blank, so a collapse
        // cannot eat an escape or fuse it to its neighbour.
        assert_eq!(collapse_blanks(&escape_controls("a\tb")), "a\\tb");
        assert_eq!(collapse_blanks(&escape_controls("a\n\nb")), "a\\n\\nb");

        // Codex's repro in shape: outside a Roost tab an unusable `$SHELL`
        // is `shell.login`'s skipped detail, which is the section headline.
        const FORGED: &str = "[✗] Selected tab";
        let hostile = |pad: char, n: usize| Inputs {
            shell_path: Some(format!("{}{FORGED}", String::from(pad).repeat(n))),
            shell_usable: false,
            env_shell_integration: None,
            env_resources_dir: None,
            ..healthy()
        };
        let row_for = |pad: char, n: usize| {
            let report = evaluate(&hostile(pad, n));
            let summary = summary_text(&report);
            summary_rows(&summary)
                .into_iter()
                .find(|r| r.contains("$SHELL="))
                .unwrap_or_else(|| panic!("no shell row in:\n{summary}"))
                .to_string()
        };

        // Every scalar that pads, not just the ones Unicode calls
        // whitespace. U+180E is deliberately absent: it is `Cf`, so
        // `escape_controls` rewrites it to an 8-character escape before a
        // headline exists (`format_characters_are_escaped` covers it), and
        // 49 of those trip `redact`'s cap — a different neutralization, not
        // this one.
        for pad in [
            ' ', '\u{a0}', '\u{2003}', '\u{3000}', '\u{2800}', '\u{3164}', '\u{115f}', '\u{1160}',
            '\u{ffa0}',
        ] {
            let label = format!("pad U+{:04X}", pad as u32);
            // The whole attack is aim, so the property that kills it is
            // that the padding width no longer moves anything: 49 pads
            // (aimed at an 80-column wrap) and 89 (aimed at 120) render one
            // same row. Both stay under `MAX_DISPLAY_CHARS`, or `redact`
            // would truncate one of them and the rows would differ for an
            // unrelated reason.
            assert_eq!(row_for(pad, 49), row_for(pad, 89), "{label}");

            let report = evaluate(&hostile(pad, 49));
            let row = row_for(pad, 49);
            let (title_w, _) = summary_columns(&report);
            let headline: Vec<char> = row.chars().skip(MARKER_W + title_w).collect();
            assert!(
                !headline
                    .windows(2)
                    .any(|w| w.iter().all(|c| renders_blank(*c))),
                "{label}: a padding run survived past the title column: {headline:?}"
            );
            assert!(row.chars().count() <= SUMMARY_W, "{label}: {row:?}");

            // And with the padding gone the forged marker sits at a column
            // fixed by the real prose, which no common terminal wraps at.
            let at = row.find(FORGED).unwrap_or_else(|| {
                panic!("{label}: the fixture no longer carries the forgery: {row:?}")
            });
            let col = row[..at].chars().count();
            for w in [40, 60, 80, 100, 120, 132] {
                assert_ne!(col % w, 0, "{label}: forgery aimed at a {w}-column wrap");
            }

            // `detail` is untouched, which is the point of collapsing the
            // headline only: `-v` and `--json` still report what `$SHELL`
            // actually is, padding and all.
            let raw_pad = String::from(pad).repeat(49);
            assert!(
                find(&report, "shell.login").detail.contains(&raw_pad),
                "{label}"
            );
            assert!(verbose_text(&report).contains(&raw_pad), "{label}");
            let json = render_json(&report).unwrap();
            assert!(
                serde_json::from_str::<serde_json::Value>(&json).unwrap()["sections"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|s| s["checks"].as_array().unwrap())
                    .any(|c| c["id"] == "shell.login"
                        && c["detail"].as_str().unwrap().contains(&raw_pad)),
                "{label}"
            );
        }
    }

    /// F8: an unavailable `$HOME` means the settings location is unknown,
    /// never a CWD-relative path that could diagnose an unrelated file.
    #[test]
    fn an_unavailable_home_leaves_the_settings_location_unknown() {
        let inputs = Inputs {
            claude_settings_path: None,
            claude_settings: SettingsProbe::LocationUnknown,
            claude_version: SubprocessOutcome::Output("2.1.220".into()),
            ..healthy()
        };
        let report = evaluate(&inputs);
        let c = find(&report, "claude.settings");
        assert_eq!(c.status, Some(Status::Skipped));
        assert!(c.detail.contains("$HOME"), "{}", c.detail);
        assert!(!c.detail.contains(".config/roost"), "{}", c.detail);
        for id in ["claude.hook_events", "claude.hook_command"] {
            assert_status(&report, id, Status::Skipped);
        }
        assert_eq!(report.exit_code(), 0, "{}", verbose_text(&report));
    }

    #[test]
    fn an_agent_supplied_ownership_record_is_redacted_in_the_report() {
        let mut metadata = BTreeMap::new();
        metadata.insert("session_title".into(), "secret prompt text".into());
        let inputs = Inputs {
            tab_list: Ok(tab_list(&[Tab {
                ownership: Some(Ownership {
                    source: "claude".into(),
                    session_id: "sess-abcdef123456".into(),
                    last_event_at: 1_700_000_040,
                    detail: "permission\x1b[31m_prompt".into(),
                    metadata,
                }),
                ..tab(7)
            }])),
            ..healthy()
        };
        let detail = find(&evaluate(&inputs), "tab.ownership").detail.clone();
        assert!(!detail.contains("sess-abcdef123456"), "{detail}");
        assert!(!detail.contains("secret prompt text"), "{detail}");
        assert!(!detail.contains('\x1b'), "{detail}");
        assert!(detail.contains("60s ago"), "{detail}");
    }

    // ------------------------------------------------------- exit + render

    #[test]
    fn the_exit_code_is_one_iff_some_check_fails() {
        assert_eq!(evaluate(&healthy()).exit_code(), 0);
        let failing = Inputs {
            sockets: vec![SocketProbe {
                path: PathBuf::from("/tmp/roost.sock"),
                profile: None,
                outcome: SocketOutcome::Stale,
            }],
            ..healthy()
        };
        assert_eq!(evaluate(&failing).exit_code(), 1);
        // warn alone never changes it.
        let warning = Inputs {
            identify: Ok(identify(7, "0.0.1")),
            ..healthy()
        };
        let report = evaluate(&warning);
        assert!(report.summary().warn > 0);
        assert_eq!(report.exit_code(), 0);
    }

    /// Count the four statuses without going near [`Report::summary`],
    /// so swapping two of its counters is a red test rather than a
    /// tautology. Run over the whole battery: a report with zero `warn`
    /// and zero `fail` cannot tell those two columns apart.
    fn count_statuses(report: &Report) -> Summary {
        let mut s = Summary::default();
        for section in &report.sections {
            for c in &section.checks {
                match c.status {
                    Some(Status::Ok) => s.ok += 1,
                    Some(Status::Warn) => s.warn += 1,
                    Some(Status::Fail) => s.fail += 1,
                    Some(Status::Skipped) => s.skipped += 1,
                    None => s.facts += 1,
                }
            }
        }
        s
    }

    #[test]
    fn the_summary_counts_each_status_independently() {
        let mut seen = Summary::default();
        for inputs in doc_url_battery() {
            let report = evaluate(&inputs);
            let counted = count_statuses(&report);
            assert_eq!(report.summary(), counted, "{}", verbose_text(&report));
            assert_eq!(
                counted.ok + counted.warn + counted.fail + counted.skipped + counted.facts,
                report
                    .sections
                    .iter()
                    .map(|x| x.checks.len())
                    .sum::<usize>()
            );
            seen.ok += counted.ok;
            seen.warn += counted.warn;
            seen.fail += counted.fail;
            seen.skipped += counted.skipped;
            seen.facts += counted.facts;
        }
        // A battery that never produced a warn or a fail would let the
        // two columns be swapped undetected.
        for column in [seen.ok, seen.warn, seen.fail, seen.skipped, seen.facts] {
            assert!(column > 0, "{seen:?} leaves a status column unexercised");
        }
    }

    /// §3.13's identity: the five columns partition the whole inventory,
    /// so a state the summary forgets to count shows up as a hole rather
    /// than as a quietly missing row.
    #[test]
    fn the_summary_partitions_the_whole_inventory() {
        for inputs in doc_url_battery() {
            let s = evaluate(&inputs).summary();
            assert_eq!(
                s.ok + s.warn + s.fail + s.skipped + s.facts,
                CHECK_COUNT,
                "{s:?}"
            );
        }
    }

    /// The two renderers read the same `Report`, so redaction cannot
    /// differ between them — this pins that, plus the summary.
    #[test]
    fn the_renderers_agree() {
        let mut metadata = BTreeMap::new();
        metadata.insert("session_title".into(), "private".into());
        let inputs = Inputs {
            identify: Ok(identify(7, "0.0.1")),
            tab_list: Ok(tab_list(&[Tab {
                shell_state: ShellState::Unknown,
                ownership: Some(Ownership {
                    source: "claude".into(),
                    session_id: "sess-xyz".into(),
                    last_event_at: 1_700_000_000,
                    detail: "bad\nnews".into(),
                    metadata,
                }),
                ..tab(7)
            }])),
            ..healthy()
        };
        let report = evaluate(&inputs);
        let text = verbose_text(&report);
        let json: serde_json::Value = serde_json::from_str(&render_json(&report).unwrap()).unwrap();

        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        // Counted from the checks, not from `summary()`, so the JSON
        // cannot agree with a wrong summary and still pass.
        let s = count_statuses(&report);
        assert_eq!(json["summary"]["ok"], s.ok);
        assert_eq!(json["summary"]["warn"], s.warn);
        assert_eq!(json["summary"]["fail"], s.fail);
        assert_eq!(json["summary"]["skipped"], s.skipped);
        assert_eq!(json["summary"]["facts"], s.facts);
        assert!(s.warn > 0 && s.fail > 0, "{s:?} does not separate the two");
        // The counts are a `--json`-only surface now (§3.10): the text
        // footer names the offending ids instead, which is what a user
        // can act on.
        assert!(!text.contains("facts"), "{text}");

        // Both axes survive the envelope's `flatten`, and a fact's
        // `status` key is present-and-null rather than dropped.
        let entry = |id: &str| {
            json["sections"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|s| s["checks"].as_array().unwrap())
                .find(|c| c["id"] == id)
                .unwrap_or_else(|| panic!("no `{id}` in the json"))
                .clone()
        };
        let fact = entry("shell.current");
        assert_eq!(fact["kind"], "observation");
        assert_eq!(fact.get("status"), Some(&serde_json::Value::Null), "{fact}");
        assert_eq!(entry("ui.identify")["kind"], "check");

        // …and in text that fact prints no status word, with the id
        // column still lined up under the scored rows above it.
        let observed_row = text.lines().find(|l| l.contains("shell.current")).unwrap();
        let scored_row = text.lines().find(|l| l.contains("ui.identify")).unwrap();
        assert!(
            observed_row.trim_start().starts_with("shell.current"),
            "{observed_row:?}"
        );
        assert_eq!(
            observed_row.find("shell.current"),
            scored_row.find("ui.identify"),
            "the id column must line up whether or not there is a status word"
        );

        // Every check's redacted detail appears verbatim in both.
        for section in &report.sections {
            for c in &section.checks {
                assert!(
                    text.contains(&c.detail),
                    "text missing {}: {}",
                    c.id,
                    c.detail
                );
            }
        }
        let json_text = serde_json::to_string(&json).unwrap();
        for needle in ["sess-xyz", "private"] {
            assert!(!text.contains(needle), "text leaked {needle}");
            assert!(!json_text.contains(needle), "json leaked {needle}");
        }
        assert!(json_text.contains("fp:"), "json must carry the fingerprint");
        assert!(text.contains("fp:"), "text must carry the fingerprint");
    }

    /// `--json` must be valid JSON carrying `schema_version` on *every*
    /// degraded path (AC 9).
    #[test]
    fn json_is_valid_on_every_degraded_path() {
        let degraded = [
            Inputs::default(),
            Inputs {
                target: Err(TargetFailure::Ambiguous(vec!["mac".into(), "iced".into()])),
                ..Inputs::default()
            },
            Inputs {
                target: Err(TargetFailure::UnknownProfile("bogus".into())),
                ..Inputs::default()
            },
            Inputs {
                identify: Err(IdentifyFailure::Timeout),
                tab_list: Err("timed out".into()),
                ..healthy()
            },
            Inputs {
                tab_list: Ok(legacy_tab_list(&[7])),
                ..healthy()
            },
        ];
        for inputs in degraded {
            let report = evaluate(&inputs);
            let value: serde_json::Value =
                serde_json::from_str(&render_json(&report).unwrap()).expect("valid json");
            assert_eq!(value["schema_version"], SCHEMA_VERSION);
            assert!(value["summary"].is_object());
            assert!(!verbose_text(&report).is_empty());
        }
    }

    // ------------------------------------------------- roll-up + headline

    /// §3.9's pinned headline entry per section — the one whose detail
    /// speaks for the section when nothing in it is adverse.
    const HEADLINE_ENTRIES: &[(&str, &str)] = &[
        ("env", "env.tab_id"),
        ("ui", "ui.identify"),
        ("shell", "shell.marks_observed"),
        ("tab", "tab.derived"),
        ("claude", "claude.hook_events"),
    ];

    fn section_of<'a>(report: &'a Report, id: &str) -> &'a Section {
        report
            .sections
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no section `{id}` in the report"))
    }

    /// Everything green, including `claude.observed`: a configured Claude
    /// whose hooks have actually claimed doctor's own tab. `configured()`
    /// alone leaves that one `skipped`, which would roll the whole
    /// section up to `–`.
    fn fully_wired() -> Inputs {
        Inputs {
            tab_list: Ok(tab_list(&[Tab {
                ownership: Some(Ownership {
                    source: "claude".into(),
                    session_id: "s-1".into(),
                    last_event_at: 1_700_000_040,
                    ..Ownership::default()
                }),
                ..tab(7)
            }])),
            ..configured(hook_commands(&["/usr/local/bin/roostctl"]))
        }
    }

    /// A shell that is fine but has no Roost PTY around it: `shell.login`
    /// stays `ok` while the six in-tab checks skip, so the section's
    /// first skipped entry is **not** its headline entry.
    fn usable_shell_outside_a_tab() -> Inputs {
        Inputs {
            shell_path: Some("/bin/zsh".into()),
            shell_usable: true,
            shell_version: SubprocessOutcome::Output("zsh 5.9 (arm-apple-darwin24.0)".into()),
            ..Inputs::default()
        }
    }

    /// §3.8's roll-up and §3.9's headline, for every section in every
    /// state it can reach — not just the healthy one. The `id` on the
    /// right is the entry whose `detail` the headline must copy: the
    /// worst entry for an adverse roll-up (ties by position), the pinned
    /// headline entry otherwise.
    #[test]
    fn a_section_rolls_up_to_its_worst_entry_and_headlines_it() {
        let cases: Vec<(&str, Inputs, &str, Option<Status>, &str)> = vec![
            ("env ok", healthy(), "env", Some(Status::Ok), "env.tab_id"),
            (
                "env skipped",
                Inputs::default(),
                "env",
                Some(Status::Skipped),
                "env.tab_id",
            ),
            (
                "env fail",
                Inputs {
                    env_shell_integration: Some("1".into()),
                    ..Inputs::default()
                },
                "env",
                Some(Status::Fail),
                "env.tab_id",
            ),
            ("ui ok", healthy(), "ui", Some(Status::Ok), "ui.identify"),
            (
                "ui warn",
                Inputs {
                    identify: Ok(identify(7, "0.0.1")),
                    ..healthy()
                },
                "ui",
                Some(Status::Warn),
                "ui.version",
            ),
            (
                "ui skipped",
                Inputs {
                    tab_list: Ok(tab_list(&[])),
                    identify: Ok(identify(0, env!("CARGO_PKG_VERSION"))),
                    ..healthy()
                },
                "ui",
                Some(Status::Skipped),
                "ui.agent_model",
            ),
            (
                "ui fail",
                Inputs::default(),
                "ui",
                Some(Status::Fail),
                "ui.target",
            ),
            (
                "shell ok",
                healthy(),
                "shell",
                Some(Status::Ok),
                "shell.marks_observed",
            ),
            (
                "shell warn",
                Inputs {
                    shell_path: Some("/opt/homebrew/bin/fish".into()),
                    shell_version: SubprocessOutcome::Output("fish, version 3.7.1".into()),
                    resources_script: None,
                    resources_script_readable: false,
                    ..healthy()
                },
                "shell",
                Some(Status::Warn),
                "shell.marks_capability",
            ),
            (
                "shell skipped",
                usable_shell_outside_a_tab(),
                "shell",
                Some(Status::Skipped),
                "shell.integration",
            ),
            (
                "shell fail",
                Inputs {
                    env_shell_integration: None,
                    ..healthy()
                },
                "shell",
                Some(Status::Fail),
                "shell.integration",
            ),
            ("tab observational", healthy(), "tab", None, "tab.derived"),
            (
                "tab skipped",
                Inputs {
                    tab_list: Ok(legacy_tab_list(&[7])),
                    ..healthy()
                },
                "tab",
                Some(Status::Skipped),
                "tab.shell_state",
            ),
            (
                "tab fail",
                Inputs {
                    explicit_tab: Some(999),
                    ..healthy()
                },
                "tab",
                Some(Status::Fail),
                "tab.selection",
            ),
            (
                "claude ok",
                fully_wired(),
                "claude",
                Some(Status::Ok),
                "claude.hook_events",
            ),
            (
                "claude warn",
                configured(hook_commands(&["/opt/other/roostctl"])),
                "claude",
                Some(Status::Warn),
                "claude.hook_command",
            ),
            (
                "claude skipped",
                healthy(),
                "claude",
                Some(Status::Skipped),
                "claude.binary",
            ),
            (
                "claude fail",
                configured({
                    let mut c = hook_commands(&["/usr/local/bin/roostctl"]);
                    c[0].exe_executable = false;
                    c[0].exe_canonical = None;
                    c
                }),
                "claude",
                Some(Status::Fail),
                "claude.hook_command",
            ),
        ];

        let mut covered: Vec<(&str, Option<Status>)> = Vec::new();
        for (label, inputs, section_id, want_status, want_headline) in cases {
            let report = evaluate(&inputs);
            let s = section_of(&report, section_id);
            assert_eq!(s.status, want_status, "{label}: {}", verbose_text(&report));
            assert_eq!(
                s.headline,
                find(&report, want_headline).detail,
                "{label}: the headline must copy `{want_headline}`"
            );
            covered.push((section_id, want_status));
        }
        // Every section reached all three of §3.8's outcomes: adverse
        // (fail or warn), skipped, and the non-adverse marker.
        for (section_id, headline_id) in HEADLINE_ENTRIES {
            // `tab` is the observational section, so its non-adverse
            // marker is `None`, not a tick (§3.8).
            let non_adverse = (*section_id != "tab").then_some(Status::Ok);
            for want in [Some(Status::Skipped), non_adverse] {
                assert!(
                    covered.contains(&(*section_id, want)),
                    "`{section_id}` (headline `{headline_id}`) has no {want:?} case"
                );
            }
            assert!(
                covered.contains(&(*section_id, Some(Status::Fail)))
                    || covered.contains(&(*section_id, Some(Status::Warn))),
                "`{section_id}` has no adverse case"
            );
        }
    }

    /// The anti-synthesis guarantee (§3.9), swept over every fixture the
    /// suite has: a headline is always **some entry's** `detail`, so it
    /// inherits that entry's redaction and adds no second source of
    /// truth. The pick is re-derived here rather than read off the
    /// production fold — the same hand-rolled-duplicate pattern
    /// `count_statuses` uses, so the rule has to be changed twice.
    #[test]
    fn every_headline_copies_an_entrys_detail_and_never_invents_prose() {
        for inputs in doc_url_battery() {
            let report = evaluate(&inputs);
            assert_eq!(report.sections.len(), HEADLINE_ENTRIES.len());
            for (s, (section_id, headline_id)) in report.sections.iter().zip(HEADLINE_ENTRIES) {
                assert_eq!(s.id, *section_id);
                let want = match s.status {
                    Some(Status::Fail | Status::Warn | Status::Skipped) => s
                        .checks
                        .iter()
                        .find(|c| c.status == s.status)
                        .expect("an adverse roll-up names an adverse entry"),
                    Some(Status::Ok) | None => s
                        .checks
                        .iter()
                        .find(|c| c.id == *headline_id)
                        .expect("the pinned headline entry"),
                };
                assert_eq!(s.headline, want.detail, "section `{}`", s.id);
                assert!(
                    s.checks.iter().any(|c| c.detail == s.headline),
                    "section `{}` headline is not any entry's detail",
                    s.id
                );
            }
        }
    }

    /// §3.8's one carve-out, both ways. `tab` grades nothing, so a
    /// healthy tab section is `•` rather than a green tick — but the
    /// carve-out is a *default*, not a mute: against a server predating
    /// the agent model the same section rolls up `–`.
    #[test]
    fn the_tab_section_observes_rather_than_grades() {
        let healthy_report = evaluate(&healthy());
        let tab = section_of(&healthy_report, "tab");
        assert_status(&healthy_report, "tab.selection", Status::Ok);
        assert_eq!(tab.status, None, "an `ok` selection must not tick");
        assert_eq!(glyph(tab.status), "•");
        assert!(summary_text(&healthy_report).contains("[•] Selected tab"));

        let old = evaluate(&Inputs {
            tab_list: Ok(legacy_tab_list(&[7])),
            ..healthy()
        });
        let tab = section_of(&old, "tab");
        assert_status(&old, "tab.selection", Status::Ok);
        assert_eq!(tab.status, Some(Status::Skipped));
        assert_eq!(glyph(tab.status), "–");
        assert!(summary_text(&old).contains("[–] Selected tab"));

        // Every other section still ticks when it is clean, so `•` is a
        // property of `tab`, not of the roll-up giving up everywhere.
        for s in &healthy_report.sections {
            if s.id != "tab" && s.checks.iter().all(|c| c.status != Some(Status::Skipped)) {
                assert_eq!(s.status, Some(Status::Ok), "{}", s.id);
            }
        }
    }

    // -------------------------------------------------------- the two views

    /// The default view is one line per section and the footer — no
    /// per-check rows — while `-v` keeps the full report. Both name the
    /// same issues (AC 12).
    #[test]
    fn the_summary_view_is_one_line_per_section_and_v_is_the_full_report() {
        let report = evaluate(&Inputs::default());
        let summary = summary_text(&report);
        let verbose = verbose_text(&report);

        assert!(summary.contains("run `roostctl doctor -v` for the full report"));
        for s in &report.sections {
            assert!(
                summary.contains(&format!("[{}] {}", glyph(s.status), s.title)),
                "no `{}` row in:\n{summary}",
                s.title
            );
        }
        // The section rows carry headlines, not check ids.
        let rows = summary_rows(&summary);
        assert_eq!(rows.len(), report.sections.len());
        for c in report.checks() {
            assert!(
                verbose.contains(c.id),
                "`-v` dropped `{}`:\n{verbose}",
                c.id
            );
        }
        assert!(
            !rows.iter().any(|r| r.contains("env.socket")),
            "the summary must not list per-check rows: {rows:?}"
        );
        // Same issues in both, named by id.
        let issues: Vec<&str> = report
            .checks()
            .filter(|c| matches!(c.status, Some(Status::Fail | Status::Warn)))
            .map(|c| c.id)
            .collect();
        assert!(!issues.is_empty());
        for id in issues {
            assert!(summary.contains(id), "summary footer lost `{id}`");
            assert!(verbose.contains(id));
        }
    }

    /// The headline column, read the way the renderer reads it.
    fn summary_headline(report: &Report, row: &str) -> String {
        let (title_w, _) = summary_columns(report);
        row.chars().skip(MARKER_W + title_w).collect()
    }

    fn summary_rows(text: &str) -> Vec<&str> {
        text.lines().filter(|l| l.starts_with('[')).collect()
    }

    /// "One line per section" against a real overlong detail:
    /// `claude.hook_command`'s warn names all six events and two absolute
    /// paths — past 200 characters in the field, which wraps to three or
    /// four lines in an 80-column terminal and destroys the scan the
    /// rolled-up view exists to provide. `-v` and `--json` keep the whole
    /// sentence, and the header line names `-v`.
    #[test]
    fn an_overlong_headline_is_clipped_in_the_summary_but_whole_in_v_and_json() {
        let report = evaluate(&configured(hook_commands(&["/opt/other/roostctl"])));
        let claude = section_of(&report, "claude");
        let (_, headline_w) = summary_columns(&report);
        assert!(
            claude.headline.chars().count() > headline_w,
            "the fixture no longer overflows the budget: {}",
            claude.headline
        );

        let summary = summary_text(&report);
        let rows = summary_rows(&summary);
        assert_eq!(rows.len(), report.sections.len(), "{summary}");
        let row = rows
            .iter()
            .find(|r| r.starts_with("[!] Claude Code"))
            .unwrap_or_else(|| panic!("no clipped claude row in:\n{summary}"));
        assert!(row.ends_with('…'), "a clip must be visible: {row:?}");
        assert_eq!(row.chars().count(), SUMMARY_W, "{row:?}");
        // What survived is a prefix of the real thing, not new prose.
        let shown = summary_headline(&report, row);
        assert_eq!(shown.chars().count(), headline_w, "{shown:?}");
        assert!(
            claude.headline.starts_with(shown.trim_end_matches('…')),
            "{shown:?}"
        );
        // No row overruns the budget, clipped or not.
        for r in &rows {
            assert!(r.chars().count() <= SUMMARY_W, "{r:?}");
        }

        // The full detail is one keystroke away, and machine-readable.
        assert!(
            verbose_text(&report).contains(&claude.headline),
            "`-v` must carry the whole sentence"
        );
        let json: serde_json::Value = serde_json::from_str(&render_json(&report).unwrap()).unwrap();
        let section = json["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "claude")
            .unwrap()
            .clone();
        assert_eq!(section["headline"], claude.headline);
        assert_eq!(
            section["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == "claude.hook_command")
                .unwrap()["detail"],
            claude.headline
        );
    }

    /// The cap is inclusive, and the marker is paid for out of the budget
    /// rather than added past it — the two off-by-ones a column budget
    /// invites.
    #[test]
    fn the_headline_cap_is_inclusive_and_the_marker_fits_inside_it() {
        const CAP: usize = 10;
        for n in [0, 1, CAP - 1, CAP] {
            let fits = "x".repeat(n);
            assert_eq!(ellipsize(&fits, CAP), fits, "{n} characters must survive");
        }
        let over = "x".repeat(CAP + 1);
        assert_eq!(ellipsize(&over, CAP), format!("{}…", "x".repeat(CAP - 1)));
        assert_eq!(ellipsize(&over, CAP).chars().count(), CAP);

        // A budget with no room for the marker emits nothing: pushing `…`
        // anyway is how a row promised at `SUMMARY_W` comes out one
        // character wider. Cap 1 spends its single column on the marker.
        assert_eq!(ellipsize("anything", 0), "");
        assert_eq!(ellipsize("", 0), "");
        assert_eq!(ellipsize("ab", 1), "…");
        assert_eq!(ellipsize("a", 1), "a");

        // End to end, at the width that actually produces a zero budget:
        // unreachable while the section titles are constants, so the
        // contract has to hold by construction rather than by luck.
        let widest = "T".repeat(SUMMARY_W - MARKER_W - TITLE_GAP);
        let mut report = evaluate(&healthy());
        report.sections[0].title = Box::leak(widest.into_boxed_str());
        assert_eq!(summary_columns(&report).1, 0, "the fixture must starve it");
        for row in summary_rows(&summary_text(&report)) {
            assert!(row.chars().count() <= SUMMARY_W, "{row:?}");
        }
    }

    /// A byte cut would split a `—`, a `→` or an emoji mid-scalar, and
    /// details carry all three. Asserted on the cut itself and then
    /// end-to-end, with the multibyte run early enough in the detail to
    /// land under the budget rather than past it.
    #[test]
    fn a_multibyte_headline_clips_on_a_character_boundary() {
        const CAP: usize = 20;
        let wide = "—→🎉".repeat(20);

        let cut = ellipsize(&wide, CAP);
        // Characters, not display columns: the budget counts scalars, so
        // a double-width emoji spends exactly one of them.
        assert_eq!(cut.chars().count(), CAP, "{cut:?}");
        assert!(cut.ends_with('…'), "{cut:?}");
        assert!(wide.starts_with(cut.trim_end_matches('…')), "{cut:?}");

        let sock = PathBuf::from(format!("/tmp/{wide}.sock"));
        let report = evaluate(&Inputs {
            target: Err(TargetFailure::NoLiveTarget(vec![sock.clone()])),
            target_candidates: vec![sock.clone()],
            sockets: vec![SocketProbe {
                path: sock,
                profile: None,
                outcome: SocketOutcome::Missing,
            }],
            ..Inputs::default()
        });
        let ui = section_of(&report, "ui");
        let (_, headline_w) = summary_columns(&report);
        let summary = summary_text(&report);
        let row = summary_rows(&summary)
            .into_iter()
            .find(|r| r.starts_with("[✗] Roost UI"))
            .unwrap_or_else(|| panic!("no ui row in:\n{summary}"));
        let shown = summary_headline(&report, row);
        assert_eq!(row.chars().count(), SUMMARY_W, "{row:?}");
        assert_eq!(shown.chars().count(), headline_w, "{shown:?}");
        assert!(shown.ends_with('…'), "{shown:?}");
        // The cut landed inside the multibyte run…
        assert!(shown.contains('—') && shown.contains('🎉'), "{shown:?}");
        // …and still produced a whole-character prefix of the untouched
        // headline, one character short of the budget to pay for the `…`.
        let head = shown.trim_end_matches('…');
        assert!(ui.headline.starts_with(head), "{head:?}");
        assert_eq!(head.chars().count(), headline_w - 1);
    }

    /// The footer, both ways. It keeps the exit code and the exit-codes
    /// link the pre-summary footer carried — that link is the only
    /// in-band explanation for doctor exiting 1 when no UI is running —
    /// and it names **check ids**, because one section can hold two
    /// different failures.
    #[test]
    fn the_footer_names_the_issues_and_keeps_the_exit_code_and_link() {
        let clean = evaluate(&fully_wired());
        assert_eq!(clean.exit_code(), 0);
        for text in [summary_text(&clean), verbose_text(&clean)] {
            assert!(text.contains("• No issues found! — exit 0"), "{text}");
            assert!(text.contains(EXIT_CODES_DOC.url), "{text}");
        }

        // Two failures inside one section: a footer keyed on section
        // titles would collapse them into one row.
        let broken = evaluate(&Inputs::default());
        assert_eq!(broken.exit_code(), 1);
        for text in [summary_text(&broken), verbose_text(&broken)] {
            assert!(
                text.contains(&format!("issues found — exit 1 ({}):", EXIT_CODES_DOC.url)),
                "{text}"
            );
            for id in ["ui.target", "ui.socket", "ui.identify"] {
                let row = text
                    .lines()
                    .find(|l| l.trim_start().starts_with(&format!("✗ {id}")))
                    .unwrap_or_else(|| panic!("no footer row for `{id}`:\n{text}"));
                assert!(row.contains("→ https://"), "{row}");
            }
        }

        // Singular reads as English, and a `warn`-only report still
        // exits 0 while naming the issue.
        let warned = evaluate(&Inputs {
            identify: Ok(identify(7, "0.0.1")),
            ..healthy()
        });
        assert_eq!(warned.exit_code(), 0);
        let text = summary_text(&warned);
        assert!(text.contains("• 1 issue found — exit 0"), "{text}");
        assert!(text.contains("! ui.version"), "{text}");
    }

    // ------------------------------------------------------------- color

    /// The gating precedence, exhaustively — this is the part that is
    /// easy to get wrong, and the reason it is a pure function of four
    /// inputs rather than a block that reads the environment.
    #[test]
    fn color_gating_follows_mode_then_tty_then_no_color_then_term() {
        for &is_tty in &[true, false] {
            for no_color in [None, Some(""), Some("1")] {
                for term in [None, Some("xterm-256color"), Some("dumb")] {
                    assert!(
                        !color_enabled(ColorMode::Never, is_tty, no_color, term),
                        "never must never color"
                    );
                    assert!(
                        color_enabled(ColorMode::Always, is_tty, no_color, term),
                        "always bypasses every probe"
                    );
                    let want = is_tty && no_color != Some("1") && term != Some("dumb");
                    assert_eq!(
                        color_enabled(ColorMode::Auto, is_tty, no_color, term),
                        want,
                        "auto tty={is_tty} NO_COLOR={no_color:?} TERM={term:?}"
                    );
                }
            }
        }
        // <https://no-color.org/>: "present and not an empty string".
        // `NO_COLOR=` is not an opt-out, and getting this backwards is
        // the exact mistake the plan's first draft made.
        assert!(color_enabled(
            ColorMode::Auto,
            true,
            Some(""),
            Some("xterm-256color")
        ));
        assert!(!color_enabled(
            ColorMode::Auto,
            true,
            Some("0"),
            Some("xterm-256color")
        ));
        assert_eq!(ColorMode::default(), ColorMode::Auto);
    }

    /// Escapes are the renderer's own and appear only when asked for.
    /// `Style::default()` is uncolored, so every other test in this file
    /// is asserting on the bytes a pipe would see.
    #[test]
    fn no_escape_survives_an_uncolored_style_and_one_appears_with_color() {
        let colored = Style { color: true };
        for inputs in doc_url_battery() {
            let report = evaluate(&inputs);
            for verbose in [false, true] {
                let plain = render_text(&report, Style::default(), verbose);
                assert!(!plain.contains('\x1b'), "verbose={verbose}:\n{plain}");
                let painted = render_text(&report, colored, verbose);
                assert!(painted.contains('\x1b'), "verbose={verbose}");
                // Color decorates; it never changes the words.
                assert_eq!(
                    painted
                        .replace(GREEN, "")
                        .replace(YELLOW, "")
                        .replace(RED, "")
                        .replace(DIM, "")
                        .replace(RESET, ""),
                    plain
                );
                // Glyphs are not gated (§3.11): they have been
                // unconditional since #260, so an ASCII fallback would be
                // a promise the rest of the output breaks.
                assert!(plain.contains('—'), "verbose={verbose}");
            }
        }
    }

    /// AC 15: `--json` always carries everything, and neither `-v` nor
    /// `--color` can reach it.
    #[test]
    fn json_is_byte_identical_across_verbose_and_color() {
        let report = evaluate(&Inputs {
            identify: Ok(identify(7, "0.0.1")),
            ..healthy()
        });
        let baseline = render(&report, true, Style::default(), false).unwrap();
        for style in [Style::default(), Style { color: true }] {
            for verbose in [false, true] {
                assert_eq!(render(&report, true, style, verbose).unwrap(), baseline);
            }
        }
        assert!(!baseline.contains('\x1b'), "{baseline}");
        assert!(baseline.ends_with("}\n"), "one trailing newline");
        // …while the same two knobs demonstrably move the text output,
        // so the equality above is a property rather than a tautology.
        assert_ne!(
            render(&report, false, Style::default(), false).unwrap(),
            render(&report, false, Style::default(), true).unwrap()
        );
        assert_ne!(
            render(&report, false, Style::default(), false).unwrap(),
            render(&report, false, Style { color: true }, false).unwrap()
        );
    }

    // ----------------------------------------------------- ids + doc links

    /// Ids are the stable API the e2e asserts on, and the section
    /// inventory is fixed: all 26 appear in every report, in this order,
    /// whatever the environment (§3.12). Titles are pinned alongside them
    /// because several checks build one id from more than one arm, and an
    /// inconsistent title there would otherwise ship silently.
    #[test]
    fn check_ids_and_titles_are_unique_and_stable() {
        const EXPECTED: &[(&str, &str)] = &[
            ("env.tab_id", "ROOST_TAB_ID"),
            ("env.socket", "ROOST_SOCKET"),
            ("ui.target", "Target resolution"),
            ("ui.socket", "Socket"),
            ("ui.identify", "UI identity"),
            ("ui.version", "Version skew"),
            ("ui.agent_model", "Agent state model"),
            ("shell.login", "Login shell"),
            ("shell.current", "Current shell"),
            ("shell.integration", "Integration contract"),
            ("shell.resources", "Shipped scripts"),
            ("shell.marks_feature", "`marks` feature"),
            ("shell.marks_capability", "Mark capability"),
            ("shell.marks_observed", "Marks observed"),
            ("tab.selection", "Tab selection"),
            ("tab.shell_state", "Shell axis"),
            ("tab.agent_lifecycle", "Agent axis"),
            ("tab.attention", "Attention"),
            ("tab.ownership", "Ownership"),
            ("tab.derived", "Derived state"),
            ("tab.raw_osc", "Raw OSC suppression"),
            ("claude.binary", "`claude` on PATH"),
            ("claude.settings", "Settings file"),
            ("claude.hook_events", "Registered events"),
            ("claude.hook_command", "Hook commands"),
            ("claude.observed", "Hooks reaching Roost"),
        ];
        assert_eq!(EXPECTED.len(), CHECK_COUNT);
        // Every fixture the suite has, so a check whose title differs
        // only on a rare arm is still caught.
        let battery = doc_url_battery();
        for inputs in battery {
            let report = evaluate(&inputs);
            let seen: Vec<(&str, &str)> = report
                .sections
                .iter()
                .flat_map(|s| &s.checks)
                .map(|c| (c.id, c.title))
                .collect();
            assert_eq!(seen, EXPECTED, "{}", verbose_text(&report));
            let unique: std::collections::HashSet<&str> = seen.iter().map(|(id, _)| *id).collect();
            assert_eq!(unique.len(), seen.len(), "duplicate check id");
        }
    }

    /// §3.6's invariant, over every fixture the suite has: a check always
    /// carries a verdict, and an observation never carries one — only
    /// `skipped`, which is the absence of a verdict rather than one.
    /// The three constructors are what enforce it; this is what catches a
    /// `Check` literal written around them.
    #[test]
    fn the_kind_and_status_axes_cannot_disagree() {
        // Pinned here as well as in the constructors: which ids observe
        // is a product decision (§3.7), not an implementation detail.
        const OBSERVATIONS: &[&str] = &[
            "env.socket",
            "shell.current",
            "tab.shell_state",
            "tab.agent_lifecycle",
            "tab.attention",
            "tab.ownership",
            "tab.derived",
            "tab.raw_osc",
        ];
        for inputs in doc_url_battery() {
            let report = evaluate(&inputs);
            for c in report.checks() {
                let want = if OBSERVATIONS.contains(&c.id) {
                    Kind::Observation
                } else {
                    Kind::Check
                };
                assert_eq!(c.kind, want, "{}", c.id);
                match (c.kind, c.status) {
                    (Kind::Check, Some(_)) => {}
                    (Kind::Observation, None | Some(Status::Skipped)) => {}
                    (kind, status) => {
                        panic!("{}: {kind:?} carries {status:?}: {}", c.id, c.detail)
                    }
                }
                if c.kind == Kind::Observation {
                    assert!(c.docs_url.is_none(), "{}", c.id);
                }
            }
            assert_eq!(
                report
                    .checks()
                    .filter(|c| c.kind == Kind::Observation)
                    .count(),
                OBSERVATIONS.len()
            );
        }
    }

    /// The wire vocabulary, pinned directly: the text and JSON renderers
    /// read the same `as_str`, so a rename here is a break for every
    /// `--json` consumer and for every issue-pasted report.
    #[test]
    fn the_status_and_kind_spellings_are_pinned() {
        for (status, want) in [
            (Status::Ok, "ok"),
            (Status::Warn, "warn"),
            (Status::Fail, "fail"),
            (Status::Skipped, "skipped"),
        ] {
            assert_eq!(status.as_str(), want);
            assert_eq!(serde_json::to_value(status).unwrap(), want);
        }
        for (kind, want) in [(Kind::Check, "check"), (Kind::Observation, "observation")] {
            assert_eq!(kind.as_str(), want);
            assert_eq!(serde_json::to_value(kind).unwrap(), want);
        }

        let json = |c: Check| serde_json::to_value(c).unwrap();
        let fact = json(observation("x.fact", "Fact", "d"));
        assert_eq!(fact["kind"], "observation");
        // Present and null, never omitted: a dropped key is
        // indistinguishable from an older schema, and a consumer must be
        // able to read "this is a fact" straight off the field.
        assert_eq!(fact.get("status"), Some(&serde_json::Value::Null), "{fact}");
        let gone = json(unavailable("x.gone", "Gone", "why"));
        assert_eq!(gone["kind"], "observation");
        assert_eq!(gone["status"], "skipped");
        let scored = json(check("x.scored", "Scored", Status::Ok, "d"));
        assert_eq!(scored["kind"], "check");
        assert_eq!(scored["status"], "ok");
    }

    /// The widest spread of `Inputs` the suite has — every degraded
    /// branch a check can take. Shared by the two invariant tests below
    /// so a newly added check is exercised by both.
    fn doc_url_battery() -> Vec<Inputs> {
        let mut commands = hook_commands(&["/opt/other/roostctl"]);
        commands[0].exe_executable = false;
        vec![
            Inputs::default(),
            healthy(),
            // Inside a tab (a PTY-only variable is set) with nothing
            // else wired: `env.tab_id` and `shell.integration` both fail.
            Inputs {
                env_resources_dir: Some("/opt/roost".into()),
                ..Inputs::default()
            },
            Inputs {
                shell_path: Some("/opt/homebrew/bin/fish".into()),
                ..healthy()
            },
            Inputs {
                shell_path: Some("/bin/bash".into()),
                shell_version: SubprocessOutcome::Output("GNU bash, version 3.2.57(1)".into()),
                tab_list: Ok(tab_list(&[Tab {
                    shell_state: ShellState::Unknown,
                    ..tab(7)
                }])),
                ..healthy()
            },
            Inputs {
                shell_path: None,
                shell_usable: false,
                env_resources_dir: None,
                resources_script: None,
                explicit_tab: Some(999),
                identify: Ok(identify(7, "0.0.1")),
                ..healthy()
            },
            Inputs {
                target: Err(TargetFailure::Ambiguous(vec!["mac".into(), "iced".into()])),
                sockets: Vec::new(),
                identify: Err(IdentifyFailure::Protocol("drift".into())),
                ..Inputs::default()
            },
            Inputs {
                target: Err(TargetFailure::UnknownProfile("bogus".into())),
                ..Inputs::default()
            },
            Inputs {
                target: Err(TargetFailure::Path("no HOME".into())),
                ..Inputs::default()
            },
            Inputs {
                sockets: vec![SocketProbe {
                    path: PathBuf::from("/etc/hosts"),
                    profile: None,
                    outcome: SocketOutcome::NotASocket("regular file".into()),
                }],
                ..Inputs::default()
            },
            Inputs {
                tab_list: Ok(legacy_tab_list(&[7])),
                ..healthy()
            },
            Inputs {
                tab_list: Ok(serde_json::json!({"projects": null})),
                ..healthy()
            },
            Inputs {
                claude_settings_path: None,
                claude_settings: SettingsProbe::LocationUnknown,
                claude_version: SubprocessOutcome::Output("2.1.220".into()),
                ..healthy()
            },
            configured(commands),
            Inputs {
                claude_settings: SettingsProbe::Unparseable("bad".into()),
                claude_version: SubprocessOutcome::TimedOut,
                ..healthy()
            },
            Inputs {
                claude_hook_events: Vec::new(),
                claude_hook_commands: Vec::new(),
                claude_settings: SettingsProbe::Parsed,
                claude_version: SubprocessOutcome::Output("2.1.220".into()),
                ..healthy()
            },
        ]
    }

    #[test]
    fn every_fail_or_warn_carries_a_docs_url() {
        let battery = doc_url_battery();
        let mut seen_scored = std::collections::HashSet::new();
        for inputs in &battery {
            for section in &evaluate(inputs).sections {
                for c in &section.checks {
                    if matches!(c.status, Some(Status::Fail | Status::Warn)) {
                        assert!(
                            c.docs_url.is_some(),
                            "`{}` emitted {:?} with no docs_url — add a DOC_TARGETS row",
                            c.id,
                            c.status
                        );
                        seen_scored.insert(c.id);
                    }
                }
            }
        }
        // Sanity: the battery has to actually exercise the scored paths.
        assert!(seen_scored.len() >= 14, "only covered {seen_scored:?}");
    }

    // -- doc anchors -------------------------------------------------------

    fn repo_root() -> PathBuf {
        // `CARGO_MANIFEST_DIR` is `crates/roost-cli`; walk up two levels
        // to the workspace root (same trick as `roost-ipc`'s vectors.rs).
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(p.pop()); // pop "roost-cli"
        assert!(p.pop()); // pop "crates"
        p
    }

    /// Conservative stand-in for Python-Markdown's toc slugify: lowercase,
    /// spaces to hyphens, drop everything that isn't alphanumeric / `-` /
    /// `_`. Every target below is a plain ASCII heading precisely so the
    /// two agree.
    fn slugify(heading: &str) -> String {
        let mut out = String::new();
        for c in heading.trim().to_lowercase().chars() {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                out.push(c);
            } else if c == ' ' {
                out.push('-');
            }
        }
        out
    }

    /// Markdown headings only — `#` inside a fenced code block is a shell
    /// comment, not a heading. `docs/guides/cwd-tracking.md` really does
    /// carry `# 1. Allow it as a login shell` inside a bash fence, so a
    /// naive `starts_with('#')` accepts anchors that do not exist.
    fn headings(body: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut fenced = false;
        for line in body.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                fenced = !fenced;
                continue;
            }
            if !fenced && line.starts_with('#') {
                out.push(line.trim_start_matches('#'));
            }
        }
        out
    }

    /// Is `rel` an actual nav entry — `- Title: page.md` or `- page.md` —
    /// rather than a substring of a comment or a longer path?
    fn nav_lists(nav: &str, rel: &str) -> bool {
        nav.lines().any(|line| {
            let line = line.split('#').next().unwrap_or_default();
            let Some(entry) = line.trim().strip_prefix("- ") else {
                return false;
            };
            let value = entry.rsplit_once(": ").map_or(entry, |(_, v)| v);
            value.trim() == rel
        })
    }

    /// mkdocs' `nav:` block, bounded at the next top-level key — `extra:`
    /// follows it today, and its entries are not nav entries.
    fn nav_block(mkdocs: &str) -> String {
        mkdocs
            .split("\nnav:")
            .nth(1)
            .expect("mkdocs.yml has a nav:")
            .lines()
            .take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t']))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Deliberately one-directional: for each `(page, anchor)` doctor can
    /// emit, assert the page exists, that it is in `mkdocs.yml`'s
    /// hand-maintained nav (`docs/reference/terminal-queries.md` is the
    /// counterexample on disk today), and that some heading slugifies to
    /// the anchor. It does NOT reimplement mkdocs' slugify over every
    /// heading in the repo — only these URLs matter.
    #[test]
    fn doc_anchors_resolve() {
        let root = repo_root();
        let mkdocs = std::fs::read_to_string(root.join("mkdocs.yml")).expect("mkdocs.yml");
        let nav = nav_block(&mkdocs);

        let mut targets: Vec<Doc> = DOC_TARGETS.iter().map(|(_, d)| *d).collect();
        targets.push(EXIT_CODES_DOC);

        for target in targets {
            assert!(
                target.url.starts_with(DOCS_BASE),
                "{} is not built from DOCS_BASE",
                target.url
            );
            let rel = format!("{}.md", target.page);
            let path = root.join("docs").join(&rel);
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(
                nav_lists(&nav, &rel),
                "{rel} is not in mkdocs.yml's nav, so its URL would not publish"
            );
            let found = headings(&body)
                .into_iter()
                .any(|h| slugify(h) == target.anchor);
            assert!(
                found,
                "no heading in {rel} slugifies to `{}`",
                target.anchor
            );
        }
    }

    /// The two tighteners above, pinned against exactly what fooled the
    /// looser versions.
    #[test]
    fn the_doc_anchor_helpers_reject_near_misses() {
        let body = "# Real\n\n```bash\n# 1. Allow it as a login shell\n```\n\n## Also Real\n";
        assert_eq!(headings(body), vec![" Real", " Also Real"]);

        let nav = "  - CLI: reference/cli.md\n  # - Queries: reference/terminal-queries.md\n";
        assert!(nav_lists(nav, "reference/cli.md"));
        assert!(
            !nav_lists(nav, "reference/terminal-queries.md"),
            "a commented-out entry does not publish"
        );
        assert!(
            !nav_lists(nav, "cli.md"),
            "a suffix of a nav path is not a nav entry"
        );
        assert!(!nav_lists(
            &nav_block(&std::fs::read_to_string(repo_root().join("mkdocs.yml")).unwrap()),
            "reference/terminal-queries.md"
        ));
    }

    // -- pure helpers ------------------------------------------------------

    /// `shell_split` is the reader for exactly what `quote_for_shell`
    /// writes into `claude-settings.json`, so drive it from the producer
    /// rather than from hand-written literals: the two can only drift if
    /// nothing feeds one into the other, and the symptom would be doctor
    /// failing a good install whose path holds a space or a quote.
    #[test]
    fn shell_split_reads_back_what_claude_install_wrote() {
        for exe in [
            "/usr/local/bin/roostctl",
            "/Apps/My Roost.app/roostctl",
            "/it's/roostctl",
            "/opt/a$b/roostctl",
        ] {
            let line = format!("{} claude-hook Stop", crate::quote_for_shell(exe));
            assert_eq!(
                shell_split(&line).unwrap(),
                vec![exe, "claude-hook", "Stop"],
                "round-trip failed for {exe} (quoted: {line})"
            );
        }
        // Not reachable from the producer, but the reader must not panic.
        assert_eq!(shell_split("'unterminated"), None);
    }

    /// F11: doctor and `claude-hook` must agree on what `ROOST_TAB_ID`
    /// means, or doctor reports `ok` on exactly the silent no-op it
    /// exists to catch. The shared parser does not trim, because clap
    /// parses the same variable for every other per-tab command and
    /// rejects surrounding whitespace outright.
    #[test]
    fn doctor_and_the_hook_read_roost_tab_id_identically() {
        for raw in [" 7 ", "7 ", "\t7", "0", "-3", "abc", "7x", ""] {
            let inputs = Inputs {
                env_socket: Some("/tmp/roost.sock".into()),
                env_tab_id: Some(raw.to_string()),
                ..Inputs::default()
            };
            assert_eq!(env_tab_id(&inputs), crate::parse_tab_id(raw), "{raw:?}");
            assert_eq!(
                status_of(&evaluate(&inputs), "env.tab_id"),
                Some(Status::Fail),
                "ROOST_TAB_ID={raw:?} is a silent no-op for the hook, so doctor must say so"
            );
        }
        assert_eq!(crate::parse_tab_id("7"), Some(7));
    }

    /// F5: a bare `roostctl claude-hook …` works whenever `roostctl` is
    /// on Claude's PATH — the Linux `.deb` puts one at `/usr/bin` — so
    /// rejecting every non-absolute word failed a healthy install.
    #[test]
    fn a_hook_command_word_resolves_through_path() {
        // Reads `PATH`, which the `collect` tests below overwrite. libtest
        // runs them on separate threads and concurrent setenv/getenv is
        // UB, so this takes the same mutex they do. `blocking_lock` is
        // safe here precisely because this test has no runtime.
        let _env = ENV.blocking_lock();
        // `sh` is on PATH and at an absolute path on every platform this
        // ships to; nothing else about it matters here.
        let bare = resolve_program("sh").expect("`sh` must resolve through PATH");
        assert!(bare.is_absolute(), "{}", bare.display());
        assert_eq!(resolve_program("/bin/sh"), Some(PathBuf::from("/bin/sh")));
        assert_eq!(resolve_program("roostctl-does-not-exist-xyz"), None);
        assert_eq!(resolve_program("/nope/roostctl"), None);
        // A word with a slash is never PATH-searched, even a bare-looking
        // relative one.
        assert_eq!(resolve_program("./sh"), None);

        let cmd = resolve_hook_command("Stop", "sh claude-hook stop");
        assert!(
            cmd.exe_executable,
            "a PATH-resolved hook must not read as missing"
        );
        assert!(cmd.exe_canonical.is_some());
    }

    #[test]
    fn bash_version_parses_the_release_banner() {
        assert_eq!(
            bash_version("GNU bash, version 5.3.9(1)-release (aarch64-apple-darwin24.4.0)"),
            Some((5, 3))
        );
        assert_eq!(
            bash_version("GNU bash, version 3.2.57(1)-release (arm64-apple-darwin24)"),
            Some((3, 2))
        );
        assert_eq!(bash_version("zsh 5.9"), None);
        // bash translates its banner. `capture` forces `LC_ALL=C`, but a
        // banner that reaches the parser any other way must still read.
        assert_eq!(
            bash_version("GNU bash, Version 5.3.9(1)-release (x86_64-pc-linux-gnu)"),
            Some((5, 3))
        );
    }

    /// The scripts match `*",no-$1,"*` against the raw variable
    /// (`roost.bash:127-132`, `roost.zsh:30-35`), so a token with
    /// surrounding whitespace does not disable anything there. Doctor
    /// trimming would report `marks` off on a shell still emitting them.
    #[test]
    fn the_marks_opt_out_matches_the_scripts_byte_for_byte() {
        let features = |v: &str| Inputs {
            env_shell_features: Some(v.to_string()),
            ..healthy()
        };
        assert!(marks_opted_out(&features("cwd,no-marks,prompt")));
        assert!(marks_opted_out(&features("no-marks")));
        assert!(!marks_opted_out(&features("cwd, no-marks")));
        assert!(!marks_opted_out(&features("cwd,no-marks ")));
        assert!(!marks_opted_out(&features("cwd,title,marks")));
    }

    #[test]
    fn shipped_scripts_exist_only_for_bash_and_zsh() {
        assert_eq!(
            shipped_script_path(Some("/opt/roost"), ShellFamily::Bash),
            Some(PathBuf::from("/opt/roost/shell-integration/roost.bash"))
        );
        assert_eq!(
            shipped_script_path(Some("/opt/roost"), ShellFamily::Zsh),
            Some(PathBuf::from("/opt/roost/shell-integration/roost.zsh"))
        );
        assert_eq!(
            shipped_script_path(Some("/opt/roost"), ShellFamily::Fish),
            None
        );
        assert_eq!(shipped_script_path(None, ShellFamily::Bash), None);
    }

    // -- collect, for real -------------------------------------------------
    //
    // The rest of the suite drives `evaluate` over synthetic `Inputs`,
    // which is exactly why the hangs and the orphaned child survived
    // review: neither is reachable from a struct literal. These four run
    // the real `collect` against a hung socket, a shell that never
    // returns, a read-only `$HOME`, and a FIFO where a config file should
    // be. Plan §8's "Integration (Rust)" row.

    /// `collect` reads process-global env, so the tests below serialize
    /// on this. A tokio mutex rather than a `std` one: it is the only
    /// kind that may be held across an `.await`.
    static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Restores whatever the variable held, so one test's `$HOME` cannot
    /// leak into the next.
    struct EnvVar {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvVar {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            EnvVar { key, prev }
        }
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// A fresh directory that removes itself, so a failing assertion is
    /// the only thing a run leaves behind.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "roost-doctor-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("tmpdir");
            TmpDir(path)
        }

        fn join(&self, leaf: &str) -> PathBuf {
            self.0.join(leaf)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Every regular file under `root`, by relative path and content.
    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                    out.push((rel, std::fs::read(&path).unwrap_or_default()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    fn dead_socket(dir: &TmpDir) -> TargetSelector {
        TargetSelector {
            socket_override: Some(dir.join("no-such.sock")),
            kind_override: None,
        }
    }

    /// A UI that accepts the connection and answers nothing must render
    /// as two failed checks, not as a hung doctor.
    #[tokio::test]
    async fn a_hung_ui_fails_the_ipc_checks_and_still_renders_the_report() {
        let _env = ENV.lock().await;
        let dir = TmpDir::new("hung");
        let sock = dir.join("roost.sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let selector = TargetSelector {
            socket_override: Some(sock),
            kind_override: None,
        };
        let started = std::time::Instant::now();
        let inputs = tokio::time::timeout(Duration::from_secs(30), collect(&selector, None))
            .await
            .expect("doctor must not hang on a silent UI");
        let elapsed = started.elapsed();
        server.abort();

        assert!(
            matches!(inputs.identify, Err(IdentifyFailure::Timeout)),
            "{:?}",
            inputs.identify
        );
        assert!(inputs.tab_list.is_err());
        assert!(elapsed < Duration::from_secs(20), "{elapsed:?}");

        let report = evaluate(&inputs);
        assert_status(&report, "ui.socket", Status::Ok);
        assert_status(&report, "ui.identify", Status::Fail);
        // The whole inventory still renders (§3.12).
        assert_eq!(
            report
                .sections
                .iter()
                .map(|s| s.checks.len())
                .sum::<usize>(),
            CHECK_COUNT
        );
        assert!(verbose_text(&report).contains("ui.identify"));
    }

    /// A UI that answers *late* — not never — is the case the hung-UI
    /// test above cannot see. `identify` is cancelled at the deadline but
    /// the response is still in flight, and `IpcClient` matches responses
    /// by monotonic id, so reusing that connection made `tab.list` report
    /// `IdMismatch`: the tab section blanking for a reason that never
    /// happened.
    #[tokio::test]
    async fn a_late_identify_reply_does_not_poison_tab_list() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let _env = ENV.lock().await;
        let dir = TmpDir::new("slow-ui");
        let sock = dir.join("roost.sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let id = serde_json::from_str::<serde_json::Value>(&line)
                            .ok()
                            .and_then(|v| v.get("id").and_then(serde_json::Value::as_i64))
                            .unwrap_or(0);
                        // Well past IPC_TIMEOUT, so every request is
                        // cancelled with its answer still coming.
                        tokio::time::sleep(IPC_TIMEOUT + Duration::from_secs(1)).await;
                        let frame = format!("{{\"id\":{id},\"ok\":true,\"result\":{{}}}}\n");
                        if w.write_all(frame.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let selector = TargetSelector {
            socket_override: Some(sock),
            kind_override: None,
        };
        let inputs = tokio::time::timeout(Duration::from_secs(30), collect(&selector, None))
            .await
            .expect("doctor must not hang on a slow UI");
        server.abort();

        assert!(
            matches!(inputs.identify, Err(IdentifyFailure::Timeout)),
            "{:?}",
            inputs.identify
        );
        let err = inputs.tab_list.expect_err("tab.list must not succeed here");
        assert!(
            !err.contains("id mismatch"),
            "tab.list inherited the cancelled identify's stream: {err}"
        );
        assert!(err.contains("timed out"), "{err}");
    }

    /// Version banners are translated — `bash --version` says "Version"
    /// under de_DE — so the parsers get a fixed locale. Asserted through
    /// the real subprocess, since the env is only set inside [`capture`].
    #[tokio::test]
    async fn version_subprocesses_run_under_the_c_locale() {
        let _env = ENV.lock().await;
        let dir = TmpDir::new("locale");
        let script = dir.join("echo-locale");
        std::fs::write(&script, "#!/bin/sh\necho \"LC_ALL=$LC_ALL LANG=$LANG\"\n")
            .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let _shell = EnvVar::set("SHELL", &script);
        let _lc = EnvVar::set("LC_ALL", "de_DE.UTF-8");
        let _lang = EnvVar::set("LANG", "de_DE.UTF-8");

        let inputs = collect(&dead_socket(&dir), None).await;

        assert_eq!(
            inputs.shell_version,
            SubprocessOutcome::Output("LC_ALL=C LANG=C\n".to_string())
        );
    }

    /// A `$SHELL` that never returns must degrade on the deadline **and
    /// leave nothing behind**: `kill_on_drop` is documented best-effort,
    /// so the timeout path kills and reaps explicitly.
    #[tokio::test]
    async fn a_hanging_shell_times_out_and_its_child_is_reaped() {
        let _env = ENV.lock().await;
        let dir = TmpDir::new("slow-shell");
        let pidfile = dir.join("pid");
        let script = dir.join("slow-sh");
        // `exec` so the pid the script reports is the pid tokio spawned:
        // a forked grandchild would outlive the kill, which is a known
        // limit (plan §9) rather than what this pins.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pidfile.display()
            ),
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let _shell = EnvVar::set("SHELL", &script);

        let started = std::time::Instant::now();
        let inputs = collect(&dead_socket(&dir), None).await;
        let elapsed = started.elapsed();

        assert_eq!(inputs.shell_version, SubprocessOutcome::TimedOut);
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");

        let pid = std::fs::read_to_string(&pidfile).expect("script must have run");
        let alive = std::process::Command::new("/bin/ps")
            .args(["-p", pid.trim()])
            .output()
            .expect("ps");
        assert!(
            !alive.status.success(),
            "the timed-out child {} survived doctor",
            pid.trim()
        );

        let report = evaluate(&inputs);
        assert!(find(&report, "shell.login").detail.contains("timed out"));
    }

    /// AC 12: doctor mutates nothing. Asserted against a `$HOME` doctor
    /// has every reason to write to — it is where `claude install` puts
    /// the settings file doctor reads.
    #[tokio::test]
    async fn collect_leaves_home_byte_identical() {
        let _env = ENV.lock().await;
        let home = TmpDir::new("home");
        let config = home.join(".config/roost");
        std::fs::create_dir_all(&config).expect("mkdir");
        std::fs::write(
            config.join("claude-settings.json"),
            r#"{"hooks":{"Stop":[]}}"#,
        )
        .expect("write settings");

        let _home = EnvVar::set("HOME", &home.0);
        // The three subprocesses are out of doctor's control — `claude`
        // in particular may create its own state — and the guarantee is
        // about *doctor's own* actions, so point PATH at nothing and give
        // `$SHELL` a binary that cannot write anywhere.
        let _path = EnvVar::set("PATH", home.join("empty-bin"));
        let _shell = EnvVar::set("SHELL", "/bin/sh");

        let before = snapshot(&home.0);
        assert!(
            !before.is_empty(),
            "the fixture must have something to lose"
        );
        let inputs = collect(&dead_socket(&home), None).await;
        assert_eq!(
            snapshot(&home.0),
            before,
            "doctor wrote to the tree under $HOME"
        );

        assert_eq!(inputs.claude_settings, SettingsProbe::Parsed);
        assert_eq!(inputs.claude_hook_events, vec!["Stop".to_string()]);
        assert_eq!(inputs.claude_version, SubprocessOutcome::Missing);
    }

    /// A FIFO where the settings file belongs blocked `open` forever.
    /// Doctor has to be safe to run blind, so a non-regular file is a
    /// diagnostic detail and never a read.
    #[tokio::test]
    async fn a_fifo_settings_file_is_a_finding_not_a_hang() {
        let _env = ENV.lock().await;
        let home = TmpDir::new("fifo-home");
        let config = home.join(".config/roost");
        std::fs::create_dir_all(&config).expect("mkdir");
        let fifo = config.join("claude-settings.json");
        assert!(std::process::Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo")
            .success());

        let _home = EnvVar::set("HOME", &home.0);
        let _path = EnvVar::set("PATH", home.join("empty-bin"));
        let _shell = EnvVar::set("SHELL", "/bin/sh");

        let inputs =
            tokio::time::timeout(Duration::from_secs(20), collect(&dead_socket(&home), None))
                .await
                .expect("doctor must not block on a FIFO");

        let SettingsProbe::Unreadable(why) = &inputs.claude_settings else {
            panic!("{:?}", inputs.claude_settings);
        };
        assert!(why.contains("fifo"), "{why}");
        assert_status(&evaluate(&inputs), "claude.settings", Status::Fail);
    }
}
