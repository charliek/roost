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
//! The name is fixed across bundle profiles on purpose — one unit, one
//! label — so a *debug* `roostctl` installs over a release install's
//! artifact. That is why the resolved binary is printed on every
//! install: it is the user's only signal that the slot just changed
//! hands.
//!
//! # Why the binary path is absolutized but never canonicalized
//!
//! A packaged `/usr/bin/roost-session` is usually a symlink into a
//! versioned target that the next upgrade replaces. Writing the
//! resolved target would pin the unit to a build that is about to
//! vanish, so the path is only made absolute — lexically, `.`/`..`
//! components folded — and written as itself.
//!
//! The split here is `session.rs`'s: the decisions ([`render_unit`],
//! [`render_plist`], [`check_unit_path`], [`absolutize`],
//! [`parse_artifact`], [`installed_state`], [`classify_install`],
//! [`already_gone`]) are pure and table-tested, and the I/O around them
//! is thin. Both renderers compile everywhere so those tables run on
//! every platform; only the install/uninstall/status I/O picks by
//! `cfg(target_os)`.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;

use roost_ipc::paths::BundleProfile;
use roost_ipc::session_launch::{
    self, confirm_serving, locate_session_binary, BIN_ENV, IPC_TIMEOUT,
};

use crate::session::scaled;

/// The bounded `session.identify` poll after the supervisor has been
/// asked to start one — the same budget `session start` climbs, because
/// it is the same wait.
const CONFIRM_TIMEOUT: Duration = session_launch::DEFAULT_CONFIRM_BUDGET;

/// The systemd unit's file name. Profile-independent: see the module docs.
pub const UNIT_NAME: &str = "roost-session.service";

/// The launchd job label, and the plist's file stem.
pub const LAUNCHD_LABEL: &str = "ai.stridelabs.roost-session";

/// The `Description=` line that says a unit is ours.
const UNIT_DESCRIPTION: &str = "Description=Roost host session (roost-session)";

/// The marker that makes an artifact ours, in each supervisor's comment
/// syntax — the same sentence both times. Ownership hangs on this line
/// alone: a description and a `--foreground` tail are things another
/// program can carry by coincidence, and adopting such a file would
/// overwrite it without `--force`.
const UNIT_MARKER: &str =
    "# Written by roostctl session autostart. Reinstalling replaces this file.";
const PLIST_MARKER: &str =
    "<!-- Written by roostctl session autostart. Reinstalling replaces this file. -->";

/// The tail every artifact of ours ends in — `--foreground` is what
/// keeps the daemon attached to its supervisor instead of forking away.
const EXEC_TAIL: &str = "start --foreground";

const XDG_CONFIG_HOME_ENV: &str = "XDG_CONFIG_HOME";

#[derive(Subcommand, Debug)]
pub enum AutostartCmd {
    /// Write the supervisor artifact for this platform, load it, and
    /// confirm a session answers. Prints the artifact path and the
    /// `roost-session` binary it names.
    Install {
        /// Replace a file at the artifact path that roostctl did not
        /// write. Its previous contents are echoed to stderr first.
        #[arg(long)]
        force: bool,
    },
    /// Remove the artifact and unload it. This **stops the supervised
    /// session** — a session the supervisor is not running is untouched.
    Uninstall,
}

/// Run an `autostart` verb. Returns the process exit code; the caller
/// keeps the one exit point.
pub async fn run(cmd: &AutostartCmd) -> Result<i32> {
    match cmd {
        AutostartCmd::Install { force } => install(*force).await,
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

/// How the artifact is named in user-facing output.
pub fn supervisor_label(platform: Platform) -> String {
    match platform {
        Platform::Linux => format!("systemd --user {UNIT_NAME}"),
        Platform::MacOs => format!("launchd {LAUNCHD_LABEL}"),
    }
}

/// Where the artifact lives. `xdg_config_home` is honoured only when it
/// is absolute — the XDG spec's own rule, and what keeps a stray
/// relative value from writing a unit against the process cwd.
pub fn artifact_path(platform: Platform, home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    match platform {
        Platform::Linux => xdg_config_home
            .filter(|p| p.is_absolute())
            .map_or_else(|| home.join(".config"), Path::to_path_buf)
            .join("systemd")
            .join("user")
            .join(UNIT_NAME),
        Platform::MacOs => home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")),
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
pub fn render_unit(bin: &Path) -> String {
    format!(
        "[Unit]\n\
         {UNIT_MARKER}\n\
         {UNIT_DESCRIPTION}\n\
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
        bin = bin.to_string_lossy(),
    )
}

/// The recipe `docs/guides/host-sessions.md` has always documented, plus
/// `WorkingDirectory` for the same reason the unit carries `%h`.
///
/// Both paths must have cleared [`check_plist_path`] first, which is why
/// the lossy conversions below cannot lose anything.
pub fn render_plist(bin: &Path, home: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
{PLIST_MARKER}
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
        label = LAUNCHD_LABEL,
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
}

/// Is this file ours, and which binary does it name?
///
/// Ours means our marker line **and** the label/`Description` **and** an
/// exec line ending in `start --foreground` — all three. A hand-edited
/// copy (extra `Environment=` lines, a reordered plist) still reads as
/// ours and is overwritten on re-install; anything else, including a
/// file hand-written from the docs, is foreign and needs `--force`.
pub fn parse_artifact(text: &str) -> Option<Artifact> {
    parse_unit(text).or_else(|| parse_plist(text))
}

fn has_line(text: &str, marker: &str) -> bool {
    text.lines().any(|line| line.trim() == marker)
}

fn parse_unit(text: &str) -> Option<Artifact> {
    if !has_line(text, UNIT_MARKER) || !has_line(text, UNIT_DESCRIPTION) {
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
    })
}

fn parse_plist(text: &str) -> Option<Artifact> {
    if !has_line(text, PLIST_MARKER) || !text.contains(&format!("<string>{LAUNCHD_LABEL}</string>"))
    {
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
    Installed {
        binary: PathBuf,
        binary_missing: bool,
    },
}

/// Installed-ness is the file existing at the expected path **and being
/// ours**. The binary it names is reported, never compared with the one
/// this `roostctl` would resolve: that comparison is roostctl-relative,
/// so a dev build beside a release install would cry wolf.
pub fn installed_state(
    path_exists: bool,
    parsed: Option<Artifact>,
    binary_executable: bool,
) -> AutostartState {
    match (path_exists, parsed) {
        (false, _) => AutostartState::NotInstalled,
        (true, None) => AutostartState::Foreign,
        (true, Some(artifact)) => AutostartState::Installed {
            binary: artifact.binary,
            binary_missing: !binary_executable,
        },
    }
}

/// The `autostart=…` line, in the plain one-line style the rest of
/// `session status` prints.
pub fn render_status_line(platform: Platform, artifact: &Path, state: &AutostartState) -> String {
    match state {
        AutostartState::NotInstalled => "autostart=not installed".to_string(),
        AutostartState::Foreign => format!(
            "autostart=not installed (foreign file: not written by roostctl: {})",
            artifact.display()
        ),
        AutostartState::Installed {
            binary,
            binary_missing,
        } => {
            let mut line = format!(
                "autostart=installed ({} → {})",
                supervisor_label(platform),
                binary.display()
            );
            if *binary_missing {
                line.push_str(&format!(" (binary missing: {})", binary.display()));
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
    let home = match home_dir() {
        Ok(home) => home,
        Err(error) => return format!("{UNAVAILABLE_LINE} ({error})"),
    };
    let artifact = artifact_path(platform, &home, xdg_config_home().as_deref());
    let text = std::fs::read_to_string(&artifact).ok();
    let parsed = text.as_deref().and_then(parse_artifact);
    let executable = parsed
        .as_ref()
        .is_some_and(|a| is_executable_file(&a.binary));
    let state = installed_state(text.is_some(), parsed, executable);
    render_status_line(platform, &artifact, &state)
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
        Ok(self.run(&active_command(self.platform(), self.uid()))?.ok())
    }

    /// Is a supervised process alive right now?
    ///
    /// This — not the socket — is what proves whose daemon is answering:
    /// a supervised start that finds a session already there prints
    /// `already-running` and exits 0, leaving the job idle while the
    /// socket answers perfectly well.
    fn is_running(&self) -> Result<bool> {
        let out = self.run(&active_command(self.platform(), self.uid()))?;
        Ok(match self.platform() {
            Platform::Linux => out.ok(),
            // `launchctl print` exits 0 for a *loaded* label whether or
            // not a process exists; only a `pid` line says one does.
            Platform::MacOs => out.ok() && out.stdout.contains("pid = "),
        })
    }
}

fn service_target(uid: u32) -> String {
    format!("gui/{uid}/{LAUNCHD_LABEL}")
}

fn active_command(platform: Platform, uid: u32) -> Command {
    match platform {
        Platform::Linux => Command::new("systemctl", &["--user", "is-active", UNIT_NAME]),
        Platform::MacOs => Command::new("launchctl", &["print", &service_target(uid)]),
    }
}

/// What a user runs by hand when this command deliberately did not.
pub fn restart_command(platform: Platform, artifact: &Path, uid: u32) -> String {
    match platform {
        Platform::Linux => "systemctl --user restart roost-session".to_string(),
        Platform::MacOs => format!(
            "launchctl bootout {} && launchctl bootstrap gui/{uid} {}",
            service_target(uid),
            artifact.display()
        ),
    }
}

/// What starts the *next* session under the supervisor when this one
/// was already running unsupervised.
pub fn start_command(platform: Platform, uid: u32) -> String {
    match platform {
        Platform::Linux => "systemctl --user start roost-session".to_string(),
        Platform::MacOs => format!("launchctl kickstart {}", service_target(uid)),
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

/// Load a freshly written artifact.
pub fn activate(supervisor: &dyn Supervisor, artifact: &Path) -> Result<(), ActivationFailure> {
    let commands = match supervisor.platform() {
        Platform::Linux => vec![
            Command::new("systemctl", &["--user", "daemon-reload"]),
            Command::new("systemctl", &["--user", "enable", "--now", UNIT_NAME]),
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
    let commands = match platform {
        Platform::Linux => vec![Command::new(
            "systemctl",
            &["--user", "disable", "--now", UNIT_NAME],
        )],
        Platform::MacOs => vec![Command::new(
            "launchctl",
            &["bootout", &service_target(supervisor.uid())],
        )],
    };
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
    /// Ours, byte-identical to what this install would write.
    Identical,
    /// Ours, different bytes — a hand edit, or a different binary.
    Changed,
    /// Not ours. Only reaches the state machine under `--force`.
    Foreign,
}

/// Compare what is on disk with what this install would write.
pub fn classify_existing(existing: Option<&str>, rendered: &str) -> Existing {
    match existing {
        None => Existing::Absent,
        Some(text) if parse_artifact(text).is_none() => Existing::Foreign,
        Some(text) if text == rendered => Existing::Identical,
        Some(_) => Existing::Changed,
    }
}

/// What an install does with the bytes on disk, before the supervisor
/// hears anything at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStep {
    /// A file roostctl did not write. Refused by name rather than
    /// clobbered — the artifact name is fixed across profiles, so this
    /// is the only thing standing between an install and somebody
    /// else's unit.
    RefuseForeign,
    /// The bytes already match: leave the file exactly as it is.
    Keep,
    /// Write, echoing any previous contents to stderr first so a hand
    /// edit is never silently lost.
    Write,
}

pub fn write_step(existing: Existing, force: bool) -> WriteStep {
    match existing {
        Existing::Foreign if !force => WriteStep::RefuseForeign,
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

// ============================================================================
// I/O
// ============================================================================

struct HostSupervisor {
    platform: Platform,
    uid: u32,
}

impl HostSupervisor {
    fn new(platform: Platform) -> Self {
        Self {
            platform,
            // SAFETY: a plain getter with no arguments.
            uid: unsafe { libc::getuid() },
        }
    }
}

impl Supervisor for HostSupervisor {
    fn platform(&self) -> Platform {
        self.platform
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

fn home_dir() -> Result<PathBuf> {
    let raw = std::env::var_os("HOME").context("$HOME is not set")?;
    let path = PathBuf::from(raw);
    anyhow::ensure!(
        path.is_absolute(),
        "$HOME is not an absolute path (got {})",
        path.display()
    );
    Ok(path)
}

fn xdg_config_home() -> Option<PathBuf> {
    std::env::var_os(XDG_CONFIG_HOME_ENV)
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
}

/// The three facts both verbs open with: this build's supervisor, the
/// home directory, and where the artifact goes. `None` — already
/// reported — where roost has no supervisor to write for.
fn artifact_target() -> Result<Option<(Platform, PathBuf, PathBuf)>> {
    let Some(platform) = host_platform() else {
        eprintln!("roostctl session autostart: autostart is not supported on this platform");
        return Ok(None);
    };
    let home = home_dir()?;
    let artifact = artifact_path(platform, &home, xdg_config_home().as_deref());
    Ok(Some((platform, home, artifact)))
}

/// The artifact text this platform installs, or the refusal that stops
/// the install before anything is written.
fn render_artifact(platform: Platform, bin: &Path, home: &Path) -> Result<String, Refusal> {
    match platform {
        Platform::Linux => {
            check_unit_path(bin)?;
            Ok(render_unit(bin))
        }
        Platform::MacOs => {
            check_plist_path(bin)?;
            check_plist_path(home)?;
            Ok(render_plist(bin, home))
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

fn read_artifact(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Which session, if any, is answering right now.
async fn current_session_id(socket: &Path) -> Option<String> {
    session_launch::identify(socket, scaled(IPC_TIMEOUT))
        .await
        .ok()
        .map(|identity| identity.session_id)
}

async fn install(force: bool) -> Result<i32> {
    let Some((platform, home, artifact)) = artifact_target()? else {
        return Ok(1);
    };

    let located = locate_session_binary(
        std::env::var_os(BIN_ENV).as_deref(),
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )?;
    let cwd = std::env::current_dir().context("read the working directory")?;
    let bin = absolutize(&located.path, &cwd);
    let text = render_artifact(platform, &bin, &home)?;

    println!("autostart: {} → {}", artifact.display(), bin.display());

    let previous = read_artifact(&artifact)?;
    let existing = classify_existing(previous.as_deref(), &text);
    match write_step(existing, force) {
        WriteStep::RefuseForeign => {
            eprintln!(
                "roostctl session autostart: {} was not written by roostctl; \
                 pass --force to replace it",
                artifact.display()
            );
            return Ok(1);
        }
        WriteStep::Keep => {}
        WriteStep::Write => {
            if let Some(old) = &previous {
                eprintln!(
                    "roostctl session autostart: replacing {}; previous contents follow\n{old}",
                    artifact.display()
                );
            }
            write_artifact(&artifact, &text)?;
        }
    }

    let supervisor = HostSupervisor::new(platform);
    let uid = supervisor.uid();
    let loaded_before = supervisor.is_loaded()?;

    if let Some(settled) = settled_without_supervisor(existing, loaded_before) {
        report_install(settled, platform, &artifact, uid, None);
        return Ok(0);
    }

    let socket = BundleProfile::session()
        .context("resolve the session socket path")?
        .socket_path;
    let before = current_session_id(&socket).await;

    if let Err(failure) = activate(&supervisor, &artifact) {
        // The file is kept: a retry is `install` again, and `status`
        // reporting it as installed is the truth about what roost owns.
        eprintln!("roostctl session autostart: installed; activation failed: {failure}");
        return Ok(1);
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
    report_install(outcome, platform, &artifact, uid, after.as_deref());
    Ok(match outcome {
        InstallOutcome::ActivationUnconfirmed => 1,
        _ => 0,
    })
}

fn report_install(
    outcome: InstallOutcome,
    platform: Platform,
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
                restart_command(platform, artifact, uid)
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
                start_command(platform, uid)
            );
        }
        InstallOutcome::ActivationUnconfirmed => {
            eprintln!(
                "roostctl session autostart: installed, but no supervised session could be \
                 confirmed — ask {} what happened",
                supervisor_label(platform)
            );
        }
    }
}

async fn uninstall() -> Result<i32> {
    let Some((platform, _home, artifact)) = artifact_target()? else {
        return Ok(1);
    };

    let Some(previous) = read_artifact(&artifact)? else {
        println!(
            "autostart: nothing to uninstall (no {})",
            artifact.display()
        );
        return Ok(0);
    };
    if parse_artifact(&previous).is_none() {
        eprintln!(
            "roostctl session autostart: {} was not written by roostctl; leaving it alone",
            artifact.display()
        );
        return Ok(1);
    }

    let supervisor = HostSupervisor::new(platform);
    // `is_running`, not `is_loaded`: the sentence is about shells that
    // are about to be hung up, and a loaded-but-idle agent has none.
    if supervisor.is_running()? {
        println!(
            "stopping the supervised session ({})",
            supervisor_label(platform)
        );
    } else {
        println!("the supervisor is not running a session; nothing to stop");
    }

    if let Err(failure) = deactivate(&supervisor) {
        eprintln!("roostctl session autostart: {failure}");
        return Ok(1);
    }
    std::fs::remove_file(&artifact).with_context(|| format!("remove {}", artifact.display()))?;
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
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    /// Records every command and answers from a queue; an exhausted
    /// queue answers exit 0 with no output.
    struct FakeSupervisor {
        platform: Platform,
        uid: u32,
        calls: RefCell<Vec<String>>,
        answers: RefCell<VecDeque<CommandResult>>,
    }

    impl FakeSupervisor {
        fn new(platform: Platform) -> Self {
            Self {
                platform,
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
    // Rendering
    // ------------------------------------------------------------------

    #[test]
    fn the_unit_renders_the_pinned_text_with_a_quoted_path() {
        // A space is legal precisely because the path is quoted.
        let rendered = render_unit(Path::new("/opt/my apps/roost-session"));
        assert_eq!(
            rendered,
            "[Unit]\n\
             # Written by roostctl session autostart. Reinstalling replaces this file.\n\
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
            Path::new("/Apps/R&D <beta>/roost-session"),
            Path::new("/Users/a&b"),
        );
        assert_eq!(
            rendered,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<!-- Written by roostctl session autostart. Reinstalling replaces this file. -->
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
            parse_artifact(&rendered).unwrap().binary,
            PathBuf::from("/Apps/R&D <beta>/roost-session")
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
        assert!(render_artifact(Platform::Linux, Path::new("/opt/$HOME/roost"), home).is_err());
        assert!(render_artifact(Platform::MacOs, Path::new("/opt/$HOME/roost"), home).is_ok());
        assert!(render_artifact(Platform::MacOs, &non_utf8, home).is_err());
    }

    #[test]
    fn a_plist_path_is_refused_for_a_control_character_or_non_utf8() {
        let home = Path::new("/Users/u");
        let bin = Path::new("/Apps/roost-session");

        // XML 1.0 cannot write U+0007 at all, so this renders a plist
        // launchd rejects.
        let bell = PathBuf::from("/Apps/roost\u{7}session");
        let error = render_artifact(Platform::MacOs, &bell, home)
            .expect_err("a control character must be refused, not rendered");
        assert!(error.to_string().contains("'\\u{7}'"), "{error}");
        assert!(error.to_string().contains("/Apps/roost"), "{error}");

        // The working directory is rendered too, and is named by itself.
        let bad_home = PathBuf::from("/Users/u\u{1}");
        let error = render_artifact(Platform::MacOs, bin, &bad_home)
            .expect_err("the home path is rendered into the plist as well");
        assert!(error.to_string().contains("'\\u{1}'"), "{error}");
        assert!(error.to_string().contains("/Users/u"), "{error}");

        // A non-UTF-8 home would otherwise go through `to_string_lossy`
        // and name a *different* directory than the one it came from.
        let lossy_home = PathBuf::from(OsStr::from_bytes(b"/Users/\xffu"));
        let error = render_artifact(Platform::MacOs, bin, &lossy_home)
            .expect_err("a non-UTF-8 home must be refused rather than mangled");
        assert!(error.to_string().contains("UTF-8"), "{error}");

        // The escaping still carries what XML *can* represent.
        assert!(check_plist_path(Path::new("/Apps/R&D <beta>/roost-session")).is_ok());
        assert!(render_artifact(Platform::MacOs, bin, Path::new("/Users/a&b")).is_ok());
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
        let unit = render_unit(Path::new("/usr/bin/roost-session"));
        assert_eq!(
            parse_artifact(&unit),
            Some(Artifact {
                platform: Platform::Linux,
                binary: PathBuf::from("/usr/bin/roost-session"),
            })
        );

        // A hand edit (an extra Environment= line) is still ours.
        let edited = unit.replace(
            "Restart=on-failure",
            "Environment=RUST_LOG=debug\nRestart=always",
        );
        assert_eq!(
            parse_artifact(&edited).unwrap().binary,
            PathBuf::from("/usr/bin/roost-session")
        );

        let plist = render_plist(Path::new("/Apps/roost-session"), Path::new("/Users/a"));
        assert_eq!(
            parse_artifact(&plist),
            Some(Artifact {
                platform: Platform::MacOs,
                binary: PathBuf::from("/Apps/roost-session"),
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
            assert_eq!(parse_artifact(foreign), None, "{foreign}");
        }
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
        let unit = render_unit(Path::new("/usr/bin/roost-session"));
        let plist = render_plist(Path::new("/Apps/roost-session"), Path::new("/Users/a"));

        // What this command writes reads back as ours, and the marker
        // sits where the docs say it does.
        assert!(parse_artifact(&unit).is_some(), "{unit}");
        assert!(parse_artifact(&plist).is_some(), "{plist}");
        assert!(unit.contains(&format!("[Unit]\n{UNIT_MARKER}\n")), "{unit}");
        assert!(
            plist.contains(&format!("<plist version=\"1.0\">\n{PLIST_MARKER}\n")),
            "{plist}"
        );

        // The same text with the marker deleted is somebody else's file.
        for stripped in [
            without_line(&unit, UNIT_MARKER),
            without_line(&plist, PLIST_MARKER),
        ] {
            assert_eq!(parse_artifact(&stripped), None, "{stripped}");
        }

        // The case the marker exists for: a foreign unit carrying our
        // Description and a `start --foreground` tail, which the shape
        // check alone would adopt and overwrite without `--force`.
        let impostor = "[Unit]\n\
                        Description=Roost host session (roost-session)\n\
                        \n\
                        [Service]\n\
                        ExecStart=\"/opt/other-daemon\" start --foreground\n";
        assert_eq!(parse_artifact(impostor), None);
        assert_eq!(classify_existing(Some(impostor), &unit), Existing::Foreign);
        assert_eq!(
            write_step(classify_existing(Some(impostor), &unit), false),
            WriteStep::RefuseForeign
        );

        // Its launchd twin: our label, our arguments, no marker.
        let impostor_plist =
            without_line(&plist, PLIST_MARKER).replace("/Apps/roost-session", "/opt/other-daemon");
        assert_eq!(parse_artifact(&impostor_plist), None);
    }

    #[test]
    fn installed_state_covers_every_state() {
        assert_eq!(
            installed_state(false, None, false),
            AutostartState::NotInstalled
        );
        assert_eq!(installed_state(true, None, false), AutostartState::Foreign);
        let ours = Artifact {
            platform: Platform::Linux,
            binary: PathBuf::from("/usr/bin/roost-session"),
        };
        assert_eq!(
            installed_state(true, Some(ours.clone()), true),
            AutostartState::Installed {
                binary: PathBuf::from("/usr/bin/roost-session"),
                binary_missing: false,
            }
        );
        assert_eq!(
            installed_state(true, Some(ours), false),
            AutostartState::Installed {
                binary: PathBuf::from("/usr/bin/roost-session"),
                binary_missing: true,
            }
        );
    }

    #[test]
    fn the_status_line_names_the_supervisor_the_binary_and_both_qualifiers() {
        let artifact = Path::new("/home/u/.config/systemd/user/roost-session.service");
        assert_eq!(
            render_status_line(Platform::Linux, artifact, &AutostartState::NotInstalled),
            "autostart=not installed"
        );
        assert_eq!(
            render_status_line(
                Platform::Linux,
                artifact,
                &AutostartState::Installed {
                    binary: PathBuf::from("/usr/bin/roost-session"),
                    binary_missing: false,
                }
            ),
            "autostart=installed (systemd --user roost-session.service → /usr/bin/roost-session)"
        );
        assert_eq!(
            render_status_line(
                Platform::MacOs,
                Path::new("/Users/u/Library/LaunchAgents/ai.stridelabs.roost-session.plist"),
                &AutostartState::Installed {
                    binary: PathBuf::from(
                        "/Applications/Roost-Iced.app/Contents/MacOS/roost-session"
                    ),
                    binary_missing: true,
                }
            ),
            "autostart=installed (launchd ai.stridelabs.roost-session → \
             /Applications/Roost-Iced.app/Contents/MacOS/roost-session) \
             (binary missing: /Applications/Roost-Iced.app/Contents/MacOS/roost-session)"
        );
        let foreign = render_status_line(Platform::Linux, artifact, &AutostartState::Foreign);
        assert!(
            foreign.contains("foreign file: not written by roostctl"),
            "{foreign}"
        );
        assert!(
            foreign.contains(&artifact.display().to_string()),
            "{foreign}"
        );
    }

    #[test]
    fn the_artifact_path_follows_xdg_when_it_is_absolute_and_home_otherwise() {
        let home = Path::new("/home/u");
        assert_eq!(
            artifact_path(Platform::Linux, home, None),
            PathBuf::from("/home/u/.config/systemd/user/roost-session.service")
        );
        assert_eq!(
            artifact_path(Platform::Linux, home, Some(Path::new("/xdg"))),
            PathBuf::from("/xdg/systemd/user/roost-session.service")
        );
        // A relative XDG value is invalid per the spec, not a base dir.
        assert_eq!(
            artifact_path(Platform::Linux, home, Some(Path::new("conf"))),
            PathBuf::from("/home/u/.config/systemd/user/roost-session.service")
        );
        assert_eq!(
            artifact_path(
                Platform::MacOs,
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
    fn classify_existing_compares_bytes_only_for_a_file_of_ours() {
        let rendered = render_unit(Path::new("/usr/bin/roost-session"));
        assert_eq!(classify_existing(None, &rendered), Existing::Absent);
        assert_eq!(
            classify_existing(Some(&rendered), &rendered),
            Existing::Identical
        );
        assert_eq!(
            classify_existing(
                Some(&render_unit(Path::new("/opt/roost-session"))),
                &rendered
            ),
            Existing::Changed
        );
        assert_eq!(
            classify_existing(Some("[Unit]\nDescription=Someone else\n"), &rendered),
            Existing::Foreign
        );
    }

    #[test]
    fn a_foreign_file_is_refused_until_force_and_identical_bytes_are_left_alone() {
        assert_eq!(
            write_step(Existing::Foreign, false),
            WriteStep::RefuseForeign
        );
        assert_eq!(write_step(Existing::Foreign, true), WriteStep::Write);
        // `--force` has nothing to do with our own files.
        for force in [false, true] {
            assert_eq!(write_step(Existing::Identical, force), WriteStep::Keep);
            assert_eq!(write_step(Existing::Changed, force), WriteStep::Write);
            assert_eq!(write_step(Existing::Absent, force), WriteStep::Write);
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
        // A loaded supervisor is left alone, whatever was on disk.
        for existing in [Existing::Absent, Existing::Changed, Existing::Foreign] {
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
    fn a_supervisor_failure_after_the_write_keeps_the_file() {
        let root = scratch("activation-failure");
        let artifact = root.join("systemd/user/roost-session.service");
        let text = render_unit(Path::new("/usr/bin/roost-session"));
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
            installed_state(true, parse_artifact(&kept), false),
            AutostartState::Installed {
                binary: PathBuf::from("/usr/bin/roost-session"),
                binary_missing: true,
            }
        );
    }

    #[test]
    fn the_artifact_is_written_atomically_at_0644_under_a_0755_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("write-mode");
        let artifact = root.join("systemd/user/roost-session.service");
        let text = render_unit(Path::new("/usr/bin/roost-session"));
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
            .filter(|name| name != OsStr::new(UNIT_NAME))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn an_existing_artifact_directory_keeps_the_mode_it_had() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("dir-mode");
        let dir = root.join("systemd/user");
        std::fs::create_dir_all(&dir).unwrap();
        // `~/.config/systemd/user` is often deliberately 0700; installing
        // a unit into it must not widen it.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let text = render_unit(Path::new("/usr/bin/roost-session"));
        write_artifact(&dir.join(UNIT_NAME), &text).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        assert_eq!(std::fs::read_to_string(dir.join(UNIT_NAME)).unwrap(), text);
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

        let dir = scratch("write-failure");
        let artifact = dir.join(UNIT_NAME);
        let text = render_unit(Path::new("/usr/bin/roost-session"));

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
            let tmp = dir.join(format!(".{UNIT_NAME}.{}.tmp", std::process::id()));
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

        let host = HostSupervisor::new(Platform::Linux);
        let result = host
            .run(&Command::new(&script.to_string_lossy(), &[]))
            .unwrap();

        assert!(result.ok(), "{result:?}");
        assert_eq!(result.stdout, "LC_ALL=C LANG=C\n");
    }
}
