//! `roostctl session autostart` — the one supervisor artifact that
//! brings `roost-session` back after a login or a reboot.
//!
//! # Why the file is what roost owns, and nothing else
//!
//! There is exactly one artifact per platform — a `systemd --user` unit
//! or a launchd LaunchAgent — at a fixed path with a fixed name. Roost
//! writes it, reads it back to decide whether it is still ours, and
//! removes it. Everything past that (enabled? active? why did it fail?)
//! stays the supervisor's own business: `systemctl --user status` and
//! `launchctl print` are the authority, and `session status` deliberately
//! does not second-guess them.
//!
//! "Still ours" is settled by a marker line this command writes, not by
//! the shape of the file: an artifact hand-copied from an older doc
//! recipe carries no marker and so reads as foreign, needing `--force`
//! once — deliberate, because the alternative is adopting somebody
//! else's unit that happens to look like ours.
//!
//! # How the artifact is named
//!
//! The stem is the session profile's Linux namespace — `roost-session`
//! or `roost-session-dev`, straight from
//! [`roost_ipc::paths::session_namespace`] — and one rule derives the
//! systemd unit name, the launchd label and the `Description`/`Label`
//! line from it ([`Names`]). So a debug build and a release build own
//! different artifacts exactly as they own different sockets, and
//! neither slot is read to decide anything about the other.
//!
//! The artifact name follows the **`roostctl` build**, which is why
//! `ROOST_SESSION_BIN` must name a `roost-session` of the same profile:
//! a debug `roostctl` pointed at a release binary would write
//! `roost-session-dev.service` for a process that binds the *release*
//! socket, and start it at every login.
//!
//! # How a template change reaches a file already on disk
//!
//! The marker carries a **format generation** ([`ARTIFACT_FORMAT`]): an
//! artifact a generation behind is rewritten by a plain `install`, one
//! written by a *newer* roostctl is refused until `--force`. The app
//! version was rejected for this — it changes every release, so every
//! upgrade would report a template that never changed as outdated and
//! demand a restart for it.
//!
//! # Lingering
//!
//! `install --linger` grants `loginctl enable-linger`, which keeps the
//! user manager — and with it the session — up from **boot** rather than
//! from the next login. `uninstall` never revokes it: the grant is
//! per-user and system-wide, and any other unit of that user may be
//! relying on it.
//!
//! # Why the binary path is absolutized but never canonicalized
//!
//! A packaged `/usr/bin/roost-session` is usually a symlink into a
//! versioned target that the next upgrade replaces. Writing the
//! resolved target would pin the unit to a build that is about to
//! vanish, so the path is only made absolute — lexically, `.`/`..`
//! components folded — and written as itself.
//!
//! The split here is `session.rs`'s: the decisions — rendering, path
//! checking, parsing, and the install/uninstall state machines — are
//! pure and table-tested, and the I/O around them is thin. Both
//! renderers compile everywhere so those tables run on every platform;
//! only the install/uninstall/status I/O picks by `cfg(target_os)`.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;

use roost_ipc::paths::{session_namespace, BundleProfile};
use roost_ipc::session_launch::{
    self, confirm_serving, locate_session_binary, BIN_ENV, IPC_TIMEOUT,
};

use crate::session::scaled;

/// The bounded `session.identify` poll after the supervisor has been
/// asked to start one — the same budget `session start` climbs, because
/// it is the same wait.
const CONFIRM_TIMEOUT: Duration = session_launch::DEFAULT_CONFIRM_BUDGET;

/// The generation of the rendered templates. Bump it whenever
/// [`render_unit`] or [`render_plist`] would write different bytes for
/// the same inputs — marker text, keys, ordering. One generation covers
/// both renderers, so a bump made for one platform costs the other a
/// single harmless rewrite.
pub const ARTIFACT_FORMAT: u32 = 2;

/// #438 wrote the marker sentence without a generation, so a file still
/// carrying it is generation 1.
const LEGACY_FORMAT: u32 = 1;

/// The two halves of the marker sentence, with the generation between
/// them. Ownership hangs on this line alone: a description and a
/// `--foreground` tail are things another program can carry by
/// coincidence, and adopting such a file would overwrite it without
/// `--force`.
const MARKER_HEAD: &str = "Written by roostctl session autostart";
const MARKER_TAIL: &str = ". Reinstalling replaces this file.";

fn marker_sentence(format: u32) -> String {
    format!("{MARKER_HEAD} (format {format}){MARKER_TAIL}")
}

/// The marker in each supervisor's comment syntax — the same sentence
/// both times.
fn unit_marker(format: u32) -> String {
    format!("# {}", marker_sentence(format))
}

fn plist_marker(format: u32) -> String {
    format!("<!-- {} -->", marker_sentence(format))
}

/// Which comment syntax a marker line is written in. Part of the
/// marker: the two artifacts are different languages, and a line only
/// systemd could read has no authority over a plist (nor the reverse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerSyntax {
    Unit,
    Plist,
}

/// The generation one line declares, if it is a marker of `syntax` at
/// all.
///
/// The whole trimmed line must be the marker, spelled the way this
/// command writes it, so neither a marker quoted inside a longer line
/// nor one in the other artifact's comment syntax is one. A generation
/// of 0, one that does not fit a `u32`, and anything that is not plain
/// digits are all read as "no marker", because an artifact this command
/// cannot place in the sequence is not one it may rewrite.
pub fn parse_marker(line: &str, syntax: MarkerSyntax) -> Option<u32> {
    let line = line.trim();
    let sentence = match syntax {
        MarkerSyntax::Unit => line.strip_prefix("# ")?,
        MarkerSyntax::Plist => line.strip_prefix("<!-- ")?.strip_suffix(" -->")?,
    };
    let rest = sentence.strip_prefix(MARKER_HEAD)?;
    if rest == MARKER_TAIL {
        return Some(LEGACY_FORMAT);
    }
    let digits = rest
        .strip_prefix(" (format ")?
        .strip_suffix(MARKER_TAIL)?
        .strip_suffix(')')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok().filter(|found| *found >= 1)
}

/// The generation a whole artifact declares, in one syntax. Two marker
/// lines disagreeing about it leave nothing to trust, so the file is
/// not ours.
fn text_format(text: &str, syntax: MarkerSyntax) -> Option<u32> {
    let mut found = None;
    for format in text.lines().filter_map(|line| parse_marker(line, syntax)) {
        if found.is_some_and(|seen| seen != format) {
            return None;
        }
        found = Some(format);
    }
    found
}

/// The tail every artifact of ours ends in — `--foreground` is what
/// keeps the daemon attached to its supervisor instead of forking away.
const EXEC_TAIL: &str = "start --foreground";

const XDG_CONFIG_HOME_ENV: &str = "XDG_CONFIG_HOME";

#[derive(Subcommand, Debug)]
pub enum AutostartCmd {
    /// Write the supervisor artifact for this platform, load it, and
    /// confirm a session answers. Prints the artifact path, the
    /// `roost-session` binary it names, and — on Linux — whether
    /// lingering is granted.
    Install {
        /// Replace a file at the artifact path that roostctl did not
        /// write. Its previous contents are echoed to stderr first.
        #[arg(long)]
        force: bool,
        /// Linux: also `loginctl enable-linger`, so the session comes
        /// back at boot and not just at the next login. A flag rather
        /// than the default because it is a system-wide grant — every
        /// `default.target` unit of this user starts at boot too.
        /// Refused on macOS, which has no equivalent.
        #[arg(long)]
        linger: bool,
    },
    /// Remove the artifact and unload it. This **stops the supervised
    /// session** — a session the supervisor is not running is untouched.
    Uninstall,
}

/// Run an `autostart` verb. Returns the process exit code; the caller
/// keeps the one exit point.
pub async fn run(cmd: &AutostartCmd) -> Result<i32> {
    match cmd {
        AutostartCmd::Install { force, linger } => install(*force, *linger).await,
        AutostartCmd::Uninstall => uninstall().await,
    }
}

// ============================================================================
// Platform
// ============================================================================

/// The supervisors roost knows how to write for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
}

/// This build's supervisor, or `None` where roost has none — every I/O
/// entry point reports "not supported" / "unavailable" on `None`.
pub const fn host_platform() -> Option<Platform> {
    #[cfg(target_os = "linux")]
    {
        Some(Platform::Linux)
    }
    #[cfg(target_os = "macos")]
    {
        Some(Platform::MacOs)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

// ============================================================================
// Names
// ============================================================================

/// Every identifier one bundle profile's artifact carries, derived from
/// the profile's Linux namespace — see the module docs for why that
/// string and not the app label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Names {
    debug_build: bool,
}

impl Names {
    pub fn for_build(debug_build: bool) -> Self {
        Self { debug_build }
    }

    pub fn stem(&self) -> &'static str {
        session_namespace(self.debug_build)
    }

    pub fn unit(&self) -> String {
        format!("{}.service", self.stem())
    }

    pub fn label(&self) -> String {
        format!("ai.stridelabs.{}", self.stem())
    }

    pub fn plist_file(&self) -> String {
        format!("{}.plist", self.label())
    }

    /// The `Description=` line that says a unit is ours — profile
    /// included, so the other profile's file reads as foreign.
    pub fn description(&self) -> String {
        format!("Description=Roost host session ({})", self.stem())
    }

    pub fn service_target(&self, uid: u32) -> String {
        format!("gui/{uid}/{}", self.label())
    }

    /// The sibling profile: dev from a release build, release from a dev
    /// one.
    pub fn other(&self) -> Self {
        Self::for_build(!self.debug_build)
    }

    /// How the profile is named in a sentence about the other slot.
    pub fn slot(&self) -> &'static str {
        if self.debug_build {
            "dev"
        } else {
            "release"
        }
    }
}

/// How the artifact is named in user-facing output.
pub fn supervisor_label(platform: Platform, names: &Names) -> String {
    match platform {
        Platform::Linux => format!("systemd --user {}", names.unit()),
        Platform::MacOs => format!("launchd {}", names.label()),
    }
}

/// Where the artifact lives. `xdg_config_home` is honoured only when it
/// is absolute — the XDG spec's own rule, and what keeps a stray
/// relative value from writing a unit against the process cwd.
pub fn artifact_path(
    platform: Platform,
    names: &Names,
    home: &Path,
    xdg_config_home: Option<&Path>,
) -> PathBuf {
    match platform {
        Platform::Linux => xdg_config_home
            .filter(|p| p.is_absolute())
            .map_or_else(|| home.join(".config"), Path::to_path_buf)
            .join("systemd")
            .join("user")
            .join(names.unit()),
        Platform::MacOs => home
            .join("Library")
            .join("LaunchAgents")
            .join(names.plist_file()),
    }
}

// ============================================================================
// Rendering
// ============================================================================

/// `Restart=on-failure` is the systemd spelling of the launchd recipe's
/// `KeepAlive {SuccessfulExit: false}`: a clean `session stop` exits 0
/// and stays stopped, a crash respawns. `KillMode=mixed` sends the
/// stop's SIGTERM to the daemon alone so its own orderly ladder (SIGHUP
/// the shells, reap, SIGKILL fallback) runs the way `session stop` does;
/// the default `control-group` would SIGTERM every shell at once and
/// race it. `WorkingDirectory=%h` seeds the first project on an empty
/// state file — without it a supervisor-launched daemon seeds `/`.
pub fn render_unit(names: &Names, bin: &Path) -> String {
    format!(
        "[Unit]\n\
         {marker}\n\
         {description}\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory=%h\n\
         ExecStart=\"{bin}\" {EXEC_TAIL}\n\
         Restart=on-failure\n\
         KillMode=mixed\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        marker = unit_marker(ARTIFACT_FORMAT),
        description = names.description(),
        bin = bin.to_string_lossy(),
    )
}

/// The recipe `docs/guides/host-sessions.md` has always documented, plus
/// `WorkingDirectory` for the same reason the unit carries `%h`.
///
/// Both paths must have cleared [`check_plist_path`] first, which is why
/// the lossy conversions below cannot lose anything.
pub fn render_plist(names: &Names, bin: &Path, home: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
{marker}
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>WorkingDirectory</key>
    <string>{home}</string>
</dict>
</plist>
"#,
        marker = plist_marker(ARTIFACT_FORMAT),
        label = names.label(),
        bin = xml_escape(&bin.to_string_lossy()),
        home = xml_escape(&home.to_string_lossy()),
    )
}

fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_unescape(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// A path this command will not write into an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(String);

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// The bytes both renderers need before they can render anything.
fn utf8_path(bin: &Path) -> Result<&str, Refusal> {
    bin.to_str().ok_or_else(|| {
        Refusal(format!(
            "{} is not valid UTF-8, so it cannot be written into an autostart artifact",
            bin.display()
        ))
    })
}

/// The character set a systemd `ExecStart` path may use.
///
/// The path is written double-quoted, so a space is fine. Everything
/// else is refused **by name** rather than escaped: `systemd.syntax(7)`
/// gives `"`, `\`, `$` and `%` (a specifier) their own meanings inside a
/// quoted word, and `;` ends the line — reimplementing that quoting for
/// a path no packaging ever produces would be more code than the case
/// deserves, and getting it subtly wrong would write a unit that starts
/// the wrong program.
pub fn check_unit_path(bin: &Path) -> Result<(), Refusal> {
    let text = utf8_path(bin)?;
    for ch in text.chars() {
        let allowed = ch.is_ascii_alphanumeric() || " /._+@:,~-".contains(ch);
        if !allowed {
            return Err(Refusal(format!(
                "{} contains {ch:?}, which a systemd ExecStart path may not carry \
                 (allowed: letters, digits, and ` /._+@:,~-`)",
                bin.display()
            )));
        }
    }
    Ok(())
}

/// The characters a plist string may carry.
///
/// XML 1.0 has no representation at all for most control characters, so
/// a path holding one renders a plist launchd rejects outright — and a
/// non-UTF-8 path would go through `to_string_lossy` and name a
/// *different* program or working directory than the one asked for.
/// Both are refused by name here, the way [`check_unit_path`] refuses
/// what systemd cannot quote. Tab, CR and LF are legal XML and refused
/// anyway: no real path carries them, and one that did would write an
/// artifact nobody could read back.
pub fn check_plist_path(path: &Path) -> Result<(), Refusal> {
    let text = utf8_path(path)?;
    match text.chars().find(|ch| ch.is_control()) {
        Some(ch) => Err(Refusal(format!(
            "{} contains {ch:?}, which XML cannot carry in a plist string",
            path.display()
        ))),
        None => Ok(()),
    }
}

/// Make `bin` absolute against `cwd` and fold `.` / `..` **lexically**.
///
/// Deliberately not `canonicalize`: see the module docs — a packaged
/// symlink must be written as itself, not as the versioned build it
/// happens to point at today.
pub fn absolutize(bin: &Path, cwd: &Path) -> PathBuf {
    let joined = if bin.is_absolute() {
        bin.to_path_buf()
    } else {
        cwd.join(bin)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `/..` is `/`; a leading `..` in a relative path has
                // nothing to climb and has to survive.
                let rooted = out.has_root();
                if !out.pop() && !rooted {
                    out.push(Component::ParentDir);
                }
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(Component::CurDir);
    }
    out
}

// ============================================================================
// Reading a file back
// ============================================================================

/// An artifact roostctl recognises as its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub platform: Platform,
    pub binary: PathBuf,
    pub format: u32,
}

/// Whose file this is, as far as this roostctl can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Foreign,
    /// Ours by the marker, from a generation this build does not know.
    Newer {
        found: u32,
    },
    Ours(Artifact),
}

/// Is this file ours, and which binary does it name?
///
/// Ours means our marker line **and** the label/`Description` **and** an
/// exec line ending in `start --foreground` — all three. A hand-edited
/// copy (extra `Environment=` lines, a reordered plist) still reads as
/// ours and is overwritten on re-install; anything else, including a
/// file hand-written from the docs, is foreign and needs `--force`.
///
/// A newer generation is settled from the marker **before** the other
/// two parts are looked at: a template from the future may have changed
/// exactly those parts, and the marker sentence is the one thing this
/// command controls across generations.
pub fn parse_artifact(text: &str, names: &Names) -> Parsed {
    let unit = text_format(text, MarkerSyntax::Unit);
    let plist = text_format(text, MarkerSyntax::Plist);
    // A file carrying both syntaxes' markers is not one this command
    // wrote, whatever the two of them say.
    let (format, parsed) = match (unit, plist) {
        (Some(_), Some(_)) | (None, None) => return Parsed::Foreign,
        (Some(format), None) => (format, parse_unit(text, names, format)),
        (None, Some(format)) => (format, parse_plist(text, names, format)),
    };
    if format > ARTIFACT_FORMAT {
        return Parsed::Newer { found: format };
    }
    parsed.map_or(Parsed::Foreign, Parsed::Ours)
}

fn has_line(text: &str, marker: &str) -> bool {
    text.lines().any(|line| line.trim() == marker)
}

fn parse_unit(text: &str, names: &Names, format: u32) -> Option<Artifact> {
    if !has_line(text, &names.description()) {
        return None;
    }
    let exec = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("ExecStart="))?;
    let binary = exec
        .trim()
        .strip_suffix(EXEC_TAIL)?
        .trim()
        .trim_matches('"');
    if binary.is_empty() {
        return None;
    }
    Some(Artifact {
        platform: Platform::Linux,
        binary: PathBuf::from(binary),
        format,
    })
}

fn parse_plist(text: &str, names: &Names, format: u32) -> Option<Artifact> {
    if !text.contains(&format!("<string>{}</string>", names.label())) {
        return None;
    }
    let array = text
        .split_once("<key>ProgramArguments</key>")?
        .1
        .split_once("<array>")?
        .1
        .split_once("</array>")?
        .0;
    let args: Vec<String> = array
        .split("<string>")
        .skip(1)
        .filter_map(|chunk| chunk.split_once("</string>"))
        .map(|(value, _)| xml_unescape(value))
        .collect();
    match args.as_slice() {
        [binary, verb, flag] if verb == "start" && flag == "--foreground" && !binary.is_empty() => {
            Some(Artifact {
                platform: Platform::MacOs,
                binary: PathBuf::from(binary),
                format,
            })
        }
        _ => None,
    }
}

/// What `session status` says about the artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartState {
    NotInstalled,
    /// A file is there that roostctl did not write.
    Foreign,
    /// Something is at the path that [`read_artifact`] will not read.
    Unreadable(String),
    /// Ours, from a generation this build cannot read — so the binary it
    /// names is unknown.
    Newer {
        found: u32,
    },
    Installed {
        binary: PathBuf,
        format: u32,
        binary_missing: bool,
    },
}

/// Installed-ness is the file existing at the expected path **and being
/// ours**. The binary it names is reported, never compared with the one
/// this `roostctl` would resolve: that comparison is roostctl-relative,
/// so a dev build beside a release install would cry wolf.
pub fn installed_state(
    read: &ArtifactRead,
    parsed: Parsed,
    binary_executable: bool,
) -> AutostartState {
    match (read, parsed) {
        (ArtifactRead::Absent, _) => AutostartState::NotInstalled,
        (ArtifactRead::Unavailable(reason), _) => AutostartState::Unreadable(reason.clone()),
        (ArtifactRead::Text(_), Parsed::Foreign) => AutostartState::Foreign,
        (ArtifactRead::Text(_), Parsed::Newer { found }) => AutostartState::Newer { found },
        (ArtifactRead::Text(_), Parsed::Ours(artifact)) => AutostartState::Installed {
            binary: artifact.binary,
            format: artifact.format,
            binary_missing: !binary_executable,
        },
    }
}

/// The `autostart=…` line, in the plain one-line style the rest of
/// `session status` prints.
///
/// "installed on disk" is the whole claim: this reads the file and
/// nothing else, so whether the supervisor is enabled or loaded is
/// `roostctl doctor`'s answer, not this line's.
pub fn render_status_line(
    platform: Platform,
    names: &Names,
    artifact: &Path,
    state: &AutostartState,
) -> String {
    match state {
        AutostartState::NotInstalled => "autostart=not installed".to_string(),
        AutostartState::Foreign => format!(
            "autostart=not installed (foreign file: not written by roostctl: {})",
            artifact.display()
        ),
        AutostartState::Unreadable(reason) => format!(
            "autostart=not installed (unreadable: {}: {reason})",
            artifact.display()
        ),
        AutostartState::Newer { found } => format!(
            "autostart=installed on disk ({}; format {found}, newer than this roostctl)",
            supervisor_label(platform, names)
        ),
        AutostartState::Installed {
            binary,
            format,
            binary_missing,
        } => {
            let mut line = format!(
                "autostart=installed on disk ({} → {})",
                supervisor_label(platform, names),
                binary.display()
            );
            if *binary_missing {
                line.push_str(&format!(" (binary missing: {})", binary.display()));
            }
            if *format < ARTIFACT_FORMAT {
                line.push_str(&format!(
                    " (format {format}, outdated: rerun roostctl session autostart install)"
                ));
            }
            line
        }
    }
}

/// The line for a platform roost has no supervisor for.
pub const UNAVAILABLE_LINE: &str = "autostart=unavailable";

/// Read the artifact and render the `autostart=` line. Never fails: the
/// running/not-running exit semantics of `session status` are
/// independent of what this finds.
pub fn status_line() -> String {
    let Some(platform) = host_platform() else {
        return UNAVAILABLE_LINE.to_string();
    };
    let names = host_names();
    let home = match home_dir() {
        Ok(home) => home,
        Err(error) => return format!("{UNAVAILABLE_LINE} ({error})"),
    };
    let artifact = artifact_path(platform, &names, &home, xdg_config_home().as_deref());
    let read = read_artifact(&artifact);
    let parsed = match &read {
        ArtifactRead::Text(text) => parse_artifact(text, &names),
        // Ignored — `installed_state` settles a non-`Text` read first.
        _ => Parsed::Foreign,
    };
    let executable = matches!(&parsed, Parsed::Ours(a) if is_executable_file(&a.binary));
    let state = installed_state(&read, parsed, executable);
    render_status_line(platform, &names, &artifact, &state)
}

/// The other profile's slot, if it holds an artifact of *its own* — the
/// one sentence either verb prints about a file it will not touch.
///
/// A foreign file over there is nobody's business and yields nothing;
/// nor does one this process cannot safely read, nor one from a
/// generation too new to name the binary it starts.
pub fn sibling_notice(
    platform: Platform,
    other: &Names,
    other_path: &Path,
    other_read: &ArtifactRead,
    uid: u32,
) -> Option<String> {
    let ArtifactRead::Text(text) = other_read else {
        return None;
    };
    let Parsed::Ours(artifact) = parse_artifact(text, other) else {
        return None;
    };
    let remedy = format!(
        "{} && rm {}",
        unload_command(platform, other, uid),
        shell_word(other_path)
    );
    Some(format!(
        "note: the {}-slot artifact also exists ({} → {}) and is left alone; to remove it: {remedy}",
        other.slot(),
        other_path.display(),
        artifact.binary.display()
    ))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

// ============================================================================
// The supervisor seam
// ============================================================================

/// One supervisor invocation, owned so a sequence can be recorded and
/// asserted on before or after it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.program)?;
        for arg in &self.args {
            write!(f, " {arg}")?;
        }
        Ok(())
    }
}

/// What a supervisor command said. A nonzero exit is an `Ok` result —
/// only "the command could not be run at all" is an `Err`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    /// `None` when the command was killed by a signal.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

/// Every supervisor interaction goes through here, so the command set
/// and its order are testable without a systemd or a launchd.
pub trait Supervisor {
    fn platform(&self) -> Platform;
    fn names(&self) -> &Names;
    fn uid(&self) -> u32;
    fn run(&self, command: &Command) -> Result<CommandResult>;

    /// Does the supervisor already hold this definition?
    ///
    /// This is the question [`activate`] must not be asked over: `enable
    /// --now` on an active unit is a silent no-op for the running
    /// process, and `launchctl bootstrap` on a label that is merely
    /// *loaded* — no process anywhere — fails outright. Answering "not
    /// loaded" for an idle-but-loaded agent would bootstrap it, fail,
    /// and leave the next install reporting `Unchanged` over a
    /// definition the supervisor never re-read.
    ///
    /// So on macOS "loaded" is exactly `launchctl print` exiting 0;
    /// whether a process exists is [`Supervisor::is_running`]'s
    /// question. On Linux `systemctl --user is-active` answers both —
    /// there is no loaded-but-idle state for a `Type=simple` unit.
    fn is_loaded(&self) -> Result<bool> {
        Ok(self
            .run(&active_command(self.platform(), self.names(), self.uid()))?
            .ok())
    }

    /// Is a supervised process alive right now?
    ///
    /// This — not the socket — is what proves whose daemon is answering:
    /// a supervised start that finds a session already there prints
    /// `already-running` and exits 0, leaving the job idle while the
    /// socket answers perfectly well.
    fn is_running(&self) -> Result<bool> {
        let out = self.run(&active_command(self.platform(), self.names(), self.uid()))?;
        Ok(match self.platform() {
            Platform::Linux => out.ok(),
            // `launchctl print` exits 0 for a *loaded* label whether or
            // not a process exists; only a `pid` line says one does.
            Platform::MacOs => out.ok() && out.stdout.contains("pid = "),
        })
    }
}

fn active_command(platform: Platform, names: &Names, uid: u32) -> Command {
    match platform {
        Platform::Linux => Command::new("systemctl", &["--user", "is-active", &names.unit()]),
        Platform::MacOs => Command::new("launchctl", &["print", &names.service_target(uid)]),
    }
}

/// What unloads an artifact — run by [`deactivate`] for our own slot, and
/// printed verbatim by [`sibling_notice`] as the remedy for the other
/// one, so the sentence cannot drift from the command.
fn unload_command(platform: Platform, names: &Names, uid: u32) -> Command {
    match platform {
        Platform::Linux => {
            Command::new("systemctl", &["--user", "disable", "--now", &names.unit()])
        }
        Platform::MacOs => Command::new("launchctl", &["bootout", &names.service_target(uid)]),
    }
}

/// What a user runs by hand when this command deliberately did not.
pub fn restart_command(platform: Platform, names: &Names, artifact: &Path, uid: u32) -> String {
    match platform {
        Platform::Linux => format!("systemctl --user restart {}", names.stem()),
        Platform::MacOs => format!(
            "launchctl bootout {} && launchctl bootstrap gui/{uid} {}",
            names.service_target(uid),
            shell_word(artifact)
        ),
    }
}

/// A path as one word in a command a user is invited to paste: bare
/// when every byte is shell-inert, single-quoted otherwise.
pub fn shell_word(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let inert = |c: char| c.is_ascii_alphanumeric() || "/._+@:,~=-".contains(c);
    if !raw.is_empty() && raw.chars().all(inert) {
        raw.into_owned()
    } else {
        format!("'{}'", raw.replace('\'', "'\\''"))
    }
}

/// What starts the *next* session under the supervisor when this one
/// was already running unsupervised.
pub fn start_command(platform: Platform, names: &Names, uid: u32) -> String {
    match platform {
        Platform::Linux => format!("systemctl --user start {}", names.stem()),
        Platform::MacOs => format!("launchctl kickstart {}", names.service_target(uid)),
    }
}

/// A supervisor command that failed, named with what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationFailure {
    pub command: String,
    pub detail: String,
}

impl fmt::Display for ActivationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.command, self.detail.trim())
    }
}

impl std::error::Error for ActivationFailure {}

fn run_all(
    supervisor: &dyn Supervisor,
    commands: &[Command],
    ignorable: impl Fn(&CommandResult) -> bool,
) -> Result<(), ActivationFailure> {
    for command in commands {
        let result = supervisor.run(command).map_err(|error| ActivationFailure {
            command: command.to_string(),
            detail: format!("{error:#}"),
        })?;
        if !result.ok() && !ignorable(&result) {
            return Err(ActivationFailure {
                command: command.to_string(),
                detail: if result.stderr.trim().is_empty() {
                    format!("exit {:?}", result.status)
                } else {
                    result.stderr.clone()
                },
            });
        }
    }
    Ok(())
}

/// Tell the supervisor the bytes changed, before anything asks it what
/// it holds.
///
/// systemd caches unit definitions: without this a rewrite over a loaded
/// unit leaves `systemctl --user restart` — the very command the
/// `ReinstalledLoaded` report tells the user to run — starting the *old*
/// `ExecStart`. launchd re-reads the plist on `bootstrap`, so macOS
/// needs nothing here.
///
/// [`activate`] keeps its own `daemon-reload` for the path where nothing
/// was written (an identical file the supervisor never loaded), so a
/// fresh install reloads twice. Idempotent, and it leaves each command
/// list readable on its own.
pub fn reload_after_write(
    supervisor: &dyn Supervisor,
    wrote: bool,
) -> Result<(), ActivationFailure> {
    let commands = match (supervisor.platform(), wrote) {
        (Platform::Linux, true) => vec![Command::new("systemctl", &["--user", "daemon-reload"])],
        _ => Vec::new(),
    };
    run_all(supervisor, &commands, |_| false)
}

/// Load a freshly written artifact.
pub fn activate(supervisor: &dyn Supervisor, artifact: &Path) -> Result<(), ActivationFailure> {
    let commands = match supervisor.platform() {
        Platform::Linux => vec![
            Command::new("systemctl", &["--user", "daemon-reload"]),
            Command::new(
                "systemctl",
                &["--user", "enable", "--now", &supervisor.names().unit()],
            ),
        ],
        // `bootstrap` also starts it, per `RunAtLoad`.
        Platform::MacOs => vec![Command::new(
            "launchctl",
            &[
                "bootstrap",
                &format!("gui/{}", supervisor.uid()),
                &artifact.to_string_lossy(),
            ],
        )],
    };
    run_all(supervisor, &commands, |_| false)
}

/// Unload it, **before** the file is removed. The supervisor answering
/// "there is nothing here" is the state the caller asked for, not a
/// failure.
pub fn deactivate(supervisor: &dyn Supervisor) -> Result<(), ActivationFailure> {
    let platform = supervisor.platform();
    let commands = [unload_command(
        platform,
        supervisor.names(),
        supervisor.uid(),
    )];
    run_all(supervisor, &commands, |result| {
        already_gone(platform, result.status, &result.stderr)
    })
}

/// The tail of an uninstall, after the file is gone. launchd needs none.
pub fn finish_uninstall(supervisor: &dyn Supervisor) -> Result<(), ActivationFailure> {
    let commands = match supervisor.platform() {
        Platform::Linux => vec![Command::new("systemctl", &["--user", "daemon-reload"])],
        Platform::MacOs => Vec::new(),
    };
    run_all(supervisor, &commands, |_| false)
}

/// Did the unload fail only because there was nothing to unload?
///
/// Narrow on purpose: any other nonzero exit is a real failure and
/// surfaces, rather than being swallowed as "probably fine". The
/// phrases are the ones each supervisor emits under the C locale, which
/// is what [`HostSupervisor::run`] pins them to.
pub fn already_gone(platform: Platform, exit: Option<i32>, stderr: &str) -> bool {
    match platform {
        Platform::Linux => {
            exit == Some(1) && (stderr.contains("does not exist") || stderr.contains("not loaded"))
        }
        Platform::MacOs => {
            exit.is_some_and(|code| code != 0)
                && (stderr.contains("No such process") || stderr.contains("Could not find service"))
        }
    }
}

// ============================================================================
// The install state machine
// ============================================================================

/// What was at the artifact path when the install began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    Absent,
    /// Something is there that [`read_artifact`] will not read.
    Unreadable,
    /// Ours, byte-identical to what this install would write.
    Identical,
    /// Ours, different bytes — a hand edit, or a different binary.
    Changed,
    /// Ours, from an older generation of the templates.
    Outdated {
        found: u32,
    },
    /// Ours, from a generation this build does not know. Only reaches
    /// the state machine under `--force`.
    Newer {
        found: u32,
    },
    /// Not ours. Only reaches the state machine under `--force`.
    Foreign,
}

/// Compare what is on disk with what this install would write. A
/// generation mismatch can never be byte-identical, so `Identical` and
/// `Changed` are answers about the current generation alone.
pub fn classify_existing(read: &ArtifactRead, rendered: &str, names: &Names) -> Existing {
    let text = match read {
        ArtifactRead::Absent => return Existing::Absent,
        ArtifactRead::Unavailable(_) => return Existing::Unreadable,
        ArtifactRead::Text(text) => text,
    };
    match parse_artifact(text, names) {
        Parsed::Foreign => Existing::Foreign,
        Parsed::Newer { found } => Existing::Newer { found },
        Parsed::Ours(artifact) if artifact.format < ARTIFACT_FORMAT => Existing::Outdated {
            found: artifact.format,
        },
        Parsed::Ours(_) if text == rendered => Existing::Identical,
        Parsed::Ours(_) => Existing::Changed,
    }
}

/// What an install does with the bytes on disk, before the supervisor
/// hears anything at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStep {
    /// Something is at the path this process will not read, so it cannot
    /// say whose it is. `--force` does not help: the contents that would
    /// be echoed are exactly what could not be read.
    RefuseUnreadable,
    /// A file roostctl did not write. Refused by name rather than
    /// clobbered — the marker is the only thing standing between an
    /// install and somebody else's unit.
    RefuseForeign,
    /// A file a newer roostctl wrote. Replacing it would be a downgrade,
    /// which only `--force` asks for.
    RefuseNewer { found: u32 },
    /// The bytes already match: leave the file exactly as it is.
    Keep,
    /// Write, echoing any previous contents to stderr first so a hand
    /// edit is never silently lost.
    Write,
}

pub fn write_step(existing: Existing, force: bool) -> WriteStep {
    match existing {
        Existing::Unreadable => WriteStep::RefuseUnreadable,
        Existing::Foreign if !force => WriteStep::RefuseForeign,
        Existing::Newer { found } if !force => WriteStep::RefuseNewer { found },
        Existing::Identical => WriteStep::Keep,
        _ => WriteStep::Write,
    }
}

/// The supervisor's own view of the unit/agent, asked twice for two
/// different reasons: before the write, whether it already **holds** the
/// definition (which is what decides whether it may be loaded at all);
/// after the load, whether it is **running** the session that answered.
/// On macOS those are genuinely different states — see
/// [`Supervisor::is_loaded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Supervision {
    pub loaded_before: bool,
    pub running_after: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Byte-identical artifact: nothing written, supervisor untouched.
    Unchanged,
    /// Written over a loaded supervisor: the running definition is the
    /// old one until the user restarts it. `enable --now` on an active
    /// unit is a silent no-op for the running process and `launchctl
    /// bootstrap` on a loaded label fails outright, so neither is run.
    ReinstalledLoaded,
    /// The supervisor started a session that did not exist before.
    SupervisedFresh,
    /// A session was already running; the supervised start found it and
    /// exited 0, so the unit sits stopped-on-purpose. Install never
    /// hangs up a user's shells.
    AlreadyRunningUntouched,
    /// The commands succeeded but neither the supervisor nor the socket
    /// shows the session they should have produced.
    ActivationUnconfirmed,
}

/// The two cases that settle before the supervisor is asked anything.
/// `None` means "write, then run the load commands".
///
/// Identical bytes settle only while the supervisor already holds the
/// definition. Otherwise the file is right and nothing is running it —
/// which is precisely the state [`InstallOutcome::ActivationUnconfirmed`]
/// leaves behind, since it keeps the file and asks for a retry. Settling
/// there would make that retry a no-op reporting success.
pub fn settled_without_supervisor(
    existing: Existing,
    loaded_before: bool,
) -> Option<InstallOutcome> {
    match existing {
        Existing::Identical if loaded_before => Some(InstallOutcome::Unchanged),
        Existing::Identical => None,
        _ if loaded_before => Some(InstallOutcome::ReinstalledLoaded),
        _ => None,
    }
}

/// The whole install, classified over what was on disk, what the
/// supervisor says, and which session id answered before and after.
///
/// The socket alone cannot tell whose session answered — see
/// [`Supervisor::is_loaded`].
pub fn classify_install(
    existing: Existing,
    supervision: Supervision,
    before: Option<&str>,
    after: Option<&str>,
) -> InstallOutcome {
    if let Some(settled) = settled_without_supervisor(existing, supervision.loaded_before) {
        return settled;
    }
    match (after, supervision.running_after) {
        (Some(id), true) if Some(id) != before => InstallOutcome::SupervisedFresh,
        (Some(id), false) if Some(id) == before => InstallOutcome::AlreadyRunningUntouched,
        _ => InstallOutcome::ActivationUnconfirmed,
    }
}

/// What an uninstall does with what is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallStep {
    NothingToDo,
    RefuseForeign,
    RefuseUnreadable(String),
    Remove,
}

/// Removing a newer artifact is not downgrading it — the user asked for
/// it to be gone, and it is ours by the marker. Only a file this command
/// cannot claim (foreign) or cannot read at all is left alone.
pub fn uninstall_step(read: &ArtifactRead, names: &Names) -> UninstallStep {
    match read {
        ArtifactRead::Absent => UninstallStep::NothingToDo,
        ArtifactRead::Unavailable(reason) => UninstallStep::RefuseUnreadable(reason.clone()),
        ArtifactRead::Text(text) => match parse_artifact(text, names) {
            Parsed::Foreign => UninstallStep::RefuseForeign,
            Parsed::Newer { .. } | Parsed::Ours(_) => UninstallStep::Remove,
        },
    }
}

// ============================================================================
// Lingering
// ============================================================================

/// What macOS gets instead of a grant, printed verbatim.
///
/// Lingering *is* a logind concept: launchd starts a LaunchAgent at the
/// next login and has nothing that means "and at boot, with nobody
/// logged in". So the flag is refused by name rather than quietly
/// ignored — and refused before the binary is resolved, before any file
/// is read and before anything is printed, so a flag this platform
/// cannot honour never leaves half an install behind.
pub const MACOS_LINGER_REFUSAL: &str = "roostctl session autostart: --linger is a systemd-logind \
     concept; a LaunchAgent starts at your next login and macOS has no equivalent \
     (see the host-sessions guide)";

pub fn linger_refusal(platform: Platform, linger: bool) -> Option<&'static str> {
    (linger && platform == Platform::MacOs).then_some(MACOS_LINGER_REFUSAL)
}

/// What an install does about lingering once its outcome is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LingerStep {
    /// Say nothing: there is nothing installed for lingering to keep
    /// alive, or this platform has no such concept.
    Skip,
    /// Ask `show-user` and print the `linger=` line.
    Report,
    /// Grant it first, then ask `show-user` anyway.
    GrantThenReport,
}

/// The `--linger` table: what the flag does for each way an install can
/// end. `outcome` is `None` when no outcome was reached at all — a
/// refusal before or at the write, or an activation that failed.
///
/// The grant is made only where the install actually succeeded.
/// `enable-linger` is idempotent, so `Unchanged` with the flag is the
/// natural "grant it now" gesture; `ActivationUnconfirmed` keeps the
/// file but has no working session, so it reports the state without
/// changing it.
pub fn linger_step(
    platform: Platform,
    outcome: Option<InstallOutcome>,
    linger: bool,
) -> LingerStep {
    if platform != Platform::Linux {
        return LingerStep::Skip;
    }
    let Some(outcome) = outcome else {
        return LingerStep::Skip;
    };
    match outcome {
        InstallOutcome::ActivationUnconfirmed => LingerStep::Report,
        InstallOutcome::Unchanged
        | InstallOutcome::ReinstalledLoaded
        | InstallOutcome::SupervisedFresh
        | InstallOutcome::AlreadyRunningUntouched => {
            if linger {
                LingerStep::GrantThenReport
            } else {
                LingerStep::Report
            }
        }
    }
}

/// Whether this user's manager stays up with nobody logged in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LingerState {
    Yes,
    No,
    /// `loginctl` could not be run, or said something this command
    /// cannot place — the reason, as it will be printed.
    Unknown(String),
}

/// Read `loginctl show-user … --property=Linger --value`.
///
/// Takes the seam's whole `Result` so the one failure that produces no
/// output at all — no `loginctl` on this box — is a state like any
/// other rather than an error that would abort an install that has
/// already succeeded.
/// Supervisor words as one line of a report.
///
/// A failed `loginctl` puts its own text in front of the user, and a
/// newline in it would open a line the report never wrote — a forged
/// `linger=yes` among them. Every line break and control byte becomes a
/// space, and runs of blanks collapse, so the text stays legible and
/// stays one line.
fn one_line(raw: &str) -> String {
    raw.split(|c: char| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The one wording for a linger question `loginctl` never answered. An
/// install reaches it through the supervisor seam and doctor's probe
/// through its own runner, and the two must not word it differently.
fn linger_unavailable(reason: &str) -> LingerState {
    LingerState::Unknown(format!("loginctl could not be run: {reason}"))
}

pub fn parse_linger(result: &Result<CommandResult>) -> LingerState {
    match result {
        Ok(result) => parse_linger_answer(result),
        Err(error) => linger_unavailable(&one_line(&format!("{error:#}"))),
    }
}

fn parse_linger_answer(result: &CommandResult) -> LingerState {
    if !result.ok() {
        let stderr = result.stderr.trim();
        return LingerState::Unknown(if stderr.is_empty() {
            match result.status {
                Some(code) => format!("loginctl exited {code}"),
                None => "loginctl was killed by a signal".to_string(),
            }
        } else {
            one_line(stderr)
        });
    }
    match result.stdout.trim() {
        "yes" => LingerState::Yes,
        "no" => LingerState::No,
        // Quoted, so an empty answer is visible and a hostile one
        // cannot forge a line out of control bytes.
        other => LingerState::Unknown(format!("unexpected loginctl answer: {other:?}")),
    }
}

pub fn render_linger(state: &LingerState, uid: u32) -> String {
    match state {
        LingerState::Yes => "linger=yes".to_string(),
        LingerState::No => format!(
            "linger=no (the unit starts at login, not at boot; \
             rerun with --linger or run: loginctl enable-linger {uid})"
        ),
        LingerState::Unknown(reason) => format!("linger=unknown ({reason})"),
    }
}

const NO_ASK_PASSWORD: &str = "--no-ask-password";

/// `--no-ask-password` on every `loginctl` invocation: [`HostSupervisor`]
/// inherits this process's stdin, so without it a polkit check from an
/// inactive session (over ssh, say) would put a password prompt in the
/// middle of an install. With it the refusal is a deterministic stderr
/// line instead — and the command printed for the user to run by hand
/// deliberately omits the flag, because there the prompt is the point.
fn enable_linger_command(uid: u32) -> Command {
    Command::new(
        "loginctl",
        &["enable-linger", &uid.to_string(), NO_ASK_PASSWORD],
    )
}

/// `program` because `doctor` asks the same question through an
/// absolute path — see [`LOGINCTL_PATH`] — while the install path keeps
/// PATH resolution.
fn linger_query_command(program: &str, uid: u32) -> Command {
    Command::new(
        program,
        &[
            "show-user",
            &uid.to_string(),
            "--property=Linger",
            "--value",
            NO_ASK_PASSWORD,
        ],
    )
}

/// What the linger half of an install has to say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LingerOutcome {
    /// The stderr lines a refused grant prints, in order.
    pub grant_failure: Vec<String>,
    /// What `show-user` says, or `None` when the step was
    /// [`LingerStep::Skip`].
    pub state: Option<LingerState>,
}

/// Grant lingering if the step asks for it, then read the state back.
///
/// `show-user` is the authority either way: `enable-linger` exiting 0
/// is a command that was accepted, not a state that was observed, and
/// the printed line only ever claims what was observed.
pub fn settle_linger(supervisor: &dyn Supervisor, step: LingerStep) -> LingerOutcome {
    let uid = supervisor.uid();
    let grant_failure = match step {
        LingerStep::Skip => return LingerOutcome::default(),
        LingerStep::Report => Vec::new(),
        // `failure.detail`, not `ActivationFailure`'s `Display`: that
        // names the command including `--no-ask-password`, which the
        // manual line beside it deliberately omits.
        LingerStep::GrantThenReport => {
            match run_all(supervisor, &[enable_linger_command(uid)], |_| false) {
                Ok(()) => Vec::new(),
                Err(failure) => vec![
                    format!(
                        "roostctl session autostart: loginctl enable-linger {uid}: {}",
                        one_line(&failure.detail)
                    ),
                    format!("run it by hand: loginctl enable-linger {uid}"),
                ],
            }
        }
    };
    LingerOutcome {
        grant_failure,
        state: Some(parse_linger(
            &supervisor.run(&linger_query_command("loginctl", uid)),
        )),
    }
}

// ============================================================================
// The doctor probe
// ============================================================================

/// The programs the probe runs, named by absolute path.
///
/// `roostctl doctor` has to be safe to run under a hostile `PATH` — the
/// rule its own `/bin/ps` follows — and every command below only reads.
/// The mutating verbs keep PATH resolution ([`HostSupervisor::run`]):
/// they are user-invoked, and a layout that puts `systemctl` elsewhere
/// (NixOS) has to stay installable.
const SYSTEMCTL_PATH: &str = "/usr/bin/systemctl";
const LOGINCTL_PATH: &str = "/usr/bin/loginctl";
const LAUNCHCTL_PATH: &str = "/bin/launchctl";

/// What the artifact slot holds, with the read and the parse folded into
/// one fact — the form every `doctor` entry keys on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactState {
    Absent,
    /// Something is there that [`read_artifact`] will not read, and why.
    Unavailable(String),
    Foreign,
    Newer {
        found: u32,
    },
    Ours(Artifact),
}

impl ArtifactState {
    /// Is this a file this roostctl owns? A newer generation counts: its
    /// marker names the unit, which is what every supervisor question is
    /// about, even though its body is not this build's to read.
    fn is_ours(&self) -> bool {
        matches!(self, ArtifactState::Ours(_) | ArtifactState::Newer { .. })
    }
}

pub fn artifact_state(read: &ArtifactRead, names: &Names) -> ArtifactState {
    match read {
        ArtifactRead::Absent => ArtifactState::Absent,
        ArtifactRead::Unavailable(reason) => ArtifactState::Unavailable(one_line(reason)),
        ArtifactRead::Text(text) => match parse_artifact(text, names) {
            Parsed::Foreign => ArtifactState::Foreign,
            Parsed::Newer { found } => ArtifactState::Newer { found },
            Parsed::Ours(artifact) => ArtifactState::Ours(artifact),
        },
    }
}

/// What became of one probe command. Mirrors doctor's own subprocess
/// outcome, so its runner maps onto this 1:1 and nothing here needs to
/// know how the command was spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    Ran(CommandResult),
    /// The program is not on this system at all.
    Missing,
    TimedOut,
    Failed(String),
}

/// Will the supervisor start the session at the next login?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enablement {
    Enabled,
    /// systemd's `enabled-runtime`: this boot only.
    EnabledUntilReboot,
    /// The supervisor's own word for "it will not start".
    Disabled(String),
    Masked,
    /// The file is at the path and the manager does not see it.
    NotSeen,
    NotProbed(String),
}

/// Is a supervised session running right now?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Running {
        pid: Option<u32>,
    },
    /// Not running, legitimately — the word observed. `session stop`
    /// exits 0 on purpose, so a stopped session is a state, not a fault.
    Idle(String),
    Failed(String),
    /// launchd holds no such job right now.
    NotLoaded,
    NotProbed(String),
}

/// The half of a probe that comes off the filesystem, before any
/// supervisor is asked anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeTarget {
    /// `None` where roost has no supervisor for this platform.
    pub platform: Option<Platform>,
    pub names: Names,
    pub uid: u32,
    /// `None` when `$HOME` left nowhere to look — the reason is then in
    /// [`Self::artifact`], and the linger question is asked anyway.
    pub artifact_path: Option<PathBuf>,
    pub artifact: ArtifactState,
    /// `None` when no artifact of ours named a binary to check.
    pub binary_executable: Option<bool>,
    /// The other profile's slot, when it holds an artifact of its own.
    pub sibling: Option<(PathBuf, Artifact)>,
}

impl Default for ProbeTarget {
    fn default() -> Self {
        Self {
            platform: None,
            names: host_names(),
            uid: host_uid(),
            artifact_path: None,
            artifact: ArtifactState::Absent,
            binary_executable: None,
            sibling: None,
        }
    }
}

/// Everything `roostctl doctor` knows about autostart. Every fact is
/// typed so that "not answered" carries its own reason instead of
/// collapsing into an absent `Option`.
impl ProbeTarget {
    /// The binary the artifact names, when this build could read the
    /// artifact at all.
    pub fn binary(&self) -> Option<&Path> {
        match &self.artifact {
            ArtifactState::Ours(artifact) => Some(&artifact.binary),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub target: ProbeTarget,
    pub enablement: Enablement,
    pub activity: Activity,
    /// Asked on Linux whatever the artifact says: the grant is per-user
    /// and outlives any unit.
    pub linger: LingerState,
}

impl Default for Probe {
    fn default() -> Self {
        interpret_probe(ProbeTarget::default(), &[])
    }
}

/// Every command doctor runs to answer "will it come back?" — all of
/// them read-only, which a test pins by name.
///
/// The supervisor is asked only about a slot holding an artifact of
/// ours: over an absent, foreign or unreadable one there is no unit of
/// roost's to report on, and `is-enabled` against a name nobody wrote
/// would answer about somebody else's file. Lingering is asked
/// regardless — it is a property of the user, not of any artifact.
pub fn probe_commands(
    platform: Platform,
    names: &Names,
    uid: u32,
    artifact: &ArtifactState,
) -> Vec<Command> {
    let ours = artifact.is_ours();
    match platform {
        Platform::Linux => {
            let mut commands = Vec::new();
            if ours {
                commands.push(Command::new(
                    SYSTEMCTL_PATH,
                    &["--user", "is-enabled", &names.unit()],
                ));
                commands.push(Command::new(
                    SYSTEMCTL_PATH,
                    &["--user", "is-active", &names.unit()],
                ));
            }
            commands.push(linger_query_command(LOGINCTL_PATH, uid));
            commands
        }
        Platform::MacOs if ours => vec![
            Command::new(LAUNCHCTL_PATH, &["print", &names.service_target(uid)]),
            Command::new(LAUNCHCTL_PATH, &["print-disabled", &format!("gui/{uid}")]),
        ],
        Platform::MacOs => Vec::new(),
    }
}

/// Which question a command asks, read back off the command itself so
/// the runner may answer them in any order. The verb is the first
/// argument that is not a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeKind {
    Enabled,
    Active,
    Print,
    PrintDisabled,
    Linger,
}

fn probe_kind(command: &Command) -> Option<ProbeKind> {
    let verb = command.args.iter().find(|arg| !arg.starts_with('-'))?;
    match verb.as_str() {
        "is-enabled" => Some(ProbeKind::Enabled),
        "is-active" => Some(ProbeKind::Active),
        "print" => Some(ProbeKind::Print),
        "print-disabled" => Some(ProbeKind::PrintDisabled),
        "show-user" => Some(ProbeKind::Linger),
        _ => None,
    }
}

/// Fold a finished probe run into typed facts.
///
/// Each fact reads **its own** command's outcome, so a program that
/// could not be run leaves every fact that needed it `NotProbed` /
/// `Unknown` carrying that reason — the commands run concurrently, so
/// one of them failing to spawn truncates nothing. A fact no command was
/// issued for keeps the reason it was never asked ([`not_asked`]).
pub fn interpret_probe(target: ProbeTarget, outcomes: &[(Command, CommandOutcome)]) -> Probe {
    let unasked = not_asked(&target);
    let label = target.names.label();
    let mut probe = Probe {
        enablement: Enablement::NotProbed(unasked.clone()),
        activity: Activity::NotProbed(unasked.clone()),
        linger: LingerState::Unknown(unasked),
        target,
    };
    for (command, outcome) in outcomes {
        match probe_kind(command) {
            Some(ProbeKind::Enabled) => probe.enablement = read_is_enabled(command, outcome),
            Some(ProbeKind::Active) => probe.activity = read_is_active(command, outcome),
            Some(ProbeKind::Print) => probe.activity = read_launchctl_print(command, outcome),
            Some(ProbeKind::PrintDisabled) => {
                probe.enablement = read_print_disabled(command, outcome, &label);
            }
            Some(ProbeKind::Linger) => probe.linger = read_linger(command, outcome),
            None => {}
        }
    }
    probe
}

/// Why the supervisor was never asked. The report's entries settle on
/// the artifact state before they look at these, so this is a backstop
/// that keeps a `NotProbed` from carrying an empty reason.
fn not_asked(target: &ProbeTarget) -> String {
    match (target.platform, &target.artifact) {
        (None, _) => "roost has no supervisor on this platform".to_string(),
        (_, ArtifactState::Absent) => "the artifact is not installed".to_string(),
        (_, ArtifactState::Unavailable(reason)) => reason.clone(),
        (_, ArtifactState::Foreign) => "the artifact was not written by roostctl".to_string(),
        (_, ArtifactState::Ours(_) | ArtifactState::Newer { .. }) => {
            "the supervisor was not asked".to_string()
        }
    }
}

/// `systemctl is-enabled` answers with **one word on stdout** and an
/// exit code that only repeats it (1 for disabled, 4 for not-found), so
/// the word is the answer and the code is ignored. No word at all means
/// the manager never answered — a dead bus, say — and what it said
/// instead becomes the reason.
fn read_is_enabled(command: &Command, outcome: &CommandOutcome) -> Enablement {
    let CommandOutcome::Ran(result) = outcome else {
        return Enablement::NotProbed(outcome_reason(command, outcome));
    };
    let Some(word) = result.stdout.split_whitespace().next() else {
        return Enablement::NotProbed(ran_reason(result));
    };
    match word {
        "enabled" => Enablement::Enabled,
        "enabled-runtime" => Enablement::EnabledUntilReboot,
        "disabled" => Enablement::Disabled(word.to_string()),
        "masked" | "masked-runtime" => Enablement::Masked,
        "not-found" => Enablement::NotSeen,
        // Quoted, the way `parse_linger` quotes an answer it cannot
        // place: a word this build does not know is reported, never
        // guessed at.
        other => Enablement::NotProbed(format!("unexpected is-enabled answer: {other:?}")),
    }
}

/// `systemctl is-active`, read exactly like [`read_is_enabled`].
fn read_is_active(command: &Command, outcome: &CommandOutcome) -> Activity {
    let CommandOutcome::Ran(result) = outcome else {
        return Activity::NotProbed(outcome_reason(command, outcome));
    };
    let Some(word) = result.stdout.split_whitespace().next() else {
        return Activity::NotProbed(ran_reason(result));
    };
    match word {
        "active" => Activity::Running { pid: None },
        "inactive" | "activating" | "deactivating" => Activity::Idle(word.to_string()),
        "failed" => Activity::Failed("the unit is in the failed state".to_string()),
        other => Activity::NotProbed(format!("unexpected is-active answer: {other:?}")),
    }
}

/// `launchctl print <target>`: exiting 0 means launchd holds the job,
/// and the body says whether a process exists (`pid = N`) or how the
/// last one ended (`last exit code = N`). "Could not find service" is
/// the one nonzero exit that is an answer rather than a failure.
fn read_launchctl_print(command: &Command, outcome: &CommandOutcome) -> Activity {
    let CommandOutcome::Ran(result) = outcome else {
        return Activity::NotProbed(outcome_reason(command, outcome));
    };
    if !result.ok() {
        return if result.stderr.contains("Could not find service")
            || result.stdout.contains("Could not find service")
        {
            Activity::NotLoaded
        } else {
            Activity::NotProbed(ran_reason(result))
        };
    }
    if let Some(pid) = launchctl_field(&result.stdout, "pid").and_then(|v| v.parse().ok()) {
        return Activity::Running { pid: Some(pid) };
    }
    match launchctl_field(&result.stdout, "last exit code").and_then(|v| v.parse::<i64>().ok()) {
        Some(code) if code != 0 => Activity::Failed(format!("last exit code {code}")),
        _ => Activity::Idle("loaded, not running".to_string()),
    }
}

/// One `key = value` line out of a `launchctl print` body, matched on
/// the whole key so `pid` cannot read `spawn type`'s value.
fn launchctl_field<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim())
    })
}

/// `launchctl print-disabled gui/<uid>` lists the override database, one
/// `"<label>" => disabled|enabled` row per label. The label is matched
/// **whole and quoted**: `ai.stridelabs.roost-session` is a prefix of
/// `ai.stridelabs.roost-session-dev`, so a substring test would read one
/// profile's row as the other's. A label with no row is not disabled.
fn read_print_disabled(command: &Command, outcome: &CommandOutcome, label: &str) -> Enablement {
    let CommandOutcome::Ran(result) = outcome else {
        return Enablement::NotProbed(outcome_reason(command, outcome));
    };
    // Unlike `is-enabled`, this one has no answer to give through its
    // exit code: a nonzero exit is a failure to list the database.
    if !result.ok() {
        return Enablement::NotProbed(ran_reason(result));
    }
    let quoted = format!("\"{label}\"");
    let disabled = result.stdout.lines().any(|line| {
        matches!(line.split_once("=>"), Some((name, state))
            if name.trim() == quoted && state.trim() == "disabled")
    });
    if disabled {
        Enablement::Disabled("disabled in launchd's database".to_string())
    } else {
        Enablement::Enabled
    }
}

/// The linger fact from a probe outcome — the install path's own
/// answer reader, so a probe and an install can never word the same
/// state differently.
fn read_linger(command: &Command, outcome: &CommandOutcome) -> LingerState {
    match outcome {
        CommandOutcome::Ran(result) => parse_linger_answer(result),
        other => linger_unavailable(&outcome_reason(command, other)),
    }
}

/// Why a probe command left nothing usable behind, as one line.
fn outcome_reason(command: &Command, outcome: &CommandOutcome) -> String {
    match outcome {
        CommandOutcome::Ran(result) => ran_reason(result),
        CommandOutcome::Missing => format!("{} is not on this system", command.program),
        CommandOutcome::TimedOut => format!("`{command}` timed out"),
        CommandOutcome::Failed(reason) => one_line(reason),
    }
}

fn ran_reason(result: &CommandResult) -> String {
    let stderr = one_line(&result.stderr);
    if !stderr.is_empty() {
        return stderr;
    }
    match result.status {
        Some(code) => format!("exited {code} with no output"),
        None => "was killed by a signal".to_string(),
    }
}

// ============================================================================
// I/O
// ============================================================================

struct HostSupervisor {
    platform: Platform,
    names: Names,
    uid: u32,
}

impl HostSupervisor {
    fn new(platform: Platform, names: Names) -> Self {
        Self {
            platform,
            names,
            uid: host_uid(),
        }
    }
}

pub fn host_uid() -> u32 {
    // SAFETY: a plain getter with no arguments.
    unsafe { libc::getuid() }
}

impl Supervisor for HostSupervisor {
    fn platform(&self) -> Platform {
        self.platform
    }

    fn names(&self) -> &Names {
        &self.names
    }

    fn uid(&self) -> u32 {
        self.uid
    }

    fn run(&self, command: &Command) -> Result<CommandResult> {
        let output = std::process::Command::new(&command.program)
            .args(&command.args)
            // [`already_gone`] reads these commands' stderr for phrases
            // systemd and launchd translate, so they get the fixed
            // locale those phrases belong to — `doctor`'s version
            // parsers run under the same one, for the same reason.
            // Without it a German shell's "existiert nicht" would look
            // like a real failure and the uninstall would keep the file.
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .output()
            .with_context(|| format!("run `{command}`"))?;
        Ok(CommandResult {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

pub fn home_dir() -> Result<PathBuf> {
    let raw = std::env::var_os("HOME").context("$HOME is not set")?;
    let path = PathBuf::from(raw);
    anyhow::ensure!(
        path.is_absolute(),
        "$HOME is not an absolute path (got {})",
        path.display()
    );
    Ok(path)
}

pub fn xdg_config_home() -> Option<PathBuf> {
    std::env::var_os(XDG_CONFIG_HOME_ENV)
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
}

/// This build's profile. The one place the build profile is read — every
/// other name in this module is derived from the [`Names`] it hands out.
pub fn host_names() -> Names {
    Names::for_build(cfg!(debug_assertions))
}

/// The four facts both verbs open with: this build's supervisor, its
/// names, the home directory, and where the artifact goes. `None` —
/// already reported — where roost has no supervisor to write for.
fn artifact_target() -> Result<Option<(Platform, Names, PathBuf, PathBuf)>> {
    let Some(platform) = host_platform() else {
        eprintln!("roostctl session autostart: autostart is not supported on this platform");
        return Ok(None);
    };
    let names = host_names();
    let home = home_dir()?;
    let artifact = artifact_path(platform, &names, &home, xdg_config_home().as_deref());
    Ok(Some((platform, names, home, artifact)))
}

/// The artifact text this platform installs, or the refusal that stops
/// the install before anything is written.
fn render_artifact(
    platform: Platform,
    names: &Names,
    bin: &Path,
    home: &Path,
) -> Result<String, Refusal> {
    match platform {
        Platform::Linux => {
            check_unit_path(bin)?;
            Ok(render_unit(names, bin))
        }
        Platform::MacOs => {
            check_plist_path(bin)?;
            check_plist_path(home)?;
            Ok(render_plist(names, bin, home))
        }
    }
}

/// Write `text` atomically at `path`, mode 0644. A directory this call
/// has to create is created 0755; **an existing one is left exactly as
/// it is** — `~/.config/systemd/user` is often deliberately 0700, and
/// widening it because a unit was installed into it would be a silent
/// permission change nobody asked for. The tmp file is a sibling so the
/// rename is one `rename(2)` inside the target's own directory, and it
/// is removed on every path out that is not that rename.
fn write_artifact(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = path
        .parent()
        .context("the artifact path has no parent directory")?;
    if !dir.is_dir() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| -> Result<()> {
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
            .with_context(|| format!("chmod {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// What was at an artifact path, for a caller that must go on either
/// way: neither verb may hang or exhaust itself over a file it does not
/// own, and `status` reports rather than fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactRead {
    Absent,
    Text(String),
    /// Something is there that this process will not read, and why.
    Unavailable(String),
}

/// A supervisor artifact is a few hundred bytes; anything past this is
/// not one, and reading it would be the only unbounded allocation in
/// either verb.
pub const ARTIFACT_READ_CAP: u64 = 64 * 1024;

/// Read an artifact, refusing anything that is not a plain file of
/// sensible size.
///
/// Opened `O_NONBLOCK` so a FIFO at the artifact path returns at once
/// instead of blocking until somebody writes to it, and the
/// regular-file check runs on the handle actually opened — a `stat`
/// first would leave a window for a FIFO to be swapped in before the
/// `open`. A regular file's reads ignore the flag.
pub fn read_artifact(path: &Path) -> ArtifactRead {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ArtifactRead::Absent,
        Err(error) => return ArtifactRead::Unavailable(error.to_string()),
    };
    let meta = match file.metadata() {
        Ok(meta) => meta,
        Err(error) => return ArtifactRead::Unavailable(error.to_string()),
    };
    if !meta.is_file() {
        return ArtifactRead::Unavailable("not a regular file".to_string());
    }
    if meta.len() > ARTIFACT_READ_CAP {
        return ArtifactRead::Unavailable(too_large());
    }

    let mut bytes = Vec::new();
    match file.take(ARTIFACT_READ_CAP + 1).read_to_end(&mut bytes) {
        Err(error) => ArtifactRead::Unavailable(error.to_string()),
        // The size check above raced a writer that grew the file.
        Ok(_) if bytes.len() as u64 > ARTIFACT_READ_CAP => ArtifactRead::Unavailable(too_large()),
        Ok(_) => match String::from_utf8(bytes) {
            Ok(text) => ArtifactRead::Text(text),
            Err(_) => ArtifactRead::Unavailable("not valid UTF-8".to_string()),
        },
    }
}

fn too_large() -> String {
    format!("larger than {} KiB", ARTIFACT_READ_CAP / 1024)
}

/// Which session, if any, is answering right now.
async fn current_session_id(socket: &Path) -> Option<String> {
    session_launch::identify(socket, scaled(IPC_TIMEOUT))
        .await
        .ok()
        .map(|identity| identity.session_id)
}

async fn install(force: bool, linger: bool) -> Result<i32> {
    if let Some(refusal) = host_platform().and_then(|platform| linger_refusal(platform, linger)) {
        eprintln!("{refusal}");
        return Ok(1);
    }
    let Some((platform, names, home, artifact)) = artifact_target()? else {
        return Ok(1);
    };

    let located = locate_session_binary(
        std::env::var_os(BIN_ENV).as_deref(),
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )?;
    let cwd = std::env::current_dir().context("read the working directory")?;
    let bin = absolutize(&located.path, &cwd);
    let text = render_artifact(platform, &names, &bin, &home)?;

    let previous = read_artifact(&artifact);
    let existing = classify_existing(&previous, &text, &names);
    let step = write_step(existing, force);

    // Reported before the target line: a path this process could not
    // read is not a path it is about to install into.
    if let (WriteStep::RefuseUnreadable, ArtifactRead::Unavailable(reason)) = (step, &previous) {
        eprintln!(
            "roostctl session autostart: cannot read {}: {reason}",
            artifact.display()
        );
        return Ok(1);
    }

    println!("autostart: {} → {}", artifact.display(), bin.display());
    match step {
        // Reported above, before the target line.
        WriteStep::RefuseUnreadable => return Ok(1),
        WriteStep::RefuseForeign => {
            eprintln!(
                "roostctl session autostart: {} was not written by roostctl; \
                 pass --force to replace it",
                artifact.display()
            );
            return Ok(1);
        }
        WriteStep::RefuseNewer { found } => {
            eprintln!(
                "roostctl session autostart: {} was written by a newer roostctl \
                 (format {found}; this one writes {ARTIFACT_FORMAT}); \
                 pass --force to downgrade it",
                artifact.display()
            );
            return Ok(1);
        }
        WriteStep::Keep => {}
        WriteStep::Write => {
            if let ArtifactRead::Text(old) = &previous {
                match existing {
                    Existing::Outdated { found } => eprintln!(
                        "roostctl session autostart: upgrading {} from format {found} to \
                         format {ARTIFACT_FORMAT}; previous contents follow\n{old}",
                        artifact.display()
                    ),
                    _ => eprintln!(
                        "roostctl session autostart: replacing {}; previous contents follow\n{old}",
                        artifact.display()
                    ),
                }
            }
            write_artifact(&artifact, &text)?;
        }
    }

    let supervisor = HostSupervisor::new(platform, names);
    let uid = supervisor.uid();
    let wrote = matches!(step, WriteStep::Write);
    let outcome = supervise_install(&supervisor, &artifact, existing, wrote).await?;

    let lingering = settle_linger(&supervisor, linger_step(platform, outcome, linger));
    let sibling = sibling_report(platform, &names, &home, xdg_config_home().as_deref(), uid);
    for line in install_tail(sibling, &lingering, uid) {
        match line {
            Tail::Note(text) => eprintln!("{text}"),
            Tail::Report(text) => println!("{text}"),
        }
    }
    Ok(install_exit_code(outcome, &lingering))
}

/// One line the tail of an install prints, and which stream it belongs
/// on: a note is advice about something this verb did not do, a report
/// is a fact about the install that just ran.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tail {
    Note(String),
    Report(String),
}

/// Everything an install says after its outcome, in the order it says
/// it: the other slot's artifact first — a fact about a file this verb
/// deliberately left alone — then anything the grant had to say, and
/// the `linger=` line last, as the closing word about this machine.
fn install_tail(sibling: Option<String>, linger: &LingerOutcome, uid: u32) -> Vec<Tail> {
    sibling
        .map(Tail::Note)
        .into_iter()
        .chain(linger.grant_failure.iter().cloned().map(Tail::Note))
        .chain(
            linger
                .state
                .as_ref()
                .map(|state| Tail::Report(render_linger(state, uid))),
        )
        .collect()
}

/// A refusal or a failed activation reached no outcome at all and exits
/// 1; so does an outcome no session could be confirmed for, and so does
/// a grant that was asked for and refused — the install itself
/// succeeded there, and its artifact and unit stay where they are.
fn install_exit_code(outcome: Option<InstallOutcome>, lingering: &LingerOutcome) -> i32 {
    let failed = !lingering.grant_failure.is_empty()
        || matches!(outcome, None | Some(InstallOutcome::ActivationUnconfirmed));
    i32::from(failed)
}

/// The half of an install that runs once the bytes on disk are settled:
/// load it, confirm a session, report. Split out so every way it can end
/// returns to one place — the tail about the other profile's slot and
/// about lingering. `None` is an activation that failed — nothing was
/// classified, and the file is kept.
async fn supervise_install(
    supervisor: &HostSupervisor,
    artifact: &Path,
    existing: Existing,
    wrote: bool,
) -> Result<Option<InstallOutcome>> {
    let platform = supervisor.platform();
    let names = supervisor.names();
    let uid = supervisor.uid();

    if let Err(failure) = reload_after_write(supervisor, wrote) {
        eprintln!("roostctl session autostart: installed; activation failed: {failure}");
        return Ok(None);
    }
    let loaded_before = supervisor.is_loaded()?;

    if let Some(settled) = settled_without_supervisor(existing, loaded_before) {
        report_install(settled, platform, names, artifact, uid, None);
        return Ok(Some(settled));
    }

    let socket = BundleProfile::session()
        .context("resolve the session socket path")?
        .socket_path;
    let before = current_session_id(&socket).await;

    if let Err(failure) = activate(supervisor, artifact) {
        // The file is kept: a retry is `install` again, and `status`
        // reporting it as installed is the truth about what roost owns.
        eprintln!("roostctl session autostart: installed; activation failed: {failure}");
        return Ok(None);
    }

    let after = confirm_serving(&socket, scaled(CONFIRM_TIMEOUT))
        .await
        .ok()
        .map(|identity| identity.session_id);
    let running_after = supervisor.is_running()?;
    let outcome = classify_install(
        existing,
        Supervision {
            loaded_before,
            running_after,
        },
        before.as_deref(),
        after.as_deref(),
    );
    report_install(outcome, platform, names, artifact, uid, after.as_deref());
    Ok(Some(outcome))
}

/// The other slot's path, resolved from *these* names, read and judged.
pub fn sibling_report(
    platform: Platform,
    names: &Names,
    home: &Path,
    xdg_config_home: Option<&Path>,
    uid: u32,
) -> Option<String> {
    let other = names.other();
    let path = artifact_path(platform, &other, home, xdg_config_home);
    let read = read_artifact(&path);
    sibling_notice(platform, &other, &path, &read, uid)
}

fn report_install(
    outcome: InstallOutcome,
    platform: Platform,
    names: &Names,
    artifact: &Path,
    uid: u32,
    session_id: Option<&str>,
) {
    match outcome {
        InstallOutcome::Unchanged => {
            println!("unchanged (already installed)");
        }
        InstallOutcome::ReinstalledLoaded => {
            println!("reinstalled; the supervisor was not touched");
            println!(
                "the running session keeps the old definition; restart it with: {}",
                restart_command(platform, names, artifact, uid)
            );
        }
        InstallOutcome::SupervisedFresh => {
            println!("installed and started");
            if let Some(id) = session_id {
                println!("session_id={id}");
            }
        }
        InstallOutcome::AlreadyRunningUntouched => {
            println!("installed; a session was already running and was not interrupted");
            println!(
                "the supervisor starts the next one (at the next login, or now with: {})",
                start_command(platform, names, uid)
            );
        }
        InstallOutcome::ActivationUnconfirmed => {
            eprintln!(
                "roostctl session autostart: installed, but no supervised session could be \
                 confirmed — ask {} what happened",
                supervisor_label(platform, names)
            );
        }
    }
}

async fn uninstall() -> Result<i32> {
    let Some((platform, names, home, artifact)) = artifact_target()? else {
        return Ok(1);
    };
    let code = remove_artifact(platform, names, &artifact)?;
    // Only once this verb has done what it could: a refusal says nothing
    // about the other slot, since nothing was uninstalled either way.
    if code == 0 {
        if let Some(note) = sibling_report(
            platform,
            &names,
            &home,
            xdg_config_home().as_deref(),
            host_uid(),
        ) {
            eprintln!("{note}");
        }
    }
    Ok(code)
}

fn remove_artifact(platform: Platform, names: Names, artifact: &Path) -> Result<i32> {
    match uninstall_step(&read_artifact(artifact), &names) {
        UninstallStep::NothingToDo => {
            println!(
                "autostart: nothing to uninstall (no {})",
                artifact.display()
            );
            return Ok(0);
        }
        UninstallStep::RefuseUnreadable(reason) => {
            eprintln!(
                "roostctl session autostart: cannot read {}: {reason}; leaving it alone",
                artifact.display()
            );
            return Ok(1);
        }
        UninstallStep::RefuseForeign => {
            eprintln!(
                "roostctl session autostart: {} was not written by roostctl; leaving it alone",
                artifact.display()
            );
            return Ok(1);
        }
        UninstallStep::Remove => {}
    }

    let supervisor = HostSupervisor::new(platform, names);
    // `is_running`, not `is_loaded`: the sentence is about shells that
    // are about to be hung up, and a loaded-but-idle agent has none.
    if supervisor.is_running()? {
        println!(
            "stopping the supervised session ({})",
            supervisor_label(platform, &names)
        );
    } else {
        println!("the supervisor is not running a session; nothing to stop");
    }

    if let Err(failure) = deactivate(&supervisor) {
        eprintln!("roostctl session autostart: {failure}");
        return Ok(1);
    }
    std::fs::remove_file(artifact).with_context(|| format!("remove {}", artifact.display()))?;
    if let Err(failure) = finish_uninstall(&supervisor) {
        eprintln!(
            "roostctl session autostart: removed {}, but {failure}",
            artifact.display()
        );
        return Ok(1);
    }
    println!("autostart uninstalled ({})", artifact.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::ffi::{CString, OsStr};
    use std::os::unix::ffi::OsStrExt;

    fn release() -> Names {
        Names::for_build(false)
    }

    fn dev() -> Names {
        Names::for_build(true)
    }

    /// Records every command and answers from a queue; an exhausted
    /// queue answers exit 0 with no output.
    struct FakeSupervisor {
        platform: Platform,
        names: Names,
        uid: u32,
        calls: RefCell<Vec<String>>,
        answers: RefCell<VecDeque<CommandResult>>,
    }

    impl FakeSupervisor {
        /// Release names, so every assertion written against #438's
        /// strings keeps exercising them.
        fn new(platform: Platform) -> Self {
            Self::with_names(platform, release())
        }

        fn with_names(platform: Platform, names: Names) -> Self {
            Self {
                platform,
                names,
                uid: 501,
                calls: RefCell::new(Vec::new()),
                answers: RefCell::new(VecDeque::new()),
            }
        }

        fn answering(platform: Platform, answers: Vec<CommandResult>) -> Self {
            let fake = Self::new(platform);
            *fake.answers.borrow_mut() = answers.into();
            fake
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Supervisor for FakeSupervisor {
        fn platform(&self) -> Platform {
            self.platform
        }

        fn names(&self) -> &Names {
            &self.names
        }

        fn uid(&self) -> u32 {
            self.uid
        }

        fn run(&self, command: &Command) -> Result<CommandResult> {
            self.calls.borrow_mut().push(command.to_string());
            Ok(self
                .answers
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| exit(0, "")))
        }
    }

    fn exit(status: i32, stderr: &str) -> CommandResult {
        CommandResult {
            status: Some(status),
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    fn printed(stdout: &str) -> CommandResult {
        CommandResult {
            status: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn supervision(loaded_before: bool, running_after: bool) -> Supervision {
        Supervision {
            loaded_before,
            running_after,
        }
    }

    fn text_read(text: &str) -> ArtifactRead {
        ArtifactRead::Text(text.to_string())
    }

    fn ours(text: &str, names: &Names) -> Artifact {
        match parse_artifact(text, names) {
            Parsed::Ours(artifact) => artifact,
            other => panic!("expected ours, got {other:?}"),
        }
    }

    /// The exact bytes #438 shipped: the marker without a generation.
    /// This is the file a release install in the wild is upgraded from.
    fn unit_438(bin: &str) -> String {
        format!(
            "[Unit]\n\
             # Written by roostctl session autostart. Reinstalling replaces this file.\n\
             Description=Roost host session (roost-session)\n\
             \n\
             [Service]\n\
             Type=simple\n\
             WorkingDirectory=%h\n\
             ExecStart=\"{bin}\" start --foreground\n\
             Restart=on-failure\n\
             KillMode=mixed\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        )
    }

    fn plist_438(bin: &str, home: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<!-- Written by roostctl session autostart. Reinstalling replaces this file. -->
<dict>
    <key>Label</key>
    <string>ai.stridelabs.roost-session</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>WorkingDirectory</key>
    <string>{home}</string>
</dict>
</plist>
"#
        )
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "roostctl-autostart-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ------------------------------------------------------------------
    // Names
    // ------------------------------------------------------------------

    #[test]
    fn the_release_names_and_paths_are_the_ones_shipped_since_438() {
        let names = release();
        assert_eq!(names.stem(), "roost-session");
        assert_eq!(names.unit(), "roost-session.service");
        assert_eq!(names.label(), "ai.stridelabs.roost-session");
        assert_eq!(names.plist_file(), "ai.stridelabs.roost-session.plist");
        assert_eq!(
            names.description(),
            "Description=Roost host session (roost-session)"
        );
        assert_eq!(
            names.service_target(501),
            "gui/501/ai.stridelabs.roost-session"
        );

        assert_eq!(
            artifact_path(Platform::Linux, &names, Path::new("/home/u"), None),
            PathBuf::from("/home/u/.config/systemd/user/roost-session.service")
        );
        assert_eq!(
            artifact_path(Platform::MacOs, &names, Path::new("/Users/u"), None),
            PathBuf::from("/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session.plist")
        );

        assert_eq!(
            supervisor_label(Platform::Linux, &names),
            "systemd --user roost-session.service"
        );
        assert_eq!(
            supervisor_label(Platform::MacOs, &names),
            "launchd ai.stridelabs.roost-session"
        );

        let plist = Path::new("/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session.plist");
        assert_eq!(
            restart_command(Platform::Linux, &names, plist, 501),
            "systemctl --user restart roost-session"
        );
        assert_eq!(
            restart_command(Platform::MacOs, &names, plist, 501),
            "launchctl bootout gui/501/ai.stridelabs.roost-session && launchctl bootstrap \
             gui/501 /Users/u/Library/LaunchAgents/ai.stridelabs.roost-session.plist"
        );
        assert_eq!(
            start_command(Platform::Linux, &names, 501),
            "systemctl --user start roost-session"
        );
        assert_eq!(
            start_command(Platform::MacOs, &names, 501),
            "launchctl kickstart gui/501/ai.stridelabs.roost-session"
        );
    }

    #[test]
    fn the_dev_names_are_the_same_rule_over_the_dev_namespace() {
        let names = dev();
        assert_eq!(names.stem(), "roost-session-dev");
        assert_eq!(names.unit(), "roost-session-dev.service");
        assert_eq!(names.label(), "ai.stridelabs.roost-session-dev");
        assert_eq!(names.plist_file(), "ai.stridelabs.roost-session-dev.plist");
        assert_eq!(
            names.description(),
            "Description=Roost host session (roost-session-dev)"
        );
        assert_eq!(
            names.service_target(501),
            "gui/501/ai.stridelabs.roost-session-dev"
        );
        assert_eq!(
            artifact_path(Platform::Linux, &names, Path::new("/home/u"), None),
            PathBuf::from("/home/u/.config/systemd/user/roost-session-dev.service")
        );
        assert_eq!(
            artifact_path(Platform::MacOs, &names, Path::new("/Users/u"), None),
            PathBuf::from("/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session-dev.plist")
        );
        assert_eq!(
            supervisor_label(Platform::Linux, &names),
            "systemd --user roost-session-dev.service"
        );
        assert_eq!(
            start_command(Platform::Linux, &names, 501),
            "systemctl --user start roost-session-dev"
        );
        assert_eq!(
            start_command(Platform::MacOs, &names, 501),
            "launchctl kickstart gui/501/ai.stridelabs.roost-session-dev"
        );
    }

    #[test]
    fn other_round_trips_between_the_two_profiles() {
        assert_eq!(release().other(), dev());
        assert_eq!(dev().other(), release());
        assert_eq!(release().other().other(), release());
    }

    #[test]
    fn an_artifact_of_one_profile_is_foreign_to_the_other() {
        // The #438 shape: the release description, the unversioned
        // marker, an ExecStart into somebody's build tree.
        let build_tree = Path::new("/home/u/roost/target/debug/roost-session");
        let unit = render_unit(&release(), build_tree);
        assert_eq!(ours(&unit, &release()).binary, build_tree.to_path_buf());
        assert_eq!(parse_artifact(&unit, &dev()), Parsed::Foreign);

        let dev_unit = render_unit(&dev(), Path::new("/usr/bin/roost-session"));
        assert_eq!(parse_artifact(&dev_unit, &release()), Parsed::Foreign);
        assert!(matches!(parse_artifact(&dev_unit, &dev()), Parsed::Ours(_)));

        // The launchd twin, where the release label is a *prefix* of the
        // dev one: the `<string>` wrapper is what keeps them apart.
        let home = Path::new("/Users/u");
        let plist = render_plist(&release(), Path::new("/Apps/roost-session"), home);
        assert!(matches!(
            parse_artifact(&plist, &release()),
            Parsed::Ours(_)
        ));
        assert_eq!(parse_artifact(&plist, &dev()), Parsed::Foreign);
        let dev_plist = render_plist(&dev(), Path::new("/Apps/roost-session"), home);
        assert_eq!(parse_artifact(&dev_plist, &release()), Parsed::Foreign);
        assert!(matches!(
            parse_artifact(&dev_plist, &dev()),
            Parsed::Ours(_)
        ));

        // So a release-slot file hand-copied into the dev slot is a
        // foreign file to a dev install, not a definition to adopt.
        assert_eq!(
            classify_existing(
                &text_read(&unit),
                &render_unit(&dev(), Path::new("/usr/bin/roost-session")),
                &dev()
            ),
            Existing::Foreign
        );
    }

    #[test]
    fn the_sibling_note_names_only_the_other_slots_own_artifact() {
        let other = release();
        let path = Path::new("/home/u/.config/systemd/user/roost-session.service");
        let ours = render_unit(
            &other,
            Path::new("/home/u/roost/target/debug/roost-session"),
        );

        for read in [
            ArtifactRead::Absent,
            text_read("[Unit]\nDescription=Somebody else\n"),
            // The *dev* profile's unit sitting in the release slot is
            // still not the release profile's artifact.
            text_read(&render_unit(&dev(), Path::new("/usr/bin/roost-session"))),
            ArtifactRead::Unavailable("not a regular file".to_string()),
        ] {
            assert_eq!(
                sibling_notice(Platform::Linux, &other, path, &read, 1000),
                None,
                "{read:?}"
            );
        }

        assert_eq!(
            sibling_notice(Platform::Linux, &other, path, &text_read(&ours), 1000).unwrap(),
            "note: the release-slot artifact also exists \
             (/home/u/.config/systemd/user/roost-session.service → \
             /home/u/roost/target/debug/roost-session) and is left alone; to remove it: \
             systemctl --user disable --now roost-session.service && \
             rm /home/u/.config/systemd/user/roost-session.service"
        );

        // The mirror sentence, from a release build about the dev slot,
        // with launchd's own remedy.
        let dev_path =
            Path::new("/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session-dev.plist");
        let dev_plist = render_plist(
            &dev(),
            Path::new("/opt/roost-session"),
            Path::new("/Users/u"),
        );
        assert_eq!(
            sibling_notice(
                Platform::MacOs,
                &dev(),
                dev_path,
                &text_read(&dev_plist),
                501
            )
            .unwrap(),
            "note: the dev-slot artifact also exists \
             (/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session-dev.plist → \
             /opt/roost-session) and is left alone; to remove it: \
             launchctl bootout gui/501/ai.stridelabs.roost-session-dev && \
             rm /Users/u/Library/LaunchAgents/ai.stridelabs.roost-session-dev.plist"
        );
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    #[test]
    fn the_unit_renders_the_pinned_text_with_a_quoted_path() {
        // A space is legal precisely because the path is quoted.
        let rendered = render_unit(&release(), Path::new("/opt/my apps/roost-session"));
        assert_eq!(
            rendered,
            "[Unit]\n\
             # Written by roostctl session autostart (format 2). Reinstalling replaces this file.\n\
             Description=Roost host session (roost-session)\n\
             \n\
             [Service]\n\
             Type=simple\n\
             WorkingDirectory=%h\n\
             ExecStart=\"/opt/my apps/roost-session\" start --foreground\n\
             Restart=on-failure\n\
             KillMode=mixed\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        );
    }

    #[test]
    fn the_plist_renders_the_pinned_text_with_xml_escapes() {
        let rendered = render_plist(
            &release(),
            Path::new("/Apps/R&D <beta>/roost-session"),
            Path::new("/Users/a&b"),
        );
        assert_eq!(
            rendered,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<!-- Written by roostctl session autostart (format 2). Reinstalling replaces this file. -->
<dict>
    <key>Label</key>
    <string>ai.stridelabs.roost-session</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Apps/R&amp;D &lt;beta&gt;/roost-session</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>WorkingDirectory</key>
    <string>/Users/a&amp;b</string>
</dict>
</plist>
"#
        );
        // The escapes survive the round trip, so an escaped path still
        // reads back as ours.
        assert_eq!(
            ours(&rendered, &release()).binary,
            PathBuf::from("/Apps/R&D <beta>/roost-session")
        );
    }

    #[test]
    fn what_is_rendered_reads_back_at_this_builds_format() {
        assert_eq!(
            ours(
                &render_unit(&release(), Path::new("/usr/bin/roost-session")),
                &release()
            )
            .format,
            ARTIFACT_FORMAT
        );
        assert_eq!(
            ours(
                &render_plist(
                    &release(),
                    Path::new("/Apps/roost-session"),
                    Path::new("/Users/a")
                ),
                &release()
            )
            .format,
            ARTIFACT_FORMAT
        );
    }

    /// The pairs of lines that differ, which must be zipped over texts of
    /// the same shape to mean anything.
    fn differing_lines(before: &str, after: &str) -> Vec<(String, String)> {
        assert_eq!(
            before.lines().count(),
            after.lines().count(),
            "the two renders must have the same line count"
        );
        before
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn the_release_render_differs_from_438_in_the_marker_line_alone() {
        let bin = "/usr/bin/roost-session";
        assert_eq!(
            differing_lines(&unit_438(bin), &render_unit(&release(), Path::new(bin))),
            [(
                "# Written by roostctl session autostart. Reinstalling replaces this file."
                    .to_string(),
                "# Written by roostctl session autostart (format 2). Reinstalling replaces \
                 this file."
                    .to_string(),
            )]
        );

        let home = "/Users/u";
        assert_eq!(
            differing_lines(
                &plist_438(bin, home),
                &render_plist(&release(), Path::new(bin), Path::new(home))
            ),
            [(
                "<!-- Written by roostctl session autostart. Reinstalling replaces this file. -->"
                    .to_string(),
                "<!-- Written by roostctl session autostart (format 2). Reinstalling replaces \
                 this file. -->"
                    .to_string(),
            )]
        );
    }

    #[test]
    fn an_unsafe_unit_path_is_refused_by_name() {
        for (path, needle) in [
            ("/opt/roost\\session", "'\\\\'"),
            ("/opt/\"roost\"", "'\"'"),
            ("/opt/$HOME/roost-session", "'$'"),
            ("/opt/%h/roost-session", "'%'"),
            ("/opt/roost;rm", "';'"),
            ("/opt/roost\u{7}session", "'\\u{7}'"),
        ] {
            let error = check_unit_path(Path::new(path))
                .expect_err("an unquotable byte must be refused, not escaped");
            assert!(
                error.to_string().contains(needle),
                "{path}: {error} lacks {needle}"
            );
        }

        let non_utf8 = PathBuf::from(OsStr::from_bytes(b"/opt/ro\xffst-session"));
        let error = check_unit_path(&non_utf8).expect_err("a non-UTF-8 path cannot be written");
        assert!(error.to_string().contains("UTF-8"), "{error}");

        // The renderer seam refuses through the same check; a plist
        // takes a `$` happily, since quoting is systemd's problem alone.
        let home = Path::new("/home/u");
        assert!(render_artifact(
            Platform::Linux,
            &release(),
            Path::new("/opt/$HOME/roost"),
            home
        )
        .is_err());
        assert!(render_artifact(
            Platform::MacOs,
            &release(),
            Path::new("/opt/$HOME/roost"),
            home
        )
        .is_ok());
        assert!(render_artifact(Platform::MacOs, &release(), &non_utf8, home).is_err());
    }

    #[test]
    fn a_plist_path_is_refused_for_a_control_character_or_non_utf8() {
        let home = Path::new("/Users/u");
        let bin = Path::new("/Apps/roost-session");

        // XML 1.0 cannot write U+0007 at all, so this renders a plist
        // launchd rejects.
        let bell = PathBuf::from("/Apps/roost\u{7}session");
        let error = render_artifact(Platform::MacOs, &release(), &bell, home)
            .expect_err("a control character must be refused, not rendered");
        assert!(error.to_string().contains("'\\u{7}'"), "{error}");
        assert!(error.to_string().contains("/Apps/roost"), "{error}");

        // The working directory is rendered too, and is named by itself.
        let bad_home = PathBuf::from("/Users/u\u{1}");
        let error = render_artifact(Platform::MacOs, &release(), bin, &bad_home)
            .expect_err("the home path is rendered into the plist as well");
        assert!(error.to_string().contains("'\\u{1}'"), "{error}");
        assert!(error.to_string().contains("/Users/u"), "{error}");

        // A non-UTF-8 home would otherwise go through `to_string_lossy`
        // and name a *different* directory than the one it came from.
        let lossy_home = PathBuf::from(OsStr::from_bytes(b"/Users/\xffu"));
        let error = render_artifact(Platform::MacOs, &release(), bin, &lossy_home)
            .expect_err("a non-UTF-8 home must be refused rather than mangled");
        assert!(error.to_string().contains("UTF-8"), "{error}");

        // The escaping still carries what XML *can* represent.
        assert!(check_plist_path(Path::new("/Apps/R&D <beta>/roost-session")).is_ok());
        assert!(render_artifact(Platform::MacOs, &release(), bin, Path::new("/Users/a&b")).is_ok());
    }

    #[test]
    fn the_whole_safe_set_is_accepted() {
        let safe = "/usr/local/lib/roost-2.0+beta@x/dir,name~tmp:1/my app/roost-session";
        check_unit_path(Path::new(safe)).unwrap();
    }

    #[test]
    fn absolutize_joins_and_folds_lexically_without_resolving_symlinks() {
        let cwd = Path::new("/home/u/work");
        assert_eq!(
            absolutize(Path::new("target/debug/roost-session"), cwd),
            PathBuf::from("/home/u/work/target/debug/roost-session")
        );
        assert_eq!(
            absolutize(Path::new("../bin/./roost-session"), cwd),
            PathBuf::from("/home/u/bin/roost-session")
        );
        // Already absolute: only the components are folded.
        assert_eq!(
            absolutize(Path::new("/usr/bin/../lib/roost-session"), cwd),
            PathBuf::from("/usr/lib/roost-session")
        );
        // `..` cannot climb above the root.
        assert_eq!(
            absolutize(Path::new("/../../roost-session"), cwd),
            PathBuf::from("/roost-session")
        );

        // A real symlink stays itself — the whole point of not
        // canonicalizing.
        let root = scratch("absolutize-symlink");
        let target = root.join("roost-session-1.2.3");
        std::fs::write(&target, b"#!/bin/sh\n").unwrap();
        let link = root.join("roost-session");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(absolutize(&link, cwd), link);
    }

    // ------------------------------------------------------------------
    // Reading a file back
    // ------------------------------------------------------------------

    #[test]
    fn parse_artifact_reads_ours_and_a_hand_edited_copy_and_refuses_a_foreign_file() {
        let unit = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        assert_eq!(
            parse_artifact(&unit, &release()),
            Parsed::Ours(Artifact {
                platform: Platform::Linux,
                binary: PathBuf::from("/usr/bin/roost-session"),
                format: ARTIFACT_FORMAT,
            })
        );

        // A hand edit (an extra Environment= line) is still ours.
        let edited = unit.replace(
            "Restart=on-failure",
            "Environment=RUST_LOG=debug\nRestart=always",
        );
        assert_eq!(
            ours(&edited, &release()).binary,
            PathBuf::from("/usr/bin/roost-session")
        );

        let plist = render_plist(
            &release(),
            Path::new("/Apps/roost-session"),
            Path::new("/Users/a"),
        );
        assert_eq!(
            parse_artifact(&plist, &release()),
            Parsed::Ours(Artifact {
                platform: Platform::MacOs,
                binary: PathBuf::from("/Apps/roost-session"),
                format: ARTIFACT_FORMAT,
            })
        );

        for foreign in [
            "[Unit]\nDescription=Somebody else\n\n[Service]\nExecStart=/bin/true\n",
            // Our description, but not our program.
            &unit.replace("start --foreground", "serve"),
            // Our label, but the arguments are somebody else's.
            &plist.replace("<string>--foreground</string>", "<string>--daemon</string>"),
            "",
            "not an artifact at all",
        ] {
            assert_eq!(
                parse_artifact(foreign, &release()),
                Parsed::Foreign,
                "{foreign}"
            );
        }
    }

    #[test]
    fn parse_marker_reads_a_generation_off_a_whole_line_in_its_own_syntax() {
        let legacy_unit =
            "# Written by roostctl session autostart. Reinstalling replaces this file.";
        let legacy_plist =
            "<!-- Written by roostctl session autostart. Reinstalling replaces this file. -->";

        // Each syntax reads only its own comments: a plist marker has no
        // authority over a unit, nor the reverse.
        for (line, syntax) in [
            (legacy_unit.to_string(), MarkerSyntax::Plist),
            (legacy_plist.to_string(), MarkerSyntax::Unit),
            (unit_marker(2), MarkerSyntax::Plist),
            (plist_marker(2), MarkerSyntax::Unit),
        ] {
            assert_eq!(parse_marker(&line, syntax), None, "{line:?}");
        }

        // The spacing this command writes is part of the marker.
        for line in [
            "#Written by roostctl session autostart. Reinstalling replaces this file.",
            "<!--Written by roostctl session autostart. Reinstalling replaces this file.-->",
        ] {
            for syntax in [MarkerSyntax::Unit, MarkerSyntax::Plist] {
                assert_eq!(parse_marker(line, syntax), None, "{line:?}");
            }
        }

        for (line, expected) in [
            (legacy_unit.to_string(), Some(1)),
            (legacy_plist.to_string(), Some(1)),
            // Trailing whitespace survives an editor and is still ours.
            (format!("{legacy_unit}   "), Some(1)),
            (format!("  {legacy_plist}\t"), Some(1)),
            (unit_marker(2), Some(2)),
            (plist_marker(2), Some(2)),
            (unit_marker(7), Some(7)),
            (plist_marker(7), Some(7)),
            // A generation this command cannot place in the sequence.
            (unit_marker(0), None),
            (plist_marker(0), None),
            (
                "# Written by roostctl session autostart (format 99999999999). \
              Reinstalling replaces this file."
                    .to_string(),
                None,
            ),
            (
                "# Written by roostctl session autostart (format two). \
              Reinstalling replaces this file."
                    .to_string(),
                None,
            ),
            (
                "# Written by roostctl session autostart (format +2). \
              Reinstalling replaces this file."
                    .to_string(),
                None,
            ),
            (
                "# Written by roostctl session autostart (format 2] \
              Reinstalling replaces this file."
                    .to_string(),
                None,
            ),
            // Quoted inside a longer line, which is not a marker line.
            (format!("ExecStart=/bin/echo \"{legacy_unit}\""), None),
            (format!("{} extra", unit_marker(2)), None),
            (format!("prefix {}", plist_marker(2)), None),
            (
                "# Written by somebody else. Reinstalling replaces this file.".to_string(),
                None,
            ),
            (String::new(), None),
        ] {
            let syntax = if line.trim_start().starts_with("<!--") {
                MarkerSyntax::Plist
            } else {
                MarkerSyntax::Unit
            };
            assert_eq!(parse_marker(&line, syntax), expected, "{line:?}");
        }
    }

    #[test]
    fn a_marker_in_the_other_artifacts_syntax_does_not_make_a_file_ours() {
        // A hand-written unit carrying our description and exec tail,
        // "authorised" by a plist-syntax comment. systemd would ignore
        // the line entirely; so must this.
        let unit = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        let smuggled = unit.replace(
            &unit_marker(ARTIFACT_FORMAT),
            &plist_marker(ARTIFACT_FORMAT),
        );
        assert_eq!(parse_artifact(&smuggled, &release()), Parsed::Foreign);
        assert_eq!(
            write_step(
                classify_existing(&text_read(&smuggled), &unit, &release()),
                false
            ),
            WriteStep::RefuseForeign
        );

        // And a file carrying both syntaxes' markers is nobody's.
        let both = unit.replace(
            &unit_marker(ARTIFACT_FORMAT),
            &format!(
                "{}\n{}",
                unit_marker(ARTIFACT_FORMAT),
                plist_marker(ARTIFACT_FORMAT)
            ),
        );
        assert_eq!(parse_artifact(&both, &release()), Parsed::Foreign);
    }

    #[test]
    fn two_markers_disagreeing_about_the_generation_are_no_marker_at_all() {
        let unit = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        // The shape a half-applied hand edit leaves: the old sentence
        // kept, the new one pasted in beside it.
        let conflicted = unit.replace(
            &unit_marker(ARTIFACT_FORMAT),
            &format!(
                "# Written by roostctl session autostart. Reinstalling replaces this file.\n{}",
                unit_marker(ARTIFACT_FORMAT)
            ),
        );
        assert_eq!(parse_artifact(&conflicted, &release()), Parsed::Foreign);
        assert_eq!(
            classify_existing(&text_read(&conflicted), &unit, &release()),
            Existing::Foreign
        );

        // The same line twice says the same thing, and is still ours.
        let doubled = unit.replace(
            &unit_marker(ARTIFACT_FORMAT),
            &format!("{0}\n{0}", unit_marker(ARTIFACT_FORMAT)),
        );
        assert_eq!(ours(&doubled, &release()).format, ARTIFACT_FORMAT);
    }

    #[test]
    fn a_438_artifact_is_ours_at_format_1_and_upgrades_without_force() {
        let bin = "/usr/bin/roost-session";
        let home = "/Users/u";

        for (previous, rendered) in [
            (unit_438(bin), render_unit(&release(), Path::new(bin))),
            (
                plist_438(bin, home),
                render_plist(&release(), Path::new(bin), Path::new(home)),
            ),
        ] {
            let artifact = ours(&previous, &release());
            assert_eq!(artifact.format, 1, "{previous}");
            assert_eq!(artifact.binary, PathBuf::from(bin));

            let existing = classify_existing(&text_read(&previous), &rendered, &release());
            assert_eq!(existing, Existing::Outdated { found: 1 }, "{previous}");
            assert_eq!(write_step(existing, false), WriteStep::Write, "{previous}");
        }
    }

    #[test]
    fn a_format_1_file_in_the_dev_slot_is_the_dev_profiles_own() {
        // Synthetic: the format-1 files in the wild all carry the release
        // description, since #438 wrote one name for both profiles. This
        // is what a dev build would have to find to adopt one.
        let unit = unit_438("/home/u/roost/target/debug/roost-session")
            .replace("(roost-session)", "(roost-session-dev)");
        assert_eq!(ours(&unit, &dev()).format, 1);
        assert_eq!(parse_artifact(&unit, &release()), Parsed::Foreign);
        assert_eq!(
            classify_existing(
                &text_read(&unit),
                &render_unit(&dev(), Path::new("/x/roost-session")),
                &dev()
            ),
            Existing::Outdated { found: 1 }
        );
    }

    /// Every line but the marker, so a file can be made foreign without
    /// changing anything else about it.
    fn without_line(text: &str, marker: &str) -> String {
        let mut out = text
            .lines()
            .filter(|line| line.trim() != marker)
            .collect::<Vec<_>>()
            .join("\n");
        out.push('\n');
        out
    }

    #[test]
    fn ownership_hangs_on_the_marker_line_alone() {
        let unit = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        let plist = render_plist(
            &release(),
            Path::new("/Apps/roost-session"),
            Path::new("/Users/a"),
        );
        let unit_mark = unit_marker(ARTIFACT_FORMAT);
        let plist_mark = plist_marker(ARTIFACT_FORMAT);

        // What this command writes reads back as ours, and the marker
        // sits where the docs say it does.
        assert!(
            matches!(parse_artifact(&unit, &release()), Parsed::Ours(_)),
            "{unit}"
        );
        assert!(
            matches!(parse_artifact(&plist, &release()), Parsed::Ours(_)),
            "{plist}"
        );
        assert!(unit.contains(&format!("[Unit]\n{unit_mark}\n")), "{unit}");
        assert!(
            plist.contains(&format!("<plist version=\"1.0\">\n{plist_mark}\n")),
            "{plist}"
        );

        // The same text with the marker deleted is somebody else's file.
        for stripped in [
            without_line(&unit, &unit_mark),
            without_line(&plist, &plist_mark),
        ] {
            assert_eq!(
                parse_artifact(&stripped, &release()),
                Parsed::Foreign,
                "{stripped}"
            );
        }

        // The case the marker exists for: a foreign unit carrying our
        // Description and a `start --foreground` tail, which the shape
        // check alone would adopt and overwrite without `--force`.
        let impostor = "[Unit]\n\
                        Description=Roost host session (roost-session)\n\
                        \n\
                        [Service]\n\
                        ExecStart=\"/opt/other-daemon\" start --foreground\n";
        assert_eq!(parse_artifact(impostor, &release()), Parsed::Foreign);
        assert_eq!(
            classify_existing(&text_read(impostor), &unit, &release()),
            Existing::Foreign
        );
        assert_eq!(
            write_step(
                classify_existing(&text_read(impostor), &unit, &release()),
                false
            ),
            WriteStep::RefuseForeign
        );

        // Its launchd twin: our label, our arguments, no marker.
        let impostor_plist =
            without_line(&plist, &plist_mark).replace("/Apps/roost-session", "/opt/other-daemon");
        assert_eq!(parse_artifact(&impostor_plist, &release()), Parsed::Foreign);

        // And the same line settles a *newer* file before anything else
        // is looked at: a template from the future may have changed the
        // very parts the three-part rule reads, which would leave it
        // read as foreign and this message unreachable.
        let from_the_future = format!(
            "[Unit]\n{}\nDescription=whatever they renamed it to\n\
             \n[Service]\nExecCommand=who knows\n",
            unit_marker(9)
        );
        assert_eq!(
            parse_artifact(&from_the_future, &release()),
            Parsed::Newer { found: 9 }
        );
        let existing = classify_existing(&text_read(&from_the_future), &unit, &release());
        assert_eq!(existing, Existing::Newer { found: 9 });
        assert_eq!(
            write_step(existing, false),
            WriteStep::RefuseNewer { found: 9 }
        );
        assert_eq!(write_step(existing, true), WriteStep::Write);
    }

    #[test]
    fn installed_state_covers_every_state() {
        assert_eq!(
            installed_state(&ArtifactRead::Absent, Parsed::Foreign, false),
            AutostartState::NotInstalled
        );
        assert_eq!(
            installed_state(&text_read("nobody else's"), Parsed::Foreign, false),
            AutostartState::Foreign
        );
        assert_eq!(
            installed_state(
                &ArtifactRead::Unavailable("not a regular file".to_string()),
                Parsed::Foreign,
                false
            ),
            AutostartState::Unreadable("not a regular file".to_string())
        );
        assert_eq!(
            installed_state(&text_read("ours"), Parsed::Newer { found: 7 }, false),
            AutostartState::Newer { found: 7 }
        );
        let mine = Artifact {
            platform: Platform::Linux,
            binary: PathBuf::from("/usr/bin/roost-session"),
            format: ARTIFACT_FORMAT,
        };
        assert_eq!(
            installed_state(&text_read("ours"), Parsed::Ours(mine.clone()), true),
            AutostartState::Installed {
                binary: PathBuf::from("/usr/bin/roost-session"),
                format: ARTIFACT_FORMAT,
                binary_missing: false,
            }
        );
        assert_eq!(
            installed_state(
                &text_read("ours"),
                Parsed::Ours(Artifact { format: 1, ..mine }),
                false
            ),
            AutostartState::Installed {
                binary: PathBuf::from("/usr/bin/roost-session"),
                format: 1,
                binary_missing: true,
            }
        );
    }

    #[test]
    fn the_status_line_names_the_supervisor_the_binary_and_every_qualifier() {
        let artifact = Path::new("/home/u/.config/systemd/user/roost-session.service");
        let linux = |state: AutostartState| {
            render_status_line(Platform::Linux, &release(), artifact, &state)
        };

        assert_eq!(
            linux(AutostartState::NotInstalled),
            "autostart=not installed"
        );
        assert_eq!(
            linux(AutostartState::Installed {
                binary: PathBuf::from("/usr/bin/roost-session"),
                format: ARTIFACT_FORMAT,
                binary_missing: false,
            }),
            "autostart=installed on disk \
             (systemd --user roost-session.service → /usr/bin/roost-session)"
        );
        // Both qualifiers at once, in the order they are appended.
        assert_eq!(
            linux(AutostartState::Installed {
                binary: PathBuf::from("/usr/bin/roost-session"),
                format: 1,
                binary_missing: true,
            }),
            "autostart=installed on disk \
             (systemd --user roost-session.service → /usr/bin/roost-session) \
             (binary missing: /usr/bin/roost-session) \
             (format 1, outdated: rerun roostctl session autostart install)"
        );
        assert_eq!(
            linux(AutostartState::Newer { found: 7 }),
            "autostart=installed on disk \
             (systemd --user roost-session.service; format 7, newer than this roostctl)"
        );
        assert_eq!(
            linux(AutostartState::Foreign),
            "autostart=not installed (foreign file: not written by roostctl: \
             /home/u/.config/systemd/user/roost-session.service)"
        );
        assert_eq!(
            linux(AutostartState::Unreadable("not a regular file".to_string())),
            "autostart=not installed (unreadable: \
             /home/u/.config/systemd/user/roost-session.service: not a regular file)"
        );

        assert_eq!(
            render_status_line(
                Platform::MacOs,
                &release(),
                Path::new("/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session.plist"),
                &AutostartState::Installed {
                    binary: PathBuf::from(
                        "/Applications/Roost-Iced.app/Contents/MacOS/roost-session"
                    ),
                    format: ARTIFACT_FORMAT,
                    binary_missing: true,
                }
            ),
            "autostart=installed on disk (launchd ai.stridelabs.roost-session → \
             /Applications/Roost-Iced.app/Contents/MacOS/roost-session) \
             (binary missing: /Applications/Roost-Iced.app/Contents/MacOS/roost-session)"
        );
        assert_eq!(UNAVAILABLE_LINE, "autostart=unavailable");
    }

    #[test]
    fn the_artifact_path_follows_xdg_when_it_is_absolute_and_home_otherwise() {
        let home = Path::new("/home/u");
        assert_eq!(
            artifact_path(Platform::Linux, &release(), home, None),
            PathBuf::from("/home/u/.config/systemd/user/roost-session.service")
        );
        assert_eq!(
            artifact_path(Platform::Linux, &release(), home, Some(Path::new("/xdg"))),
            PathBuf::from("/xdg/systemd/user/roost-session.service")
        );
        // A relative XDG value is invalid per the spec, not a base dir.
        assert_eq!(
            artifact_path(Platform::Linux, &release(), home, Some(Path::new("conf"))),
            PathBuf::from("/home/u/.config/systemd/user/roost-session.service")
        );
        assert_eq!(
            artifact_path(
                Platform::MacOs,
                &release(),
                Path::new("/Users/u"),
                Some(Path::new("/xdg"))
            ),
            PathBuf::from("/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session.plist")
        );
    }

    // ------------------------------------------------------------------
    // The state machine
    // ------------------------------------------------------------------

    #[test]
    fn classify_existing_compares_bytes_only_for_a_file_of_ours_at_this_format() {
        let rendered = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        let classify = |read: &ArtifactRead| classify_existing(read, &rendered, &release());

        assert_eq!(classify(&ArtifactRead::Absent), Existing::Absent);
        assert_eq!(
            classify(&ArtifactRead::Unavailable("not a regular file".to_string())),
            Existing::Unreadable
        );
        assert_eq!(classify(&text_read(&rendered)), Existing::Identical);
        // Ours, this generation, different bytes — a hand edit or a
        // different binary, never an upgrade.
        assert_eq!(
            classify(&text_read(&render_unit(
                &release(),
                Path::new("/opt/roost-session")
            ))),
            Existing::Changed
        );
        assert_eq!(
            classify(&text_read(&rendered.replace(
                "Restart=on-failure",
                "Environment=RUST_LOG=debug\nRestart=on-failure"
            ))),
            Existing::Changed
        );
        assert_eq!(
            classify(&text_read(&unit_438("/usr/bin/roost-session"))),
            Existing::Outdated { found: 1 }
        );
        assert_eq!(
            classify(&text_read(&format!(
                "[Unit]\n{}\n{}\n\n[Service]\nExecStart=\"/x\" start --foreground\n",
                unit_marker(9),
                release().description()
            ))),
            Existing::Newer { found: 9 }
        );
        assert_eq!(
            classify(&text_read("[Unit]\nDescription=Someone else\n")),
            Existing::Foreign
        );
    }

    #[test]
    fn a_file_this_install_may_not_replace_is_refused_and_identical_bytes_are_left_alone() {
        assert_eq!(
            write_step(Existing::Foreign, false),
            WriteStep::RefuseForeign
        );
        assert_eq!(write_step(Existing::Foreign, true), WriteStep::Write);
        assert_eq!(
            write_step(Existing::Newer { found: 3 }, false),
            WriteStep::RefuseNewer { found: 3 }
        );
        assert_eq!(
            write_step(Existing::Newer { found: 3 }, true),
            WriteStep::Write
        );
        // Nothing readable to echo and nothing to compare: `--force` has
        // no meaning over a file this process could not read.
        for force in [false, true] {
            assert_eq!(
                write_step(Existing::Unreadable, force),
                WriteStep::RefuseUnreadable
            );
            // `--force` has nothing to do with our own files.
            assert_eq!(write_step(Existing::Identical, force), WriteStep::Keep);
            assert_eq!(write_step(Existing::Changed, force), WriteStep::Write);
            assert_eq!(write_step(Existing::Absent, force), WriteStep::Write);
            assert_eq!(
                write_step(Existing::Outdated { found: 1 }, force),
                WriteStep::Write
            );
        }
    }

    #[test]
    fn an_uninstall_removes_a_newer_artifact_and_leaves_what_is_not_ours() {
        let unit = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        let newer = format!(
            "[Unit]\n{}\nDescription=whatever they renamed it to\n",
            unit_marker(9)
        );
        for (read, expected) in [
            (ArtifactRead::Absent, UninstallStep::NothingToDo),
            (
                ArtifactRead::Unavailable("not a regular file".to_string()),
                UninstallStep::RefuseUnreadable("not a regular file".to_string()),
            ),
            (
                text_read("[Unit]\nDescription=Someone else\n"),
                UninstallStep::RefuseForeign,
            ),
            // The dev profile's unit in the release slot is not ours.
            (
                text_read(&render_unit(&dev(), Path::new("/x/roost-session"))),
                UninstallStep::RefuseForeign,
            ),
            (text_read(&unit), UninstallStep::Remove),
            (
                text_read(&unit_438("/usr/bin/roost-session")),
                UninstallStep::Remove,
            ),
            (text_read(&newer), UninstallStep::Remove),
        ] {
            assert_eq!(uninstall_step(&read, &release()), expected, "{read:?}");
        }
    }

    #[test]
    fn classify_install_covers_every_outcome() {
        let untouched = supervision(false, false);
        // Identical bytes settle before anything is asked — but only
        // while the supervisor holds the definition.
        assert_eq!(
            classify_install(Existing::Identical, supervision(true, true), None, None),
            InstallOutcome::Unchanged
        );
        // Identical bytes the supervisor does NOT hold are the state an
        // earlier failed activation leaves behind: activate, do not
        // report success having done nothing.
        assert_eq!(
            classify_install(
                Existing::Identical,
                supervision(false, true),
                None,
                Some("s1")
            ),
            InstallOutcome::SupervisedFresh
        );
        assert_eq!(
            classify_install(Existing::Identical, untouched, None, None),
            InstallOutcome::ActivationUnconfirmed
        );
        // A loaded supervisor is left alone, whatever was on disk — an
        // upgrade and a forced downgrade are rewrites like any other.
        for existing in [
            Existing::Absent,
            Existing::Changed,
            Existing::Foreign,
            Existing::Outdated { found: 1 },
            Existing::Newer { found: 9 },
        ] {
            assert_eq!(
                classify_install(existing, supervision(true, true), Some("old"), Some("old")),
                InstallOutcome::ReinstalledLoaded,
                "{existing:?}"
            );
        }
        // Re-install over a launchd agent that is loaded but idle: it is
        // loaded, so `bootstrap` would fail — the file is rewritten and
        // the supervisor is left for the printed restart, not retried
        // into a bogus `Unchanged` on the next attempt.
        assert_eq!(
            classify_install(
                Existing::Changed,
                supervision(true, false),
                Some("s1"),
                Some("s1")
            ),
            InstallOutcome::ReinstalledLoaded
        );
        // Fresh: nothing before, a new id after, the unit active.
        assert_eq!(
            classify_install(Existing::Absent, supervision(false, true), None, Some("s2")),
            InstallOutcome::SupervisedFresh
        );
        // A session was already running: same id, unit stopped-on-purpose.
        assert_eq!(
            classify_install(Existing::Absent, untouched, Some("s1"), Some("s1")),
            InstallOutcome::AlreadyRunningUntouched
        );
        // Nothing answered, or the unit is inactive while a *different*
        // session answers, or it is active while the old one does.
        assert_eq!(
            classify_install(Existing::Absent, untouched, None, None),
            InstallOutcome::ActivationUnconfirmed
        );
        assert_eq!(
            classify_install(Existing::Absent, untouched, None, Some("s2")),
            InstallOutcome::ActivationUnconfirmed
        );
        assert_eq!(
            classify_install(
                Existing::Absent,
                supervision(false, true),
                Some("s1"),
                Some("s1")
            ),
            InstallOutcome::ActivationUnconfirmed
        );
        // The two "supervisor untouched" cases agree with the predicate
        // the installer branches on.
        assert_eq!(
            settled_without_supervisor(Existing::Identical, true),
            Some(InstallOutcome::Unchanged)
        );
        assert_eq!(settled_without_supervisor(Existing::Identical, false), None);
        assert_eq!(
            settled_without_supervisor(Existing::Changed, true),
            Some(InstallOutcome::ReinstalledLoaded)
        );
        assert_eq!(settled_without_supervisor(Existing::Changed, false), None);
        assert_eq!(settled_without_supervisor(Existing::Absent, false), None);
        assert_eq!(
            settled_without_supervisor(Existing::Outdated { found: 1 }, false),
            None
        );
        assert_eq!(
            settled_without_supervisor(Existing::Newer { found: 9 }, true),
            Some(InstallOutcome::ReinstalledLoaded)
        );
    }

    #[test]
    fn already_gone_matches_only_the_named_conditions() {
        assert!(already_gone(
            Platform::Linux,
            Some(1),
            "Failed to disable unit: Unit file roost-session.service does not exist."
        ));
        assert!(already_gone(
            Platform::Linux,
            Some(1),
            "Unit roost-session.service not loaded."
        ));
        // A real failure surfaces.
        assert!(!already_gone(Platform::Linux, Some(1), "Access denied"));
        assert!(!already_gone(Platform::Linux, Some(4), "does not exist"));
        assert!(!already_gone(Platform::Linux, None, "does not exist"));

        assert!(already_gone(
            Platform::MacOs,
            Some(3),
            "Boot-out failed: 3: No such process"
        ));
        assert!(already_gone(
            Platform::MacOs,
            Some(113),
            "Could not find service \"ai.stridelabs.roost-session\" in domain for gui"
        ));
        assert!(!already_gone(Platform::MacOs, Some(0), "No such process"));
        assert!(!already_gone(
            Platform::MacOs,
            Some(5),
            "Input/output error"
        ));
        assert!(!already_gone(Platform::MacOs, None, "No such process"));
    }

    // ------------------------------------------------------------------
    // The supervisor seam
    // ------------------------------------------------------------------

    #[test]
    fn is_loaded_reads_the_supervisors_own_answer() {
        // Linux has no loaded-but-idle state for this unit: `is-active`
        // answers both questions.
        let linux = FakeSupervisor::answering(Platform::Linux, vec![exit(0, ""), exit(0, "")]);
        assert!(linux.is_loaded().unwrap());
        assert!(linux.is_running().unwrap());
        assert_eq!(
            linux.calls(),
            [
                "systemctl --user is-active roost-session.service",
                "systemctl --user is-active roost-session.service",
            ]
        );

        let stopped = FakeSupervisor::answering(Platform::Linux, vec![exit(3, ""), exit(3, "")]);
        assert!(!stopped.is_loaded().unwrap());
        assert!(!stopped.is_running().unwrap());

        // A launchd label that is loaded but not running exits 0 with no
        // pid: loaded (so `bootstrap` would fail), not running.
        let idle = "ai.stridelabs.roost-session = {\n\tstate = not running\n}";
        let mac_loaded =
            FakeSupervisor::answering(Platform::MacOs, vec![printed(idle), printed(idle)]);
        assert!(mac_loaded.is_loaded().unwrap());
        assert!(!mac_loaded.is_running().unwrap());
        assert_eq!(
            mac_loaded.calls(),
            [
                "launchctl print gui/501/ai.stridelabs.roost-session",
                "launchctl print gui/501/ai.stridelabs.roost-session",
            ]
        );

        let alive = "ai.stridelabs.roost-session = {\n\tpid = 4321\n}";
        let mac_running =
            FakeSupervisor::answering(Platform::MacOs, vec![printed(alive), printed(alive)]);
        assert!(mac_running.is_loaded().unwrap());
        assert!(mac_running.is_running().unwrap());

        // Nothing bootstrapped at all: `print` fails.
        let mac_absent = FakeSupervisor::answering(
            Platform::MacOs,
            vec![
                exit(113, "Could not find service"),
                exit(113, "Could not find service"),
            ],
        );
        assert!(!mac_absent.is_loaded().unwrap());
        assert!(!mac_absent.is_running().unwrap());
    }

    #[test]
    fn an_install_runs_the_supervisor_commands_in_order() {
        let artifact = Path::new("/home/u/.config/systemd/user/roost-session.service");
        // `is-active` says inactive, so the install goes on to load it.
        let linux = FakeSupervisor::answering(Platform::Linux, vec![exit(3, "inactive")]);
        assert!(!linux.is_loaded().unwrap());
        activate(&linux, artifact).unwrap();
        assert_eq!(
            linux.calls(),
            [
                "systemctl --user is-active roost-session.service",
                "systemctl --user daemon-reload",
                "systemctl --user enable --now roost-session.service",
            ]
        );

        let plist = Path::new("/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session.plist");
        let mac = FakeSupervisor::new(Platform::MacOs);
        activate(&mac, plist).unwrap();
        assert_eq!(
            mac.calls(),
            [format!("launchctl bootstrap gui/501 {}", plist.display())]
        );
    }

    /// The whole command sequence an install runs after the bytes are
    /// settled, for each shape the write can take.
    fn install_calls(
        platform: Platform,
        wrote: bool,
        loaded: bool,
        artifact: &Path,
    ) -> Vec<String> {
        let mut answers = Vec::new();
        if platform == Platform::Linux && wrote {
            answers.push(exit(0, "")); // the reload, which runs first
        }
        answers.push(if loaded {
            exit(0, "")
        } else {
            exit(3, "inactive")
        });
        let fake = FakeSupervisor::answering(platform, answers);
        reload_after_write(&fake, wrote).unwrap();
        if !fake.is_loaded().unwrap() {
            activate(&fake, artifact).unwrap();
        }
        fake.calls()
    }

    #[test]
    fn every_linux_write_reloads_the_definition_before_anything_reads_it() {
        let unit = Path::new("/home/u/.config/systemd/user/roost-session.service");

        // The upgrade over a loaded unit: the manager is told the bytes
        // changed, and nothing touches the running process — so the
        // `systemctl --user restart` the report prints re-runs the NEW
        // ExecStart.
        assert_eq!(
            install_calls(Platform::Linux, true, true, unit),
            [
                "systemctl --user daemon-reload",
                "systemctl --user is-active roost-session.service",
            ]
        );

        // A write the supervisor does not hold: reloaded twice, once
        // here and once inside `activate` — see `reload_after_write`.
        assert_eq!(
            install_calls(Platform::Linux, true, false, unit),
            [
                "systemctl --user daemon-reload",
                "systemctl --user is-active roost-session.service",
                "systemctl --user daemon-reload",
                "systemctl --user enable --now roost-session.service",
            ]
        );

        // Nothing written (identical bytes the supervisor never loaded):
        // the fresh path's own two commands and no more.
        assert_eq!(
            install_calls(Platform::Linux, false, false, unit),
            [
                "systemctl --user is-active roost-session.service",
                "systemctl --user daemon-reload",
                "systemctl --user enable --now roost-session.service",
            ]
        );

        // launchd re-reads the plist on `bootstrap`, so a write adds
        // nothing on macOS.
        let plist = Path::new("/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session.plist");
        assert_eq!(
            install_calls(Platform::MacOs, true, true, plist),
            ["launchctl print gui/501/ai.stridelabs.roost-session"]
        );
        assert_eq!(
            install_calls(Platform::MacOs, true, false, plist),
            [
                "launchctl print gui/501/ai.stridelabs.roost-session".to_string(),
                format!("launchctl bootstrap gui/501 {}", plist.display()),
            ]
        );
    }

    #[test]
    fn a_failed_reload_is_reported_the_way_an_activation_failure_is() {
        let broken = FakeSupervisor::answering(
            Platform::Linux,
            vec![exit(1, "Failed to reload daemon: Connection refused")],
        );
        let failure =
            reload_after_write(&broken, true).expect_err("a failed daemon-reload must surface");
        assert_eq!(
            format!("installed; activation failed: {failure}"),
            "installed; activation failed: systemctl --user daemon-reload: \
             Failed to reload daemon: Connection refused"
        );
    }

    #[test]
    fn an_uninstall_runs_the_supervisor_commands_in_order_around_the_removal() {
        let linux = FakeSupervisor::new(Platform::Linux);
        deactivate(&linux).unwrap();
        // The file removal happens here, between the two halves.
        finish_uninstall(&linux).unwrap();
        assert_eq!(
            linux.calls(),
            [
                "systemctl --user disable --now roost-session.service",
                "systemctl --user daemon-reload",
            ]
        );

        // An already-gone unit is the state the caller asked for.
        let gone = FakeSupervisor::answering(
            Platform::Linux,
            vec![exit(1, "Unit roost-session.service does not exist.")],
        );
        deactivate(&gone).unwrap();

        let mac = FakeSupervisor::new(Platform::MacOs);
        deactivate(&mac).unwrap();
        finish_uninstall(&mac).unwrap();
        assert_eq!(
            mac.calls(),
            ["launchctl bootout gui/501/ai.stridelabs.roost-session"]
        );

        // Anything else surfaces, named, with its stderr.
        let refused =
            FakeSupervisor::answering(Platform::MacOs, vec![exit(1, "Operation not permitted")]);
        let failure = deactivate(&refused).expect_err("an unexpected failure must surface");
        assert_eq!(
            failure.to_string(),
            "launchctl bootout gui/501/ai.stridelabs.roost-session: Operation not permitted"
        );
    }

    #[test]
    fn a_dev_build_drives_the_dev_artifact_end_to_end() {
        let artifact = PathBuf::from("/home/u/.config/systemd/user").join(dev().unit());
        let linux = FakeSupervisor::with_names(Platform::Linux, dev());
        assert!(linux.is_loaded().unwrap());
        activate(&linux, &artifact).unwrap();
        deactivate(&linux).unwrap();
        finish_uninstall(&linux).unwrap();
        assert_eq!(
            linux.calls(),
            [
                "systemctl --user is-active roost-session-dev.service",
                "systemctl --user daemon-reload",
                "systemctl --user enable --now roost-session-dev.service",
                "systemctl --user disable --now roost-session-dev.service",
                "systemctl --user daemon-reload",
            ]
        );

        let plist = PathBuf::from("/Users/u/Library/LaunchAgents").join(dev().plist_file());
        let mac = FakeSupervisor::with_names(Platform::MacOs, dev());
        assert!(mac.is_loaded().unwrap());
        activate(&mac, &plist).unwrap();
        deactivate(&mac).unwrap();
        finish_uninstall(&mac).unwrap();
        assert_eq!(
            mac.calls(),
            [
                "launchctl print gui/501/ai.stridelabs.roost-session-dev".to_string(),
                format!("launchctl bootstrap gui/501 {}", plist.display()),
                "launchctl bootout gui/501/ai.stridelabs.roost-session-dev".to_string(),
            ]
        );
    }

    #[test]
    fn a_supervisor_failure_after_the_write_keeps_the_file() {
        let root = scratch("activation-failure");
        let artifact = root.join("systemd/user/roost-session.service");
        let text = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        write_artifact(&artifact, &text).unwrap();

        let broken = FakeSupervisor::answering(
            Platform::Linux,
            vec![exit(0, ""), exit(1, "Failed to enable unit: Bad message")],
        );
        let failure = activate(&broken, &artifact).expect_err("the enable failed");
        let printed = format!("installed; activation failed: {failure}");
        assert!(printed.contains("systemctl --user enable --now roost-session.service"));
        assert!(printed.contains("Bad message"), "{printed}");

        // The file is kept, so a retry is `install` again and `status`
        // still reports what roost owns.
        let kept = std::fs::read_to_string(&artifact).unwrap();
        assert_eq!(kept, text);
        assert_eq!(
            installed_state(
                &ArtifactRead::Text(kept.clone()),
                parse_artifact(&kept, &release()),
                false
            ),
            AutostartState::Installed {
                binary: PathBuf::from("/usr/bin/roost-session"),
                format: ARTIFACT_FORMAT,
                binary_missing: true,
            }
        );
    }

    #[test]
    fn read_artifact_takes_a_plain_file_of_sensible_size_and_nothing_else() {
        let dir = scratch("read-artifact");

        assert_eq!(read_artifact(&dir.join("absent")), ArtifactRead::Absent);

        let unit = dir.join(release().unit());
        let text = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        std::fs::write(&unit, &text).unwrap();
        assert_eq!(read_artifact(&unit), ArtifactRead::Text(text));

        // Opening a FIFO for reading blocks until somebody writes to it
        // unless the open is non-blocking; this would hang otherwise.
        let fifo = dir.join("fifo");
        let raw = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path this test owns, and a mode.
        assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o644) }, 0);
        for not_a_file in [&fifo, &dir] {
            let ArtifactRead::Unavailable(reason) = read_artifact(not_a_file) else {
                panic!("{} must not be read", not_a_file.display());
            };
            assert_eq!(reason, "not a regular file");
        }

        let big = dir.join("big");
        std::fs::write(&big, vec![b'x'; ARTIFACT_READ_CAP as usize + 1]).unwrap();
        let ArtifactRead::Unavailable(reason) = read_artifact(&big) else {
            panic!("a file past the cap must not be read");
        };
        assert_eq!(reason, "larger than 64 KiB");

        // Exactly at the cap is still an artifact.
        std::fs::write(&big, vec![b'x'; ARTIFACT_READ_CAP as usize]).unwrap();
        assert!(matches!(read_artifact(&big), ArtifactRead::Text(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sibling_report_resolves_the_other_slot_from_this_builds_names() {
        // A #438-style unit in the release slot, seen from a dev build:
        // the path is the *release* one, parsed with the release names.
        let home = scratch("sibling-report");
        let release_unit = artifact_path(Platform::Linux, &release(), &home, None);
        std::fs::create_dir_all(release_unit.parent().unwrap()).unwrap();
        std::fs::write(
            &release_unit,
            render_unit(
                &release(),
                Path::new("/old/worktree/target/debug/roost-session"),
            ),
        )
        .unwrap();

        let note = sibling_report(Platform::Linux, &dev(), &home, None, 1000)
            .expect("a dev build must report the release slot");
        assert!(note.contains(&release_unit.display().to_string()), "{note}");
        assert!(
            note.contains("/old/worktree/target/debug/roost-session"),
            "{note}"
        );
        assert!(note.contains("release-slot"), "{note}");

        // The release build looks at the dev slot, which is empty.
        assert_eq!(
            sibling_report(Platform::Linux, &release(), &home, None, 1000),
            None
        );

        // And the report never reads this build's own slot: a dev unit
        // beside it changes nothing about what the dev build says.
        let dev_unit = artifact_path(Platform::Linux, &dev(), &home, None);
        std::fs::write(
            &dev_unit,
            render_unit(&dev(), Path::new("/x/roost-session")),
        )
        .unwrap();
        let again = sibling_report(Platform::Linux, &dev(), &home, None, 1000).unwrap();
        assert_eq!(again, note);
        let from_release = sibling_report(Platform::Linux, &release(), &home, None, 1000).unwrap();
        assert!(from_release.contains("dev-slot"), "{from_release}");
        assert!(
            from_release.contains(&dev_unit.display().to_string()),
            "{from_release}"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_pasteable_path_is_quoted_only_when_the_shell_would_split_it() {
        assert_eq!(
            shell_word(Path::new("/home/u/.config/x.service")),
            "/home/u/.config/x.service"
        );
        assert_eq!(
            shell_word(Path::new("/Users/a b/x.plist")),
            "'/Users/a b/x.plist'"
        );
        assert_eq!(shell_word(Path::new("/o'k/x")), "'/o'\\''k/x'");
        assert_eq!(shell_word(Path::new("/x/$HOME/y")), "'/x/$HOME/y'");
        assert!(sibling_notice(
            Platform::Linux,
            &release(),
            Path::new("/home/a b/.config/systemd/user/roost-session.service"),
            &text_read(&render_unit(
                &release(),
                Path::new("/usr/bin/roost-session")
            )),
            1000,
        )
        .unwrap()
        .ends_with("&& rm '/home/a b/.config/systemd/user/roost-session.service'"));
    }

    #[test]
    fn an_unreadable_artifact_is_reported_rather_than_read_as_absent() {
        let artifact = Path::new("/home/u/.config/systemd/user/roost-session.service");
        let state = installed_state(
            &ArtifactRead::Unavailable("not a regular file".to_string()),
            Parsed::Foreign,
            false,
        );
        assert_eq!(
            state,
            AutostartState::Unreadable("not a regular file".to_string())
        );
        assert_eq!(
            render_status_line(Platform::Linux, &release(), artifact, &state),
            "autostart=not installed (unreadable: \
             /home/u/.config/systemd/user/roost-session.service: not a regular file)"
        );
    }

    #[test]
    fn the_artifact_is_written_atomically_at_0644_under_a_0755_directory() {
        use std::os::unix::fs::PermissionsExt;

        let unit_name = release().unit();
        let root = scratch("write-mode");
        let artifact = root.join("systemd/user").join(&unit_name);
        let text = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        write_artifact(&artifact, &text).unwrap();
        assert_eq!(std::fs::read_to_string(&artifact).unwrap(), text);
        let mode = std::fs::metadata(&artifact).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "{mode:o}");
        let dir_mode = std::fs::metadata(artifact.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o755, "{dir_mode:o}");

        // A rewrite leaves no tmp file behind.
        write_artifact(&artifact, "[Unit]\n").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(artifact.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != OsStr::new(&unit_name))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn an_existing_artifact_directory_keeps_the_mode_it_had() {
        use std::os::unix::fs::PermissionsExt;

        let unit_name = release().unit();
        let root = scratch("dir-mode");
        let dir = root.join("systemd/user");
        std::fs::create_dir_all(&dir).unwrap();
        // `~/.config/systemd/user` is often deliberately 0700; installing
        // a unit into it must not widen it.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let text = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        write_artifact(&dir.join(&unit_name), &text).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        assert_eq!(std::fs::read_to_string(dir.join(&unit_name)).unwrap(), text);
    }

    #[test]
    fn a_write_that_fails_takes_its_tmp_file_with_it() {
        use std::os::unix::fs::PermissionsExt;

        fn tmps(dir: &Path) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect();
            names.sort();
            names
        }

        let unit_name = release().unit();
        let dir = scratch("write-failure");
        let artifact = dir.join(&unit_name);
        let text = render_unit(&release(), Path::new("/usr/bin/roost-session"));

        // The rename cannot land on a directory (EISDIR), and the write
        // and chmod before it both succeed — so a tmp file exists at the
        // moment of failure.
        std::fs::create_dir(&artifact).unwrap();
        write_artifact(&artifact, &text).expect_err("a rename onto a directory must fail");
        assert_eq!(tmps(&dir), Vec::<String>::new());
        std::fs::remove_dir(&artifact).unwrap();

        // SAFETY: a plain getter with no arguments. Root ignores the mode
        // bits the next case leans on, so it only runs as a real user.
        if unsafe { libc::geteuid() } != 0 {
            // A stale, unwritable tmp from an earlier crash: the write
            // fails, and the leftover goes with the failure.
            let tmp = dir.join(format!(".{unit_name}.{}.tmp", std::process::id()));
            std::fs::write(&tmp, "stale").unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444)).unwrap();
            write_artifact(&artifact, &text).expect_err("an unwritable tmp must fail the write");
            assert_eq!(tmps(&dir), Vec::<String>::new());
            assert!(!artifact.exists(), "a failed write must not leave a file");
        }
    }

    /// systemd and launchd translate the very phrases [`already_gone`]
    /// matches, so the real runner pins the locale those phrases belong
    /// to. Asserted through a real subprocess, since the environment is
    /// only set inside [`HostSupervisor::run`].
    #[test]
    fn a_supervisor_command_runs_under_the_c_locale() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("locale");
        let script = dir.join("echo-locale");
        std::fs::write(&script, "#!/bin/sh\necho \"LC_ALL=$LC_ALL LANG=$LANG\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let host = HostSupervisor::new(Platform::Linux, release());
        let result = host
            .run(&Command::new(&script.to_string_lossy(), &[]))
            .unwrap();

        assert!(result.ok(), "{result:?}");
        assert_eq!(result.stdout, "LC_ALL=C LANG=C\n");
    }

    // ------------------------------------------------------------------
    // Lingering
    // ------------------------------------------------------------------

    /// Every way an install can end, including the `None` that covers a
    /// refusal before or at the write and an activation that failed.
    const ENDINGS: [Option<InstallOutcome>; 6] = [
        None,
        Some(InstallOutcome::Unchanged),
        Some(InstallOutcome::ReinstalledLoaded),
        Some(InstallOutcome::SupervisedFresh),
        Some(InstallOutcome::AlreadyRunningUntouched),
        Some(InstallOutcome::ActivationUnconfirmed),
    ];

    #[test]
    fn the_linger_step_is_the_outcome_crossed_with_the_flag() {
        let table = [
            (None, LingerStep::Skip, LingerStep::Skip),
            (
                Some(InstallOutcome::Unchanged),
                LingerStep::Report,
                LingerStep::GrantThenReport,
            ),
            (
                Some(InstallOutcome::ReinstalledLoaded),
                LingerStep::Report,
                LingerStep::GrantThenReport,
            ),
            (
                Some(InstallOutcome::SupervisedFresh),
                LingerStep::Report,
                LingerStep::GrantThenReport,
            ),
            (
                Some(InstallOutcome::AlreadyRunningUntouched),
                LingerStep::Report,
                LingerStep::GrantThenReport,
            ),
            (
                Some(InstallOutcome::ActivationUnconfirmed),
                LingerStep::Report,
                LingerStep::Report,
            ),
        ];
        assert_eq!(table.len(), ENDINGS.len());
        for (outcome, without, with) in table {
            assert_eq!(
                linger_step(Platform::Linux, outcome, false),
                without,
                "{outcome:?} without the flag"
            );
            assert_eq!(
                linger_step(Platform::Linux, outcome, true),
                with,
                "{outcome:?} with the flag"
            );
        }
    }

    #[test]
    fn macos_never_lingers_whatever_the_install_did() {
        for outcome in ENDINGS {
            for flag in [false, true] {
                assert_eq!(
                    linger_step(Platform::MacOs, outcome, flag),
                    LingerStep::Skip,
                    "{outcome:?} with the flag {flag}"
                );
            }
        }
    }

    #[test]
    fn only_macos_refuses_the_linger_flag() {
        assert_eq!(linger_refusal(Platform::MacOs, false), None);
        assert_eq!(linger_refusal(Platform::Linux, true), None);
        assert_eq!(
            linger_refusal(Platform::MacOs, true),
            Some(
                "roostctl session autostart: --linger is a systemd-logind concept; a LaunchAgent \
                 starts at your next login and macOS has no equivalent \
                 (see the host-sessions guide)"
            )
        );
    }

    fn unknown(reason: &str) -> LingerState {
        LingerState::Unknown(reason.to_string())
    }

    #[test]
    fn parse_linger_reads_show_users_answer_and_every_way_it_can_fail() {
        assert_eq!(parse_linger(&Ok(printed("yes\n"))), LingerState::Yes);
        assert_eq!(parse_linger(&Ok(printed("no\n"))), LingerState::No);
        assert_eq!(parse_linger(&Ok(printed("  yes \n\n"))), LingerState::Yes);
        assert_eq!(parse_linger(&Ok(printed("\t no  "))), LingerState::No);

        // No loginctl at all — the seam's own error, not a state.
        let missing: Result<CommandResult> = Err(anyhow::anyhow!(
            "run `loginctl show-user 1000 --property=Linger --value --no-ask-password`: \
             No such file or directory (os error 2)"
        ));
        assert_eq!(
            parse_linger(&missing),
            unknown(
                "loginctl could not be run: run `loginctl show-user 1000 --property=Linger \
                 --value --no-ask-password`: No such file or directory (os error 2)"
            )
        );

        // A nonzero exit says what it said, trimmed.
        for stderr in [
            "Failed to look up user 99999: No such process",
            "Failed to connect to bus: No medium found",
            "User ID 1000 is not logged in or lingering",
        ] {
            assert_eq!(
                parse_linger(&Ok(exit(1, &format!("{stderr}\n")))),
                unknown(stderr)
            );
        }
        assert_eq!(
            parse_linger(&Ok(exit(4, "  "))),
            unknown("loginctl exited 4")
        );

        // Exit 0 saying something this command cannot place.
        assert_eq!(
            parse_linger(&Ok(printed("maybe\n"))),
            unknown("unexpected loginctl answer: \"maybe\"")
        );
        assert_eq!(
            parse_linger(&Ok(printed(""))),
            unknown("unexpected loginctl answer: \"\"")
        );
        assert_eq!(
            parse_linger(&Ok(printed("yes\nlinger=no"))),
            unknown("unexpected loginctl answer: \"yes\\nlinger=no\"")
        );
    }

    #[test]
    fn supervisor_words_can_never_forge_a_line_of_the_report() {
        // stderr carrying a newline would otherwise print a `linger=`
        // line the report never wrote.
        let hostile = exit(1, "failed)\nlinger=yes\nx");
        let LingerState::Unknown(reason) = parse_linger(&Ok(hostile)) else {
            panic!("a failed probe is unknown");
        };
        assert_eq!(reason, "failed) linger=yes x");
        assert!(!render_linger(&LingerState::Unknown(reason), 1000).contains('\n'));

        // Control bytes and the Unicode line separators go the same way.
        let LingerState::Unknown(reason) = parse_linger(&Ok(exit(1, "a\u{7}b\u{2028}c\td"))) else {
            panic!("a failed probe is unknown");
        };
        assert_eq!(reason, "a b c d");

        // And the same for the grant's own failure line, through the seam.
        let fake = FakeSupervisor::answering(
            Platform::Linux,
            vec![exit(1, "denied\nlinger=yes"), printed("no")],
        );
        let outcome = settle_linger(&fake, LingerStep::GrantThenReport);
        for line in &outcome.grant_failure {
            assert!(!line.contains('\n'), "{line:?}");
        }
        assert_eq!(
            outcome.grant_failure[0],
            "roostctl session autostart: loginctl enable-linger 501: denied linger=yes"
        );
    }

    #[test]
    fn the_linger_line_says_only_what_show_user_said() {
        assert_eq!(render_linger(&LingerState::Yes, 1000), "linger=yes");
        assert_eq!(
            render_linger(&LingerState::No, 1000),
            "linger=no (the unit starts at login, not at boot; \
             rerun with --linger or run: loginctl enable-linger 1000)"
        );
        assert_eq!(
            render_linger(&unknown("Failed to connect to bus: No medium found"), 1000),
            "linger=unknown (Failed to connect to bus: No medium found)"
        );
    }

    #[test]
    fn every_loginctl_invocation_carries_no_ask_password() {
        assert_eq!(enable_linger_command(501).to_string(), GRANT);
        assert_eq!(linger_query_command("loginctl", 501).to_string(), PROBE);
        assert_eq!(
            linger_query_command(LOGINCTL_PATH, 501).to_string(),
            format!("/usr/bin/{PROBE}")
        );
    }

    /// The two `loginctl` invocations as they are built, and as the fake
    /// records them at its own uid.
    const GRANT: &str = "loginctl enable-linger 501 --no-ask-password";
    const PROBE: &str = "loginctl show-user 501 --property=Linger --value --no-ask-password";

    fn linger_calls(
        outcome: Option<InstallOutcome>,
        flag: bool,
        answers: Vec<CommandResult>,
    ) -> (Vec<String>, LingerOutcome) {
        let fake = FakeSupervisor::answering(Platform::Linux, answers);
        let lingering = settle_linger(&fake, linger_step(Platform::Linux, outcome, flag));
        (fake.calls(), lingering)
    }

    #[test]
    fn a_linux_install_probes_show_user_and_grants_only_when_asked() {
        // A fresh install without the flag ends at the probe.
        let (calls, lingering) = linger_calls(
            Some(InstallOutcome::SupervisedFresh),
            false,
            vec![printed("no\n")],
        );
        assert_eq!(calls, [PROBE]);
        assert_eq!(
            lingering,
            LingerOutcome {
                grant_failure: Vec::new(),
                state: Some(LingerState::No),
            }
        );

        // The same install with the flag: grant first, then probe.
        let (calls, lingering) = linger_calls(
            Some(InstallOutcome::SupervisedFresh),
            true,
            vec![exit(0, ""), printed("yes\n")],
        );
        assert_eq!(calls, [GRANT, PROBE]);
        assert_eq!(lingering.state, Some(LingerState::Yes));
        assert!(lingering.grant_failure.is_empty());

        // Nothing to write and the flag: it still grants.
        let (calls, _) = linger_calls(
            Some(InstallOutcome::Unchanged),
            true,
            vec![exit(0, ""), printed("yes\n")],
        );
        assert_eq!(calls, [GRANT, PROBE]);

        // No session could be confirmed: probed, never granted.
        let (calls, lingering) = linger_calls(
            Some(InstallOutcome::ActivationUnconfirmed),
            true,
            vec![printed("no\n")],
        );
        assert_eq!(calls, [PROBE]);
        assert_eq!(lingering.state, Some(LingerState::No));

        // A refusal or a failed activation: neither command runs.
        for flag in [false, true] {
            let (calls, lingering) = linger_calls(None, flag, Vec::new());
            assert_eq!(calls, Vec::<String>::new(), "with the flag {flag}");
            assert_eq!(lingering, LingerOutcome::default());
        }
    }

    #[test]
    fn show_user_decides_the_line_whether_the_grant_worked_or_not() {
        // Refused by polkit: the two stderr lines, and the verb exits 1.
        let (calls, lingering) = linger_calls(
            Some(InstallOutcome::SupervisedFresh),
            true,
            vec![
                exit(
                    1,
                    "Failed to enable linger: Interactive authentication required.\n",
                ),
                printed("no\n"),
            ],
        );
        assert_eq!(calls, [GRANT, PROBE]);
        assert_eq!(
            lingering.grant_failure,
            [
                "roostctl session autostart: loginctl enable-linger 501: \
                 Failed to enable linger: Interactive authentication required.",
                "run it by hand: loginctl enable-linger 501",
            ]
        );
        assert_eq!(lingering.state, Some(LingerState::No));
        assert_eq!(
            install_exit_code(Some(InstallOutcome::SupervisedFresh), &lingering),
            1
        );

        // A probe that fails leaves the line unknown, and the install
        // still succeeded.
        let (calls, lingering) = linger_calls(
            Some(InstallOutcome::SupervisedFresh),
            true,
            vec![
                exit(0, ""),
                exit(1, "Failed to connect to bus: No medium found\n"),
            ],
        );
        assert_eq!(calls, [GRANT, PROBE]);
        assert!(lingering.grant_failure.is_empty());
        assert_eq!(
            lingering.state,
            Some(unknown("Failed to connect to bus: No medium found"))
        );
        assert_eq!(
            install_exit_code(Some(InstallOutcome::SupervisedFresh), &lingering),
            0
        );

        // A grant with nothing to say for itself is still named, and
        // still carries the manual command.
        let (calls, lingering) = linger_calls(
            Some(InstallOutcome::SupervisedFresh),
            true,
            vec![exit(1, "  "), printed("no\n")],
        );
        assert_eq!(calls, [GRANT, PROBE]);
        assert_eq!(
            lingering.grant_failure,
            [
                "roostctl session autostart: loginctl enable-linger 501: exit Some(1)",
                "run it by hand: loginctl enable-linger 501",
            ]
        );
    }

    #[test]
    fn the_sibling_note_comes_first_and_the_linger_line_last() {
        let note = "note: the dev-slot artifact also exists".to_string();
        let lingering = LingerOutcome {
            grant_failure: vec![
                "roostctl session autostart: loginctl enable-linger 501: nope".to_string(),
                "run it by hand: loginctl enable-linger 501".to_string(),
            ],
            state: Some(LingerState::No),
        };
        assert_eq!(
            install_tail(Some(note.clone()), &lingering, 501),
            [
                Tail::Note(note),
                Tail::Note(
                    "roostctl session autostart: loginctl enable-linger 501: nope".to_string()
                ),
                Tail::Note("run it by hand: loginctl enable-linger 501".to_string()),
                Tail::Report(render_linger(&LingerState::No, 501)),
            ]
        );

        // Nothing to say about either.
        assert_eq!(
            install_tail(None, &LingerOutcome::default(), 501),
            Vec::<Tail>::new()
        );
    }

    #[test]
    fn an_install_exits_1_only_for_a_failure_it_can_name() {
        let clean = LingerOutcome::default();
        assert_eq!(install_exit_code(None, &clean), 1);
        assert_eq!(
            install_exit_code(Some(InstallOutcome::ActivationUnconfirmed), &clean),
            1
        );
        for outcome in [
            InstallOutcome::Unchanged,
            InstallOutcome::ReinstalledLoaded,
            InstallOutcome::SupervisedFresh,
            InstallOutcome::AlreadyRunningUntouched,
        ] {
            assert_eq!(install_exit_code(Some(outcome), &clean), 0, "{outcome:?}");
        }
    }

    // ------------------------------------------------------------------
    // The doctor probe
    // ------------------------------------------------------------------

    /// Every state a slot can be in, named for the assertion messages.
    fn slot_states() -> Vec<(&'static str, ArtifactState)> {
        vec![
            ("absent", ArtifactState::Absent),
            (
                "unavailable",
                ArtifactState::Unavailable("not a regular file".to_string()),
            ),
            ("foreign", ArtifactState::Foreign),
            ("newer", ArtifactState::Newer { found: 9 }),
            (
                "ours",
                ArtifactState::Ours(Artifact {
                    platform: Platform::Linux,
                    binary: PathBuf::from("/usr/bin/roost-session"),
                    format: ARTIFACT_FORMAT,
                }),
            ),
        ]
    }

    fn command_lines(commands: &[Command]) -> Vec<String> {
        commands.iter().map(Command::to_string).collect()
    }

    const LINGER_PROBE: &str =
        "/usr/bin/loginctl show-user 1000 --property=Linger --value --no-ask-password";

    #[test]
    fn the_linux_probe_asks_the_supervisor_only_about_an_artifact_of_ours() {
        for (label, state) in slot_states() {
            let lines = command_lines(&probe_commands(Platform::Linux, &release(), 1000, &state));
            let want: Vec<String> = if state.is_ours() {
                vec![
                    "/usr/bin/systemctl --user is-enabled roost-session.service".to_string(),
                    "/usr/bin/systemctl --user is-active roost-session.service".to_string(),
                    LINGER_PROBE.to_string(),
                ]
            } else {
                vec![LINGER_PROBE.to_string()]
            };
            assert_eq!(lines, want, "{label}");
        }
    }

    #[test]
    fn the_macos_probe_asks_the_supervisor_only_about_an_artifact_of_ours() {
        for (label, state) in slot_states() {
            let lines = command_lines(&probe_commands(Platform::MacOs, &release(), 501, &state));
            let want: Vec<String> = if state.is_ours() {
                vec![
                    "/bin/launchctl print gui/501/ai.stridelabs.roost-session".to_string(),
                    "/bin/launchctl print-disabled gui/501".to_string(),
                ]
            } else {
                // launchd has no linger to ask about.
                Vec::new()
            };
            assert_eq!(lines, want, "{label}");
        }
    }

    #[test]
    fn the_probe_names_the_profiles_own_unit_and_label() {
        let ours = slot_states().pop().expect("the `ours` row").1;
        assert_eq!(
            command_lines(&probe_commands(Platform::Linux, &dev(), 1000, &ours)),
            vec![
                "/usr/bin/systemctl --user is-enabled roost-session-dev.service",
                "/usr/bin/systemctl --user is-active roost-session-dev.service",
                LINGER_PROBE,
            ]
        );
        assert_eq!(
            command_lines(&probe_commands(Platform::MacOs, &dev(), 501, &ours)),
            vec![
                "/bin/launchctl print gui/501/ai.stridelabs.roost-session-dev",
                "/bin/launchctl print-disabled gui/501",
            ]
        );
    }

    /// Doctor reports; it never repairs. Every verb that would change
    /// the supervisor's mind is named here, so a probe that grows one
    /// fails rather than ships.
    #[test]
    fn no_probe_command_can_mutate_anything() {
        const MUTATING: &[&str] = &[
            "enable",
            "disable",
            "bootstrap",
            "bootout",
            "kickstart",
            "enable-linger",
            "disable-linger",
            "daemon-reload",
            "unmask",
            "mask",
            "start",
            "stop",
            "restart",
        ];
        for platform in [Platform::Linux, Platform::MacOs] {
            for names in [release(), dev()] {
                for (label, state) in slot_states() {
                    for command in probe_commands(platform, &names, 1000, &state) {
                        assert!(
                            command.program.starts_with('/'),
                            "{label}: {command} is PATH-resolved"
                        );
                        for arg in &command.args {
                            assert!(
                                !MUTATING.contains(&arg.as_str()),
                                "{label}: {command} would mutate"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The probe as doctor hands it back: the file half untouched, the
    /// supervisor half whatever the outcomes said.
    fn linux_target() -> ProbeTarget {
        ProbeTarget {
            platform: Some(Platform::Linux),
            names: release(),
            uid: 1000,
            artifact_path: Some(PathBuf::from(
                "/home/x/.config/systemd/user/roost-session.service",
            )),
            artifact: ArtifactState::Ours(Artifact {
                platform: Platform::Linux,
                binary: PathBuf::from("/usr/bin/roost-session"),
                format: ARTIFACT_FORMAT,
            }),
            binary_executable: Some(true),
            sibling: None,
        }
    }

    fn macos_target() -> ProbeTarget {
        ProbeTarget {
            platform: Some(Platform::MacOs),
            names: release(),
            uid: 501,
            artifact_path: Some(PathBuf::from(
                "/Users/x/Library/LaunchAgents/ai.stridelabs.roost-session.plist",
            )),
            artifact: ArtifactState::Ours(Artifact {
                platform: Platform::MacOs,
                binary: PathBuf::from("/usr/local/bin/roost-session"),
                format: ARTIFACT_FORMAT,
            }),
            binary_executable: Some(true),
            sibling: None,
        }
    }

    /// One command's answer, paired with the command `probe_commands`
    /// would have produced for it.
    fn answered(
        target: &ProbeTarget,
        kind: ProbeKind,
        outcome: CommandOutcome,
    ) -> Vec<(Command, CommandOutcome)> {
        let platform = target.platform.expect("a platform");
        let command = probe_commands(platform, &target.names, target.uid, &target.artifact)
            .into_iter()
            .find(|c| probe_kind(c) == Some(kind))
            .unwrap_or_else(|| panic!("no {kind:?} command"));
        vec![(command, outcome)]
    }

    fn ran(status: i32, stdout: &str, stderr: &str) -> CommandOutcome {
        CommandOutcome::Ran(CommandResult {
            status: Some(status),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        })
    }

    fn enablement_of(target: &ProbeTarget, kind: ProbeKind, outcome: CommandOutcome) -> Enablement {
        interpret_probe(target.clone(), &answered(target, kind, outcome)).enablement
    }

    fn activity_of(target: &ProbeTarget, kind: ProbeKind, outcome: CommandOutcome) -> Activity {
        interpret_probe(target.clone(), &answered(target, kind, outcome)).activity
    }

    #[test]
    fn is_enabled_is_read_from_its_first_word() {
        let target = linux_target();
        let cases: Vec<(i32, &str, Enablement)> = vec![
            (0, "enabled\n", Enablement::Enabled),
            (0, "enabled-runtime\n", Enablement::EnabledUntilReboot),
            (
                1,
                "disabled\n",
                Enablement::Disabled("disabled".to_string()),
            ),
            (1, "masked\n", Enablement::Masked),
            (1, "masked-runtime\n", Enablement::Masked),
            (4, "not-found\n", Enablement::NotSeen),
            (
                0,
                "static\n",
                Enablement::NotProbed("unexpected is-enabled answer: \"static\"".to_string()),
            ),
        ];
        for (status, stdout, want) in cases {
            assert_eq!(
                enablement_of(&target, ProbeKind::Enabled, ran(status, stdout, "")),
                want,
                "{stdout:?}"
            );
        }
    }

    /// A user manager that is not there answers on stderr and prints no
    /// word at all, which is a reason rather than an answer.
    #[test]
    fn a_dead_bus_leaves_the_enablement_unprobed_with_the_reason() {
        let bus = "Failed to connect to bus: No medium found\n";
        assert_eq!(
            enablement_of(&linux_target(), ProbeKind::Enabled, ran(1, "", bus)),
            Enablement::NotProbed("Failed to connect to bus: No medium found".to_string())
        );
        assert_eq!(
            activity_of(&linux_target(), ProbeKind::Active, ran(1, "", bus)),
            Activity::NotProbed("Failed to connect to bus: No medium found".to_string())
        );
    }

    #[test]
    fn is_active_is_read_from_its_first_word() {
        let target = linux_target();
        let cases: Vec<(i32, &str, Activity)> = vec![
            (0, "active\n", Activity::Running { pid: None }),
            (4, "inactive\n", Activity::Idle("inactive".to_string())),
            (0, "activating\n", Activity::Idle("activating".to_string())),
            (
                0,
                "deactivating\n",
                Activity::Idle("deactivating".to_string()),
            ),
            (
                3,
                "failed\n",
                Activity::Failed("the unit is in the failed state".to_string()),
            ),
            (
                0,
                "reloading\n",
                Activity::NotProbed("unexpected is-active answer: \"reloading\"".to_string()),
            ),
        ];
        for (status, stdout, want) in cases {
            assert_eq!(
                activity_of(&target, ProbeKind::Active, ran(status, stdout, "")),
                want,
                "{stdout:?}"
            );
        }
    }

    #[test]
    fn launchctl_print_separates_running_from_failed_from_absent() {
        let target = macos_target();
        let running = "\tstate = running\n\tpid = 4242\n\tlast exit code = 0\n";
        assert_eq!(
            activity_of(&target, ProbeKind::Print, ran(0, running, "")),
            Activity::Running { pid: Some(4242) }
        );
        assert_eq!(
            activity_of(
                &target,
                ProbeKind::Print,
                ran(0, "\tstate = not running\n\tlast exit code = 78\n", "")
            ),
            Activity::Failed("last exit code 78".to_string())
        );
        assert_eq!(
            activity_of(
                &target,
                ProbeKind::Print,
                ran(0, "\tstate = not running\n\tlast exit code = 0\n", "")
            ),
            Activity::Idle("loaded, not running".to_string())
        );
        assert_eq!(
            activity_of(
                &target,
                ProbeKind::Print,
                ran(
                    113,
                    "",
                    "Could not find service \"ai.stridelabs.roost-session\"\n"
                )
            ),
            Activity::NotLoaded
        );
        assert_eq!(
            activity_of(&target, ProbeKind::Print, ran(1, "", "Bad request.\n")),
            Activity::NotProbed("Bad request.".to_string())
        );
    }

    /// A database holding both labels in opposite states is the case a
    /// substring match gets backwards.
    #[test]
    fn print_disabled_matches_the_whole_quoted_label() {
        let both = |release_state: &str, dev_state: &str| {
            format!(
                "disabled services = {{\n\
                 \t\"ai.stridelabs.roost-session\" => {release_state}\n\
                 \t\"ai.stridelabs.roost-session-dev\" => {dev_state}\n\
                 }}\n"
            )
        };
        let disabled = Enablement::Disabled("disabled in launchd's database".to_string());
        for (release_state, dev_state, want_release, want_dev) in [
            ("disabled", "enabled", disabled.clone(), Enablement::Enabled),
            ("enabled", "disabled", Enablement::Enabled, disabled.clone()),
            ("disabled", "disabled", disabled.clone(), disabled.clone()),
            (
                "enabled",
                "enabled",
                Enablement::Enabled,
                Enablement::Enabled,
            ),
        ] {
            let stdout = both(release_state, dev_state);
            for (names, want) in [(release(), &want_release), (dev(), &want_dev)] {
                let target = ProbeTarget {
                    names,
                    ..macos_target()
                };
                assert_eq!(
                    &enablement_of(&target, ProbeKind::PrintDisabled, ran(0, &stdout, "")),
                    want,
                    "{} in [{release_state}, {dev_state}]",
                    names.label()
                );
            }
        }

        // A label the database says nothing about is not disabled.
        assert_eq!(
            enablement_of(
                &macos_target(),
                ProbeKind::PrintDisabled,
                ran(
                    0,
                    "disabled services = {\n\t\"com.example.other\" => disabled\n}\n",
                    ""
                )
            ),
            Enablement::Enabled
        );
        assert_eq!(
            enablement_of(
                &macos_target(),
                ProbeKind::PrintDisabled,
                ran(1, "", "Could not find domain for\n")
            ),
            Enablement::NotProbed("Could not find domain for".to_string())
        );
    }

    /// A command that never ran leaves the fact it was for unprobed,
    /// naming what stopped it.
    #[test]
    fn a_command_that_could_not_run_carries_its_reason() {
        let target = linux_target();
        for (outcome, want) in [
            (
                CommandOutcome::Missing,
                "/usr/bin/systemctl is not on this system".to_string(),
            ),
            (
                CommandOutcome::TimedOut,
                "`/usr/bin/systemctl --user is-enabled roost-session.service` timed out"
                    .to_string(),
            ),
            (
                CommandOutcome::Failed("permission denied\nsecond line".to_string()),
                "permission denied second line".to_string(),
            ),
        ] {
            assert_eq!(
                enablement_of(&target, ProbeKind::Enabled, outcome.clone()),
                Enablement::NotProbed(want),
                "{outcome:?}"
            );
        }
    }

    /// The linger fact is the install path's own `parse_linger`, so a
    /// probe and an install can never word the same state differently.
    #[test]
    fn the_probe_reads_lingering_the_way_an_install_does() {
        let target = linux_target();
        let linger = |outcome: CommandOutcome| {
            interpret_probe(
                target.clone(),
                &answered(&target, ProbeKind::Linger, outcome),
            )
            .linger
        };
        assert_eq!(linger(ran(0, "yes\n", "")), LingerState::Yes);
        assert_eq!(linger(ran(0, "no\n", "")), LingerState::No);
        assert_eq!(
            linger(ran(1, "", "Failed to look up user 1000\n")),
            unknown("Failed to look up user 1000")
        );
        assert_eq!(
            linger(CommandOutcome::Missing),
            unknown("loginctl could not be run: /usr/bin/loginctl is not on this system")
        );
    }

    /// Nothing asked is not the same as nothing to say: every unprobed
    /// fact names why it was never asked, and the file half comes back
    /// exactly as it went in.
    #[test]
    fn an_unasked_probe_keeps_its_reason_and_its_file_facts() {
        let target = ProbeTarget {
            artifact: ArtifactState::Foreign,
            binary_executable: None,
            ..linux_target()
        };
        let probe = interpret_probe(target.clone(), &[]);
        assert_eq!(probe.target, target);
        let reason = "the artifact was not written by roostctl".to_string();
        assert_eq!(probe.enablement, Enablement::NotProbed(reason.clone()));
        assert_eq!(probe.activity, Activity::NotProbed(reason.clone()));
        assert_eq!(probe.linger, LingerState::Unknown(reason));

        for (state, want) in [
            (ArtifactState::Absent, "the artifact is not installed"),
            (
                ArtifactState::Unavailable("not a regular file".to_string()),
                "not a regular file",
            ),
        ] {
            let probe = interpret_probe(
                ProbeTarget {
                    artifact: state,
                    binary_executable: None,
                    ..linux_target()
                },
                &[],
            );
            assert_eq!(probe.enablement, Enablement::NotProbed(want.to_string()));
        }

        // No supervisor at all outranks whatever is on disk.
        let nowhere = interpret_probe(ProbeTarget::default(), &[]);
        assert_eq!(
            nowhere.linger,
            unknown("roost has no supervisor on this platform")
        );
    }

    /// A whole run, mapped back onto the right facts however the runner
    /// ordered the answers.
    #[test]
    fn a_full_run_lands_each_answer_on_its_own_fact() {
        let target = linux_target();
        let commands = probe_commands(Platform::Linux, &target.names, target.uid, &target.artifact);
        let answers = [
            ran(1, "disabled\n", ""),
            ran(3, "failed\n", ""),
            ran(0, "yes\n", ""),
        ];
        let mut outcomes: Vec<(Command, CommandOutcome)> =
            commands.into_iter().zip(answers).collect();
        outcomes.reverse();
        let probe = interpret_probe(target, &outcomes);
        assert_eq!(
            probe.enablement,
            Enablement::Disabled("disabled".to_string())
        );
        assert_eq!(
            probe.activity,
            Activity::Failed("the unit is in the failed state".to_string())
        );
        assert_eq!(probe.linger, LingerState::Yes);
    }

    /// Supervisor text reaches a report line, so a newline in it would
    /// open a line the report never wrote.
    #[test]
    fn hostile_supervisor_output_cannot_forge_a_probe_line() {
        let target = linux_target();
        let forged = "boom\nlinger=yes\n";
        let probe = interpret_probe(
            target.clone(),
            &answered(&target, ProbeKind::Linger, ran(1, "", forged)),
        );
        assert_eq!(probe.linger, unknown("boom linger=yes"));
        assert_eq!(
            enablement_of(&target, ProbeKind::Enabled, ran(1, "", forged)),
            Enablement::NotProbed("boom linger=yes".to_string())
        );
        assert_eq!(
            enablement_of(
                &target,
                ProbeKind::Enabled,
                ran(0, "\u{1b}[2Kenabled\n", "")
            ),
            Enablement::NotProbed(
                "unexpected is-enabled answer: \"\\u{1b}[2Kenabled\"".to_string()
            )
        );
    }

    #[test]
    fn the_artifact_state_folds_the_read_and_the_parse() {
        let text = render_unit(&release(), Path::new("/usr/bin/roost-session"));
        assert_eq!(
            artifact_state(&text_read(&text), &release()),
            ArtifactState::Ours(ours(&text, &release()))
        );
        // The same bytes under the other profile's names are somebody
        // else's file.
        assert_eq!(
            artifact_state(&text_read(&text), &dev()),
            ArtifactState::Foreign
        );
        assert_eq!(
            artifact_state(&ArtifactRead::Absent, &release()),
            ArtifactState::Absent
        );
        assert_eq!(
            artifact_state(
                &ArtifactRead::Unavailable("not a regular file\nfoo".to_string()),
                &release()
            ),
            ArtifactState::Unavailable("not a regular file foo".to_string())
        );
        let newer = text.replace(
            &unit_marker(ARTIFACT_FORMAT),
            &unit_marker(ARTIFACT_FORMAT + 1),
        );
        assert_eq!(
            artifact_state(&text_read(&newer), &release()),
            ArtifactState::Newer {
                found: ARTIFACT_FORMAT + 1
            }
        );
    }
}
