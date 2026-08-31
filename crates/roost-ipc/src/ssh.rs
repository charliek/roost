//! The SSH transport (host-sessions HS-3, plan 038): a local Unix
//! socket that reaches a remote session over `ssh`.
//!
//! The module is in two halves, and the split is deliberate.
//!
//! The first half has no side effects at all — deciding what kind of
//! target a string names, building the generated `ssh_config` bytes,
//! building the argv for each of the four `ssh` invocations, sizing the
//! control-socket directory so `sun_path` fits, and turning a failed
//! connection's exit code + stderr into copy a user can act on. It is
//! therefore unit-tested exhaustively, right here.
//!
//! The second half is [`SshTunnel`], the runtime built out of those
//! pieces: one per saved host, owning a scratch directory, a shared
//! `ssh` mux, and a local `bridge.sock` whose every accepted connection
//! gets an `ssh` exec of its own. Its tests live in
//! `tests/ssh_transport_test.rs`, driven by a fake `ssh` — the only way
//! to pin process choreography without a real host on the other end.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::framing::{write_frame, FrameReader};
use crate::messages::{ops, RawRequest, Response, SessionIdentify, SESSION_PROTOCOL_VERSION};
use crate::paths::BundleProfile;
use crate::session_launch::{reap_by, timeout_scale};
use crate::socket_state::{self, SocketState, PROBE_TIMEOUT};

// ============================================================================
// Classifying a target
// ============================================================================

/// The `target` spelling that means "this machine's own session" — rule
/// 2 of [`classify`]'s table. Re-exported as
/// `crate::session_launch::LOCALHOST_TARGET` for every caller that
/// imported it from there before this module existed.
pub const LOCALHOST_TARGET: &str = "localhost";

/// What a saved host's `target` string turned out to mean.
///
/// [`classify`] applies these rules, in order, to the *trimmed* input:
///
/// 1. Empty after trimming — `Err`. There is no such target.
/// 2. Exactly `"localhost"` (the [`LOCALHOST_TARGET`] sentinel,
///    case-sensitive, whole-string match) — [`Self::LocalSession`],
///    this machine's own session profile socket.
/// 3. A leading `-` — `Err`. A target is about to become the last
///    positional argument of an `ssh` invocation; one that looks like
///    a flag (`-oProxyCommand=...`) must never be handed to `exec` as
///    one.
/// 4. An `ssh://` prefix, matched ASCII case-insensitively —
///    [`Self::Ssh`], the trimmed string passed through verbatim with
///    its original casing intact (only the *scheme* match is
///    case-blind).
/// 5. No `/` anywhere, but a `:` somewhere — `Err` naming the
///    `ssh://host:port` spelling. This is what catches `host:22` and a
///    bare IPv6 literal like `::1`, both of which are ambiguous with a
///    Unix socket path if read any other way.
/// 6. No `/` anywhere — [`Self::Ssh`]. Anything left with no path
///    separator reads as a host, not a file: `user@host`, a bare
///    hostname like `workbox`, and — a deliberate behavior change from
///    the pre-HS-3 reading — a bare filename like `foo.sock`, which
///    used to be treated as a same-directory socket path and now is
///    not. A caller that means the file in the current directory must
///    spell it `./foo.sock`.
/// 7. Otherwise — [`Self::UnixSocket`], the trimmed string verbatim as
///    a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTransport {
    /// The `"localhost"` sentinel: this machine's own session-profile
    /// socket.
    LocalSession(PathBuf),
    /// A filesystem path, taken verbatim.
    UnixSocket(PathBuf),
    /// An SSH-reached host.
    Ssh(SshTarget),
}

impl ResolvedTransport {
    /// Whether this is the `"localhost"` sentinel specifically.
    ///
    /// Only [`Self::LocalSession`] counts — `ssh://localhost` and
    /// `user@localhost` name a *remote* ssh target that merely resolves
    /// the hostname to a loopback address, which is a different thing
    /// than "this machine's own session" and must not auto-spawn or
    /// auto-retry the way the sentinel does.
    pub fn is_localhost(&self) -> bool {
        matches!(self, Self::LocalSession(_))
    }
}

/// An ssh-reached target: the raw string as the user/config wrote it,
/// plus a filesystem-safe token derived from it.
///
/// The token is what names this host's slice of the filesystem — its
/// control-socket directory, its generated `ssh_config` — so it must be
/// short enough to leave room for the file names underneath it (see
/// [`sun_path`]) and distinct enough that two different targets can
/// never collide on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// The trimmed target string, exactly as classified — `ssh://`
    /// targets keep their original casing.
    pub raw: String,
    /// A filesystem-safe token derived from `raw`: characters outside
    /// `[A-Za-z0-9._-]` sanitized to `-`, the result truncated to 32
    /// characters, then `-` plus 16 lowercase hex characters of a
    /// stable 64-bit hash of the *full* `raw` string (not the
    /// truncated prefix) — so two targets that only differ after
    /// character 32, or that sanitize to the same prefix (`user@host`
    /// and `user-host` both sanitize to `user-host`), still produce
    /// different tokens.
    pub token: String,
}

impl SshTarget {
    fn new(raw: &str) -> Self {
        Self {
            raw: raw.to_string(),
            token: host_token(raw),
        }
    }
}

/// Classify a saved host's `target` string. See [`ResolvedTransport`]
/// for the rule table.
pub fn classify(target: &str) -> Result<ResolvedTransport> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("target is empty"));
    }
    if trimmed == LOCALHOST_TARGET {
        return Ok(ResolvedTransport::LocalSession(
            BundleProfile::session()?.socket_path,
        ));
    }
    if trimmed.starts_with('-') {
        return Err(anyhow!(
            "target {trimmed:?} starts with '-'; that looks like an option, not a host"
        ));
    }
    if starts_with_ssh_scheme(trimmed) {
        if trimmed.len() == SSH_SCHEME.len() {
            return Err(anyhow!("target {trimmed:?} has no host after the scheme"));
        }
        return Ok(ResolvedTransport::Ssh(SshTarget::new(trimmed)));
    }
    let has_slash = trimmed.contains('/');
    if !has_slash && trimmed.contains(':') {
        return Err(anyhow!(
            "target {trimmed:?} looks like host:port; spell an ssh target as \
             ssh://host:port (brackets for an IPv6 literal: ssh://[::1]:22)"
        ));
    }
    if !has_slash {
        return Ok(ResolvedTransport::Ssh(SshTarget::new(trimmed)));
    }
    Ok(ResolvedTransport::UnixSocket(PathBuf::from(trimmed)))
}

/// Whether `target` names this machine's own session — [`classify`]'s
/// rule 2, answered on its own.
///
/// Deliberately not `classify(target).map(|t| t.is_localhost())`: rule 2
/// resolves the session profile's socket path, which allocates, can
/// fail, and answers a question this caller did not ask. The sidebar
/// asks this once per saved host on every view refresh, and the honest
/// cheap answer is the string comparison rule 2 is defined as.
pub fn target_is_localhost(target: &str) -> bool {
    target.trim() == LOCALHOST_TARGET
}

const SSH_SCHEME: &str = "ssh://";

/// `s` starts with `ssh://`, matched byte-for-byte ASCII
/// case-insensitively. Comparing bytes rather than slicing `s` on a
/// `str` boundary means this never has to worry about `s` containing a
/// multibyte character within its first six bytes — the scheme itself
/// is pure ASCII, so a byte-for-byte compare is exact.
fn starts_with_ssh_scheme(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= SSH_SCHEME.len()
        && bytes[..SSH_SCHEME.len()].eq_ignore_ascii_case(SSH_SCHEME.as_bytes())
}

/// Sanitize + truncate + hash `raw` into a filesystem-safe token. See
/// [`SshTarget::token`] for the shape and why the hash covers the full
/// string rather than the truncated prefix.
fn host_token(raw: &str) -> String {
    let truncated: String = raw
        .chars()
        .take(32)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{truncated}-{:016x}", fnv1a64(raw.as_bytes()))
}

/// FNV-1a, 64-bit. Deliberately not `std::hash::DefaultHasher`: that
/// hasher's algorithm is an implementation detail Rust does not
/// guarantee across releases, and this token is meant to be stable —
/// the same target must always sanitize to the same on-disk directory,
/// including across a `cargo update`.
fn fnv1a64(input: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

// ============================================================================
// The generated ssh config
// ============================================================================

/// The keepalive + timeout `Host *` block appended after the two
/// `Include` lines. Fixed regardless of which config files exist.
const HOST_STAR_BLOCK: &str =
    "Host *\n  ServerAliveInterval 15\n  ServerAliveCountMax 4\n  ConnectTimeout 15\n";

/// Render the generated `ssh_config` Roost points every tunnel `ssh`
/// invocation at via `-F`.
///
/// `user_config` and `system_config` are the two files this config
/// would `Include`, already checked for existence by the caller (C3,
/// which owns the filesystem) — an `Include` line is only ever emitted
/// for a path that was passed in `Some`. Keeping existence-checking
/// outside this function is what keeps it pure and independent of
/// `$HOME` and `/etc` at test time.
///
/// The user's config is included *first* so their own settings —
/// including any keepalive of their own — win over the fallback `Host
/// *` block this appends; ssh takes the first matching value for a
/// given keyword. No `ControlMaster`, `ControlPath`, `ControlPersist`,
/// or `BatchMode` appear here — those vary per invocation
/// (establish/exec/verify each want something different) and are
/// argv-only, built by [`establish_argv`] and friends.
pub fn generate_ssh_config(user_config: Option<&Path>, system_config: Option<&Path>) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for path in [user_config, system_config].into_iter().flatten() {
        let _ = writeln!(out, "Include \"{}\"", path.display());
    }
    out.push_str(HOST_STAR_BLOCK);
    out
}

// ============================================================================
// Argv builders
// ============================================================================

/// The remote command every mux `ssh` invocation execs: prefer
/// `~/.local/bin/roost-session` (the bootstrap install location) and
/// fall back to a bare `roost-session` resolved off the remote's
/// non-interactive `PATH`. One argv element — the whole thing is
/// `sh -c '<script>'`, so the far end never has to source a login
/// shell just to find the binary.
pub fn remote_command() -> String {
    r#"sh -c 'R="$HOME/.local/bin/roost-session"; [ -x "$R" ] || R=roost-session; exec "$R" client-bridge'"#.to_string()
}

/// The `-F`/`-S`/mux-option/`-T` prefix shared by [`exec_argv`] and
/// [`establish_argv`] — both open (or reuse, via `ControlMaster=auto`)
/// the same shared mux against the same target; they differ only in
/// what they run once connected.
fn mux_connect_argv(config_path: &Path, ctl_path: &Path, target: &str) -> Vec<String> {
    vec![
        "-F".to_string(),
        config_path.display().to_string(),
        "-S".to_string(),
        ctl_path.display().to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        "ControlPersist=60s".to_string(),
        "-o".to_string(),
        "RequestTTY=no".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-T".to_string(),
        target.to_string(),
    ]
}

/// Argv for the per-connection exec: opens (or reuses) the shared mux
/// and execs the remote bridge with a fresh stdio pipe of its own.
pub fn exec_argv(config_path: &Path, ctl_path: &Path, target: &str) -> Vec<String> {
    let mut argv = mux_connect_argv(config_path, ctl_path, target);
    argv.push(remote_command());
    argv
}

/// Argv for the warm-up connection that establishes the mux ahead of
/// the first real exec, so the first tab a user opens does not pay for
/// both the TCP/auth handshake and the bridge spawn serially.
pub fn establish_argv(config_path: &Path, ctl_path: &Path, target: &str) -> Vec<String> {
    let mut argv = mux_connect_argv(config_path, ctl_path, target);
    argv.push("true".to_string());
    argv
}

/// Argv that tears the mux down: `-O exit` asks the running master to
/// close, taking every multiplexed connection over it with it. No
/// `-T`, no remote command — this never execs anything remote.
pub fn teardown_argv(config_path: &Path, ctl_path: &Path, target: &str) -> Vec<String> {
    vec![
        "-F".to_string(),
        config_path.display().to_string(),
        "-S".to_string(),
        ctl_path.display().to_string(),
        "-O".to_string(),
        "exit".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        target.to_string(),
    ]
}

/// Argv for a one-shot verification connection — used to check a
/// target answers before committing to a mux. Deliberately outside the
/// mux: no `-S`, `ControlMaster=no`, no `ControlPersist`, so a stale or
/// wedged control socket from a previous attempt can never make this
/// probe report a false positive.
pub fn verify_argv(config_path: &Path, target: &str, remote_command: &str) -> Vec<String> {
    vec![
        "-F".to_string(),
        config_path.display().to_string(),
        "-o".to_string(),
        "ControlMaster=no".to_string(),
        "-o".to_string(),
        "RequestTTY=no".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-T".to_string(),
        target.to_string(),
        remote_command.to_string(),
    ]
}

// ============================================================================
// Sizing the control-socket directory
// ============================================================================

/// The `-S` mux control socket, created by `ssh` itself.
const CTL_FILE: &str = "ctl";
/// The local bridge's own listening socket — the path a client dials to
/// reach the remote session.
const BRIDGE_FILE: &str = "bridge.sock";
/// The generated `ssh_config` every invocation is pointed at with `-F`.
/// Not a socket, so it is not part of the `sun_path` probe.
const CONFIG_FILE: &str = "ssh_config";

/// The two file names created under a host's control-socket directory.
/// Both are `AF_UNIX` paths and both must fit `sun_path`, so the probe
/// below checks the longer of the two against every candidate.
/// What the length probe must measure for the control socket: OpenSSH
/// binds the ControlPath as a temporary `ctl.XXXXXXXXXXXXXXXX` (sixteen
/// random characters after a dot) and renames it into place, so the
/// *bindable* name is 17 bytes longer than the one that ends up on
/// disk. Measuring bare `ctl` passed directories whose real bind then
/// failed with ssh's own "path too long for Unix domain socket" —
/// found live on macOS, whose `$TMPDIR` is deep (plan 038 §8).
const CTL_BIND_PROBE: &str = "ctl.XXXXXXXXXXXXXXXX";

const SOCKET_FILE_NAMES: [&str; 2] = [CTL_BIND_PROBE, BRIDGE_FILE];

/// The prefix every scratch directory this module creates shares.
const SCRATCH_PREFIX: &str = "roost-ssh-";

/// Separates two scratch directories this process minted for the same
/// host. Process-global and never reset: a name has to stay unique for
/// the life of the process, not merely of one tunnel.
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// The leaf directory name one attempt claims:
/// `roost-ssh-<host_id>-<pid>-<seq>`.
///
/// Per *attempt*, not per host, and that is the whole point. A name
/// derived from the saved host alone means every overlapping lifecycle
/// path for that host — a double Connect, a disconnect racing the
/// reconnect behind it, a superseded establish landing late — writes and
/// removes the *same* directory, so the loser's cleanup takes the
/// winner's files with it. Uniqueness turns "who owns this directory"
/// into "which of these directories is still owned", which
/// [`sweep_scratch_dirs`] answers before a new one is created.
pub fn scratch_dir_name(host_id: &str) -> String {
    format!(
        "{SCRATCH_PREFIX}{host_id}-{}-{}",
        std::process::id(),
        SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Read a leaf directory name back as `(host_id, pid, seq)`, or `None`
/// when it is not one this module minted for a host.
///
/// Parsed from the *right*: a saved host's id is opaque here and a `-`
/// inside it must not shift the fields, so the last two `-`-separated
/// segments are the pid and the sequence and everything between the
/// prefix and them is the host id. Both numbers have to parse, which is
/// also what keeps a verify directory
/// (`roost-ssh-verify-<pid>-<nanos>-<n>`) from ever reading as a host's.
pub fn parse_scratch_dir_name(name: &str) -> Option<(&str, u32, u64)> {
    let rest = name.strip_prefix(SCRATCH_PREFIX)?;
    let (rest, seq) = rest.rsplit_once('-')?;
    let (host_id, pid) = rest.rsplit_once('-')?;
    if host_id.is_empty() {
        return None;
    }
    Some((host_id, pid.parse().ok()?, seq.parse().ok()?))
}

/// The historical BSD/Linux/macOS `sockaddr_un.sun_path` size: 104
/// bytes total, one of which is the mandatory NUL terminator, leaving
/// 103 usable path bytes.
pub const SUN_PATH_MAX: usize = 103;

/// Find the first of `candidate_dirs` under which
/// `<dir>/<dir_name>/<file>` fits [`SUN_PATH_MAX`] for every name in
/// [`SOCKET_FILE_NAMES`] — checking the longer file name is sufficient
/// since both live at the same depth.
///
/// `dir_name` is the *final* leaf name, pid and sequence included
/// ([`scratch_dir_name`] builds it), so what is length-checked here is
/// exactly what will be bound later. A caller that measured a shorter
/// stand-in would have measured a path nothing uses.
///
/// Directory creation is C3's job; this only picks *where*. Candidates
/// are tried in order (typically `$TMPDIR` then `/tmp`) because a
/// shorter fallback is only worth using when the preferred one does
/// not fit — `/tmp` is shared and less private than a per-user
/// `$TMPDIR`.
pub fn pick_socket_dir(candidate_dirs: &[PathBuf], dir_name: &str) -> Result<PathBuf> {
    for dir in candidate_dirs {
        let base = dir.join(dir_name);
        let longest = SOCKET_FILE_NAMES
            .iter()
            .map(|name| base.join(name))
            .max_by_key(|path| path.as_os_str().len())
            .expect("SOCKET_FILE_NAMES is non-empty");
        if longest.as_os_str().len() <= SUN_PATH_MAX {
            return Ok(base);
        }
    }
    Err(anyhow!(
        "no candidate directory leaves room for a {SUN_PATH_MAX}-byte AF_UNIX socket path \
         under {dir_name}; tried: {}",
        candidate_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

// ============================================================================
// Classifying a failed connection
// ============================================================================

/// What kind of failure a tunnel connection attempt hit, and the
/// user-facing copy for it. [`classify_ssh_failure`] is the sole
/// producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshFailure {
    /// The host key ssh saw does not match what is pinned —
    /// `REMOTE HOST IDENTIFICATION HAS CHANGED`. The one case where the
    /// copy must never suggest accepting anything: this is exactly what
    /// a machine-in-the-middle attack looks like from here, and the
    /// only safe move is verifying out-of-band.
    ChangedHostKey,
    /// ssh refused an unrecognized host key —
    /// `Host key verification failed`, with no prior pin to contradict.
    HostKeyUnknown,
    /// Authentication was refused — `Permission denied`.
    Auth,
    /// The bridge ran but found no session to attach to —
    /// `client-bridge: no session`.
    NoSession,
    /// `roost-session` is not on the remote's non-interactive `PATH` —
    /// `command not found` in stderr, or ssh's own exit code 127 for
    /// "command not found" with nothing useful in stderr.
    NotFound,
    /// None of the above matched. Carries the last non-empty line of
    /// `stderr_tail`, or `None` when there was nothing to carry.
    Transport(Option<String>),
}

impl SshFailure {
    /// User-facing copy, with `target` interpolated where the message
    /// tells the user what to run next.
    pub fn message(&self, target: &str) -> String {
        match self {
            Self::ChangedHostKey => format!(
                "the host key for {target} has CHANGED since it was last seen — this can mean \
                 the host was reinstalled, or that something is impersonating it. Do not accept \
                 the new key from here; verify its fingerprint with {target} out-of-band (e.g. a \
                 call, or a channel other than this one) before connecting again."
            ),
            Self::HostKeyUnknown => format!(
                "{target}'s host key has not been seen before. Run `ssh {target}` once in a \
                 terminal to review and accept it, then try again."
            ),
            Self::Auth => format!(
                "{target} refused authentication. Check that your key is loaded in an agent, \
                 then try `ssh {target}` in a terminal to confirm you can log in."
            ),
            Self::NoSession => format!(
                "{target} is reachable but has no roost session running. Run `roostctl session \
                 start` on that machine, then try again."
            ),
            Self::NotFound => format!(
                "roost-session isn't installed on {target} (or isn't on the non-interactive \
                 PATH ssh uses there)."
            ),
            Self::Transport(Some(line)) => format!("connecting to {target} failed: {line}"),
            Self::Transport(None) => format!("connecting to {target} failed"),
        }
    }
}

/// Classify an ssh connection attempt's outcome from its exit code and
/// the tail of its stderr. First match wins, checked as substrings
/// over `stderr_tail`:
///
/// 1. `REMOTE HOST IDENTIFICATION HAS CHANGED` → [`SshFailure::ChangedHostKey`]
/// 2. `Host key verification failed` → [`SshFailure::HostKeyUnknown`]
/// 3. `Permission denied` → [`SshFailure::Auth`]
/// 4. `client-bridge: no session` → [`SshFailure::NoSession`]
/// 5. `command not found`, or `exit_code == Some(127)` → [`SshFailure::NotFound`]
/// 6. Otherwise → [`SshFailure::Transport`], carrying the last
///    non-empty line of `stderr_tail`.
///
/// Order 1 before 2 matters: ssh's own changed-key message *also*
/// contains the string "Host key verification failed" further down the
/// blob, so checking rule 2 first would misreport a changed key — the
/// wary case — as the far more common unknown-key case.
pub fn classify_ssh_failure(exit_code: Option<i32>, stderr_tail: &str) -> SshFailure {
    if stderr_tail.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return SshFailure::ChangedHostKey;
    }
    if stderr_tail.contains("Host key verification failed") {
        return SshFailure::HostKeyUnknown;
    }
    if stderr_tail.contains("Permission denied") {
        return SshFailure::Auth;
    }
    if stderr_tail.contains("client-bridge: no session") {
        return SshFailure::NoSession;
    }
    if stderr_tail.contains("command not found") || exit_code == Some(127) {
        return SshFailure::NotFound;
    }
    let last_line = stderr_tail
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string);
    SshFailure::Transport(last_line)
}

// ============================================================================
// The tunnel runtime
// ============================================================================

/// Override naming the `ssh` binary the tunnel execs. Read once, at
/// construction — a tunnel that resolved it per invocation could exec
/// two different programs over the life of one connection.
pub const SSH_BIN_ENV: &str = "ROOST_SSH_BIN";

/// How long the warm-up connection gets to open the mux. Wide, because
/// it covers a full TCP + auth handshake on a cold link, and the config
/// this module generates already caps `ConnectTimeout` at 15s.
const ESTABLISH_BUDGET: Duration = Duration::from_secs(30);

/// How long a `-O exit` gets. It is a local round trip to the master's
/// control socket; anything past this means the master is wedged, and
/// the directory removal behind it must not wait on that.
const TEARDOWN_BUDGET: Duration = Duration::from_secs(2);

/// How long [`SshTunnel::shutdown`] lets in-flight connections finish
/// before aborting them.
const DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// How long a finished connection's `ssh` child gets to exit on its own
/// before it is killed.
const REAP_BUDGET: Duration = Duration::from_secs(2);

/// Poll interval for [`Drop`]'s blocking wait. Only ever spent on a
/// `-O exit` that is already on its way out.
const DROP_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// How much of a failed invocation's stderr is kept. `ssh`'s
/// changed-host-key blob is the longest thing that has to survive
/// intact, and it is well under this.
const STDERR_TAIL_BYTES: usize = 4 * 1024;

/// One read's worth of wire bytes, matched to the far-side bridge's own
/// chunk so a snapshot stream is not chopped into a syscall per frame.
const CHUNK: usize = 64 * 1024;

/// Every budget in this module goes through the ambient test scale.
fn scaled(budget: Duration) -> Duration {
    budget.mul_f64(timeout_scale())
}

const ACCEPT_MUTEX: &str = "ssh tunnel accept mutex";
const CONNECTIONS_MUTEX: &str = "ssh tunnel connections mutex";
const LAST_ERROR_MUTEX: &str = "ssh tunnel last_error mutex";

/// The two `ssh_config` files the generated config includes.
///
/// Both are `Option` because neither is guaranteed to exist: a user
/// with no `~/.ssh/config`, or no `$HOME` at all, is ordinary. Existence
/// is checked at write time — [`generate_ssh_config`] only ever sees a
/// path that is really there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshConfigPaths {
    pub user: Option<PathBuf>,
    pub system: Option<PathBuf>,
}

impl SshConfigPaths {
    /// `$HOME/.ssh/config` plus `/etc/ssh/ssh_config`.
    pub fn from_env() -> Self {
        Self {
            user: std::env::var_os("HOME").map(|home| {
                let mut path = PathBuf::from(home);
                path.push(".ssh");
                path.push("config");
                path
            }),
            system: Some(PathBuf::from("/etc/ssh/ssh_config")),
        }
    }

    fn render(&self) -> String {
        fn existing(candidate: &Option<PathBuf>) -> Option<&Path> {
            candidate.as_deref().filter(|path| path.exists())
        }
        generate_ssh_config(existing(&self.user), existing(&self.system))
    }
}

/// Everything a tunnel reads from its environment, in one injectable
/// place.
///
/// Every field is a value rather than a lookup so a test can point a
/// whole tunnel at a fake `ssh` and a scratch directory of its own
/// without mutating process-global environment that every other test in
/// the same binary also reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTunnelOptions {
    pub config_paths: SshConfigPaths,
    /// Candidate parents for the scratch directory, in preference
    /// order — the first one that leaves room for a `sun_path` wins.
    pub scratch_parents: Vec<PathBuf>,
    pub ssh_bin: PathBuf,
}

impl SshTunnelOptions {
    /// `$TMPDIR` then `/tmp`, `$ROOST_SSH_BIN` or `ssh`, and the
    /// standard config pair.
    pub fn from_env() -> Self {
        let mut scratch_parents: Vec<PathBuf> = Vec::new();
        if let Some(tmpdir) = std::env::var_os("TMPDIR").filter(|value| !value.is_empty()) {
            scratch_parents.push(PathBuf::from(tmpdir));
        }
        let fallback = PathBuf::from("/tmp");
        if !scratch_parents.contains(&fallback) {
            scratch_parents.push(fallback);
        }
        Self {
            config_paths: SshConfigPaths::from_env(),
            scratch_parents,
            ssh_bin: std::env::var_os(SSH_BIN_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("ssh")),
        }
    }
}

/// What went wrong reaching a target.
///
/// The split is by who can act on it: [`Self::Ssh`] carries a classified
/// [`SshFailure`] a caller can branch on (and whose `message` is written
/// for a user), while [`Self::Local`] is this side's own failure — a
/// scratch directory that could not be created, a socket that could not
/// be bound — which has no family and no remedy on the far host.
#[derive(Debug, thiserror::Error)]
pub enum SshTunnelError {
    #[error("{}", .failure.message(.target))]
    Ssh { target: String, failure: SshFailure },
    #[error("{0:#}")]
    Local(#[from] anyhow::Error),
}

impl SshTunnelError {
    fn ssh(target: &str, failure: SshFailure) -> Self {
        Self::Ssh {
            target: target.to_string(),
            failure,
        }
    }

    /// The classified family, when the failure was the far side's.
    pub fn failure(&self) -> Option<&SshFailure> {
        match self {
            Self::Ssh { failure, .. } => Some(failure),
            Self::Local(_) => None,
        }
    }
}

/// A live SSH transport to one saved host: a local `bridge.sock`, a
/// shared `ssh` mux behind it, and one `ssh` exec per accepted
/// connection.
///
/// One per saved-host id at a time, in a scratch directory named for
/// that id *plus this attempt* — so a crashed client's leftovers are
/// still findable, without two attempts ever sharing a directory. See
/// [`Self::open`].
///
/// **Spawn-per-connection is the pinned shape.** Every accepted
/// connection gets its own task and its own `ssh` exec over the shared
/// master, so a client that opens an events connection and holds it for
/// the session's lifetime cannot keep the next `tab.attach` waiting.
/// Serializing connections through one task would deadlock exactly
/// there, and that is the failure mode this design exists to avoid.
pub struct SshTunnel {
    state: Arc<TunnelState>,
    dir: PathBuf,
    bridge_path: PathBuf,
    accept: Mutex<Option<JoinHandle<()>>>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

/// The half of a tunnel every connection task needs a handle on.
struct TunnelState {
    target: String,
    ssh_bin: PathBuf,
    config_path: PathBuf,
    ctl_path: PathBuf,
    /// Bumped once per `ssh` exec, so a caller can tell "the failure I
    /// already reported" from "a new one since".
    generation: AtomicU64,
    last_error: Mutex<Option<(u64, SshFailure)>>,
    /// Set by `shutdown`/`Drop`. The accept loop reads it so a
    /// connection accepted in the window between the abort request and
    /// the task actually stopping is dropped rather than left running
    /// with nothing tracking it.
    closed: AtomicBool,
}

impl SshTunnel {
    /// Sweep this host's older scratch directories away, claim a fresh
    /// one of this attempt's own, and write its generated config. Binds
    /// nothing and connects nothing — that is [`Self::establish`].
    ///
    /// The name this attempt claims is unique
    /// ([`scratch_dir_name`] — host id, pid, sequence), so no two
    /// attempts can ever write or remove each other's files. What makes a
    /// crashed client's leftovers findable rather than merely orphaned is
    /// the *sweep*: every `roost-ssh-<this host id>-*` directory under the
    /// chosen parent is examined before the new one is created.
    ///
    /// Two rules, by whose leftovers they are:
    ///
    /// * **This process's** — superseded by construction, since the
    ///   caller above opens one tunnel per saved host at a time — are
    ///   reclaimed with no probe at all.
    /// * **Another process's** are fail-safe in exactly the way
    ///   [`crate::socket_state`] is: a `bridge.sock` that answers, or
    ///   that cannot be classified at all, means another Roost owns this
    ///   target and this one refuses. Only a socket that is provably dead
    ///   — or absent — authorizes a reclaim.
    ///
    /// Either reclaim runs `-O exit` against the *old* control socket
    /// before removing anything. That client is gone but its `ssh` master
    /// is not: `ControlPersist` keeps it alive for its own timeout, and
    /// removing the control socket out from under it would strand a
    /// process nothing can address any more.
    pub async fn open(
        host_id: &str,
        target: &SshTarget,
        options: SshTunnelOptions,
    ) -> Result<Self, SshTunnelError> {
        let dir = pick_socket_dir(&options.scratch_parents, &scratch_dir_name(host_id))?;
        let bridge_path = dir.join(BRIDGE_FILE);
        let ctl_path = dir.join(CTL_FILE);
        let config_path = dir.join(CONFIG_FILE);

        // Only the parent this attempt landed in is swept: an orphan
        // under the *other* candidate was sized against a different
        // `sun_path` budget, holds no socket this one can collide with,
        // and its master reaps itself on `ControlPersist`.
        let parent = dir
            .parent()
            .expect("pick_socket_dir joins a leaf name onto a candidate");
        sweep_scratch_dirs(&options.ssh_bin, &target.raw, parent, host_id).await?;

        // `create_new` semantics: the name carries a pid and a sequence,
        // so a collision is a real error rather than something to reclaim.
        create_private_dir(&dir)
            .with_context(|| format!("create the scratch directory {}", dir.display()))?;

        write_private_file(&config_path, options.config_paths.render().as_bytes())
            .with_context(|| format!("write {}", config_path.display()))?;

        Ok(Self {
            state: Arc::new(TunnelState {
                target: target.raw.clone(),
                ssh_bin: options.ssh_bin,
                config_path,
                ctl_path,
                generation: AtomicU64::new(0),
                last_error: Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
            dir,
            bridge_path,
            accept: Mutex::new(None),
            connections: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// The path a client dials. Only bound once [`Self::establish`] has
    /// succeeded.
    pub fn bridge_socket(&self) -> &Path {
        &self.bridge_path
    }

    /// The most recent connection failure, with the generation of the
    /// exec that hit it.
    pub fn last_error(&self) -> Option<(u64, SshFailure)> {
        self.state
            .last_error
            .lock()
            .expect(LAST_ERROR_MUTEX)
            .clone()
    }

    /// Open the shared mux, then bind `bridge.sock` and start accepting.
    ///
    /// The warm-up exec runs `true` remotely: the point is to pay for
    /// the TCP + auth handshake once, here, where a failure can still be
    /// classified and reported as itself. Without it the first tab a
    /// user opens pays for the handshake *and* the bridge spawn
    /// serially, and an auth failure surfaces as a terminal that closed.
    ///
    /// A failure never leaves `bridge.sock` bound — the listener is only
    /// created once the warm-up has come back clean — so a caller that
    /// retries is not racing its own corpse.
    pub async fn establish(&self) -> Result<(), SshTunnelError> {
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(SshTunnelError::Local(anyhow!(
                "this tunnel to {} is shut down",
                self.state.target
            )));
        }
        if self.accept.lock().expect(ACCEPT_MUTEX).is_some() {
            return Err(SshTunnelError::Local(anyhow!(
                "this tunnel to {} is already established",
                self.state.target
            )));
        }

        let budget = scaled(ESTABLISH_BUDGET);
        let deadline = Instant::now() + budget;
        let argv = establish_argv(
            &self.state.config_path,
            &self.state.ctl_path,
            &self.state.target,
        );
        let mut child = spawn_ssh_command(
            &self.state.ssh_bin,
            &argv,
            Stdio::null(),
            Stdio::null(),
            Stdio::piped(),
        )?;
        let tail = spawn_stderr_tail(&mut child);

        match tokio::time::timeout_at(deadline, child.wait()).await {
            Err(_elapsed) => {
                // Nothing is left of the budget, so the reap deadline is
                // now: kill it and take the corpse.
                reap_by(&mut child, Instant::now()).await;
                let _ = tail.await;
                Err(SshTunnelError::ssh(
                    &self.state.target,
                    SshFailure::Transport(Some(format!(
                        "timed out after {}s",
                        budget.as_secs().max(1)
                    ))),
                ))
            }
            Ok(Err(error)) => Err(SshTunnelError::Local(
                anyhow::Error::from(error).context("wait for the ssh warm-up connection"),
            )),
            Ok(Ok(status)) if !status.success() => Err(SshTunnelError::ssh(
                &self.state.target,
                classify_ssh_failure(status.code(), &tail.await.unwrap_or_default()),
            )),
            Ok(Ok(_)) => {
                // Bind and register under the accept lock, with a closed
                // re-check inside it: a shutdown that ran during the
                // warm-up has already taken (and will never re-take)
                // this lock, so the loop either registers where shutdown
                // can abort it, or never starts.
                let mut accept = self.accept.lock().expect(ACCEPT_MUTEX);
                if self.state.closed.load(Ordering::SeqCst) {
                    return Err(SshTunnelError::Local(anyhow!(
                        "this tunnel to {} is shut down",
                        self.state.target
                    )));
                }
                let listener = bind_bridge(&self.bridge_path)?;
                *accept = Some(tokio::spawn(accept_loop(
                    listener,
                    self.state.clone(),
                    self.connections.clone(),
                )));
                Ok(())
            }
        }
    }

    /// Stop accepting, let in-flight connections finish, close the mux,
    /// and remove the scratch directory. Idempotent.
    ///
    /// Ordered, not merely thorough: `-O exit` has to run while the
    /// control socket is still on disk (it is the only address the
    /// master has), and the directory can only go once it has.
    pub async fn shutdown(&self) {
        if self.state.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.stop_accepting();

        let handles = self.take_connections();
        let aborts: Vec<_> = handles.iter().map(JoinHandle::abort_handle).collect();
        let drained = tokio::time::timeout(scaled(DRAIN_BUDGET), async {
            for handle in handles {
                let _ = handle.await;
            }
        })
        .await;
        if drained.is_err() {
            // Each task's child is `kill_on_drop`, so aborting is what
            // kills the `ssh` behind a connection that will not end.
            for abort in aborts {
                abort.abort();
            }
        }

        exit_master(
            &self.state.ssh_bin,
            &self.state.config_path,
            &self.state.ctl_path,
            &self.state.target,
        )
        .await;
        if let Err(error) = tokio::fs::remove_dir_all(&self.dir).await {
            // Already gone is ordinary, not a fault: a superseded
            // tunnel's shutdown lands after its replacement's sweep has
            // reclaimed this directory (same pid, no probe).
            if error.kind() == std::io::ErrorKind::NotFound {
                tracing::debug!(
                    dir = %self.dir.display(),
                    "ssh tunnel: the scratch directory was already reclaimed"
                );
            } else {
                tracing::warn!(
                    dir = %self.dir.display(),
                    %error,
                    "ssh tunnel: could not remove the scratch directory"
                );
            }
        }
    }

    fn stop_accepting(&self) {
        if let Some(accept) = self.accept.lock().expect(ACCEPT_MUTEX).take() {
            accept.abort();
        }
    }

    fn take_connections(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut *self.connections.lock().expect(CONNECTIONS_MUTEX))
    }
}

/// The teardown a tunnel that was never shut down still owes.
///
/// Blocking on purpose: `Drop` has no runtime to await on and may well
/// be running as one shuts down. The alternative — spawning the exit —
/// is a task that reliably never runs. The wait is bounded, and what it
/// is waiting for is a local round trip to a control socket.
impl Drop for SshTunnel {
    fn drop(&mut self) {
        if self.state.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.stop_accepting();
        for handle in self.take_connections() {
            handle.abort();
        }
        blocking_exit_master(
            &self.state.ssh_bin,
            &self.state.config_path,
            &self.state.ctl_path,
            &self.state.target,
        );
        // Ignored rather than reported: this directory is this tunnel's
        // alone, and the only way it is already gone is a replacement's
        // sweep having reclaimed it.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl TunnelState {
    /// Serve one accepted connection over its own `ssh` exec.
    async fn serve(self: Arc<Self>, stream: UnixStream) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let argv = exec_argv(&self.config_path, &self.ctl_path, &self.target);
        let mut child = match spawn_ssh_command(
            &self.ssh_bin,
            &argv,
            Stdio::piped(),
            Stdio::piped(),
            Stdio::piped(),
        ) {
            Ok(child) => child,
            Err(error) => {
                self.record(
                    generation,
                    SshFailure::Transport(Some(format!("{error:#}"))),
                );
                return;
            }
        };
        let tail = spawn_stderr_tail(&mut child);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let (mut socket_read, mut socket_write) = stream.into_split();

        // Upstream runs as its own task: a client that holds its write
        // half open for the connection's life (an events subscriber)
        // must not pin this exec after the wire is finished.
        let upstream = tokio::spawn(async move {
            if let Some(mut stdin) = stdin {
                pump(&mut socket_read, &mut stdin, "client to ssh").await;
                // Dropping the handle here is the half-close: the far
                // side's bridge must read a real EOF, not a connection
                // that merely went quiet.
            }
        });
        if let Some(mut stdout) = stdout {
            pump(&mut stdout, &mut socket_write, "ssh to client").await;
        }
        let _ = socket_write.shutdown().await;

        // Child stdout has EOFed, so the exec is done or dying; the
        // client may never close its half, so the wire's end is what
        // ends the upstream pump.
        let exit = match tokio::time::timeout_at(Instant::now() + scaled(REAP_BUDGET), child.wait())
            .await
        {
            Ok(Ok(status)) => Some(status),
            Ok(Err(_)) => None,
            Err(_elapsed) => {
                let _ = child.kill().await;
                None
            }
        };
        upstream.abort();
        let _ = upstream.await;

        // Only the exec's own verdict is a failure: a connection that
        // opened and closed without traffic (a probe) is not one.
        if exit.is_none_or(|status| !status.success()) {
            let tail = tail.await.unwrap_or_default();
            self.record(
                generation,
                classify_ssh_failure(exit.and_then(|status| status.code()), &tail),
            );
        }
    }

    /// Record a connection failure, unless a *newer* exec already
    /// reported one. Connections overlap, so completion order does not
    /// follow generation order, and the last writer would otherwise be
    /// able to overwrite fresher news with staler.
    fn record(&self, generation: u64, failure: SshFailure) {
        tracing::warn!(
            host = %self.target,
            generation,
            failure = %failure.message(&self.target),
            "ssh tunnel connection failed"
        );
        let mut last = self.last_error.lock().expect(LAST_ERROR_MUTEX);
        if last.as_ref().is_none_or(|(seen, _)| *seen <= generation) {
            *last = Some((generation, failure));
        }
    }
}

/// `-O exit`, bounded, status ignored. Only ever worth running while the
/// control socket exists — without it there is no master to address, and
/// `ssh` would go open a fresh connection to say so.
async fn exit_master(ssh_bin: &Path, config_path: &Path, ctl_path: &Path, target: &str) {
    if !ctl_path.exists() {
        return;
    }
    let argv = teardown_argv(config_path, ctl_path, target);
    match spawn_ssh_command(ssh_bin, &argv, Stdio::null(), Stdio::null(), Stdio::null()) {
        Ok(mut child) => reap_by(&mut child, Instant::now() + scaled(TEARDOWN_BUDGET)).await,
        Err(error) => tracing::debug!(host = %target, %error, "ssh tunnel: -O exit"),
    }
}

/// [`exit_master`] without a runtime, for [`Drop`].
fn blocking_exit_master(ssh_bin: &Path, config_path: &Path, ctl_path: &Path, target: &str) {
    if !ctl_path.exists() {
        return;
    }
    let argv = teardown_argv(config_path, ctl_path, target);
    let Ok(mut child) =
        ssh_command(ssh_bin, &argv, Stdio::null(), Stdio::null(), Stdio::null()).spawn()
    else {
        return;
    };
    let deadline = std::time::Instant::now() + scaled(TEARDOWN_BUDGET);
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(DROP_POLL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

async fn accept_loop(
    listener: UnixListener,
    state: Arc<TunnelState>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _addr)) => stream,
            Err(error) => {
                tracing::warn!(
                    host = %state.target,
                    %error,
                    "ssh tunnel: accept failed; no longer accepting connections"
                );
                return;
            }
        };
        let served = state.clone();
        let handle = tokio::spawn(async move { served.serve(stream).await });
        let mut tracked = connections.lock().expect(CONNECTIONS_MUTEX);
        if state.closed.load(Ordering::SeqCst) {
            handle.abort();
            return;
        }
        // Finished tasks are already reaped; keeping their handles would
        // grow the vector for the life of the tunnel.
        tracked.retain(|handle| !handle.is_finished());
        tracked.push(handle);
    }
}

/// One direction of a connection's byte pump. Returns how many bytes it
/// moved.
///
/// A pump error is not returned because it is not a verdict: a client
/// hanging up mid-stream and a transport that died look identical from
/// here, and only the child's exit status can tell them apart. The
/// caller reads that status; this logs the detail and stops.
async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    from: &mut R,
    to: &mut W,
    direction: &'static str,
) -> u64 {
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let read = match from.read(&mut buf).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                tracing::debug!(direction, %error, "ssh tunnel pump: read ended");
                break;
            }
        };
        if let Err(error) = to.write_all(&buf[..read]).await {
            tracing::debug!(direction, %error, "ssh tunnel pump: write ended");
            break;
        }
        total += read as u64;
    }
    let _ = to.flush().await;
    total
}

/// Every `ssh` this module runs, shaped the same way — including the one
/// [`blocking_exit_master`] has to spawn without a runtime.
fn ssh_command(
    ssh_bin: &Path,
    argv: &[String],
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
) -> std::process::Command {
    let mut command = std::process::Command::new(ssh_bin);
    command
        .args(argv)
        // `ssh`'s stderr is a classification input, and a localized
        // build would spell "Permission denied" in whatever the user's
        // locale is. Pinning the locale keeps the failure table honest.
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    command
}

fn spawn_ssh_command(
    ssh_bin: &Path,
    argv: &[String],
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
) -> Result<Child, SshTunnelError> {
    Command::from(ssh_command(ssh_bin, argv, stdin, stdout, stderr))
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", ssh_bin.display()))
        .map_err(SshTunnelError::Local)
}

/// Drain a child's stderr into a task holding at most the last
/// [`STDERR_TAIL_BYTES`].
///
/// A task rather than a read after `wait()`, because an unread stderr
/// pipe fills and blocks the child that is being waited on.
fn spawn_stderr_tail(child: &mut Child) -> JoinHandle<String> {
    let stderr = child.stderr.take();
    tokio::spawn(async move {
        match stderr {
            Some(stderr) => read_tail(stderr).await,
            None => String::new(),
        }
    })
}

async fn read_tail<R: AsyncRead + Unpin>(mut reader: R) -> String {
    let mut tail: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                tail.extend_from_slice(&buf[..read]);
                if tail.len() > STDERR_TAIL_BYTES {
                    tail.drain(..tail.len() - STDERR_TAIL_BYTES);
                }
            }
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

fn bind_bridge(path: &Path) -> Result<UnixListener, SshTunnelError> {
    use std::os::unix::fs::PermissionsExt;

    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind {}", path.display()))
        .map_err(SshTunnelError::Local)?;
    // Same rule the session socket binds under: the bridge reaches a
    // whole remote workspace, so nobody else on this machine gets to
    // dial it.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))
        .map_err(SshTunnelError::Local)?;
    Ok(listener)
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    Ok(())
}

/// The reclaim half of [`SshTunnel::open`]: every scratch directory
/// `parent` still holds for `host_id`, examined and dealt with before a
/// fresh one is claimed. See [`SshTunnel::open`] for the two rules.
async fn sweep_scratch_dirs(
    ssh_bin: &Path,
    target: &str,
    parent: &Path,
    host_id: &str,
) -> Result<(), SshTunnelError> {
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        // A parent that does not exist holds nothing to reclaim, and the
        // directory creation behind this is what reports it as a problem.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(SshTunnelError::Local(
                anyhow::Error::from(error)
                    .context(format!("read the scratch parent {}", parent.display())),
            ))
        }
    };

    let ours = std::process::id();
    let mut leftovers: Vec<(PathBuf, u32)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((entry_host, pid, _seq)) = name.to_str().and_then(parse_scratch_dir_name) else {
            continue;
        };
        if entry_host == host_id {
            leftovers.push((entry.path(), pid));
        }
    }
    // Directory order is whatever the filesystem hands back; sorting
    // makes a sweep that refuses refuse on the same directory every time.
    leftovers.sort();

    for (dir, pid) in leftovers {
        if pid != ours {
            let bridge_path = dir.join(BRIDGE_FILE);
            match socket_state::probe(&bridge_path, PROBE_TIMEOUT).await {
                SocketState::Missing | SocketState::Stale => {}
                SocketState::NotASocket(kind) => {
                    return Err(SshTunnelError::Local(anyhow!(
                        "{} is a {kind}, not a socket; remove {} by hand",
                        bridge_path.display(),
                        dir.display()
                    )))
                }
                // Live, and anything that cannot be classified: fail-safe,
                // the same rule `socket_state` unlinks under.
                state => {
                    return Err(SshTunnelError::Local(anyhow!(
                        "another Roost is connected to {target} (its bridge socket is \
                         {state:?} at {})",
                        dir.display()
                    )))
                }
            }
        }

        exit_master(ssh_bin, &dir.join(CONFIG_FILE), &dir.join(CTL_FILE), target).await;
        if let Err(error) = tokio::fs::remove_dir_all(&dir).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(SshTunnelError::Local(anyhow::Error::from(error).context(
                    format!("remove the stale scratch directory {}", dir.display()),
                )));
            }
        }
    }
    Ok(())
}

// ============================================================================
// One-shot verification
// ============================================================================

/// Default budget for [`verify_ssh_target`], before the ambient scale.
pub const VERIFY_BUDGET: Duration = Duration::from_secs(30);

/// Ask a classified target for `session.identify`, over whichever
/// transport it named.
///
/// One bar, two ways of getting there. A socket target dials it
/// ([`crate::session_launch::verify_socket`]); an ssh target runs
/// [`verify_ssh_target`], a throwaway `ssh` exec outside any mux that
/// speaks the same exchange over the child's own stdio and leaves no
/// master behind.
///
/// **Both `roostctl host add --verify` and the Add Host dialog's
/// "Add & Connect" call exactly this**, so the two can never promise a
/// different bar than each other (plan 037 §3.5) — the promise is one
/// function rather than two copies of a rule.
///
/// The budgets differ because the work does: a socket dial gets the
/// CI-scaled IPC leg, an ssh verify gets [`VERIFY_BUDGET`] (which
/// `verify_ssh_target` scales itself) — it has a TCP + auth handshake to
/// pay for first. Both are bounded, so an unreachable target fails
/// rather than hanging the caller.
///
/// An ssh failure is flattened to its own `Display` rather than chained
/// as a source: [`SshTunnelError`] already renders the whole message,
/// and wrapping it would make `{:#}` print it twice.
pub async fn verify_transport(transport: &ResolvedTransport) -> Result<SessionIdentify> {
    match transport {
        ResolvedTransport::Ssh(target) => {
            verify_ssh_target(target, &SshTunnelOptions::from_env(), VERIFY_BUDGET)
                .await
                .map_err(|error| anyhow!("{error}"))
        }
        ResolvedTransport::LocalSession(socket) | ResolvedTransport::UnixSocket(socket) => {
            let budget = crate::session_launch::IPC_TIMEOUT.mul_f64(timeout_scale());
            crate::session_launch::verify_socket(socket, budget).await
        }
    }
}

/// Does this ssh target answer, and does it speak a protocol this build
/// can talk to?
///
/// Deliberately leaves nothing behind: one `ssh` exec outside any mux
/// ([`verify_argv`] — `ControlMaster=no`, no `-S`), a config in a
/// throwaway directory, and no master to persist afterwards. That is
/// what makes it safe to offer from an Add Host dialog, where the user
/// has not committed to the host yet and a wedged `ControlPersist`
/// master for a target they typed wrong is not a thing to leave them
/// with.
///
/// It speaks `session.identify` over the child's own stdio rather than
/// through [`crate::IpcClient`], because there is no socket to dial: the
/// remote bridge *is* the pipe. Same frames, same envelope, no
/// transport.
///
/// The compatibility bar matches
/// [`crate::session_launch::verify_target`]'s exactly — "something
/// answered, and it speaks this protocol" — so a host verifies to the
/// same standard however it is reached.
pub async fn verify_ssh_target(
    target: &SshTarget,
    options: &SshTunnelOptions,
    budget: Duration,
) -> Result<SessionIdentify, SshTunnelError> {
    let dir = pick_socket_dir(&options.scratch_parents, &verify_dir_name())?;
    create_private_dir(&dir)
        .with_context(|| format!("create the verify scratch directory {}", dir.display()))?;
    let outcome = verify_in(&dir, target, options, budget).await;
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// A directory name no concurrent verify can collide on: this process,
/// this instant, and a counter for two verifies inside the same
/// nanosecond.
///
/// Deliberately *not* [`scratch_dir_name`]'s shape — a verify belongs to
/// no saved host, and the hex instant in the middle is what stops
/// [`parse_scratch_dir_name`] from ever reading one of these as a host's
/// leftovers for a sweep to reclaim.
fn verify_dir_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    format!(
        "{SCRATCH_PREFIX}verify-{}-{nanos:x}-{:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

async fn verify_in(
    dir: &Path,
    target: &SshTarget,
    options: &SshTunnelOptions,
    budget: Duration,
) -> Result<SessionIdentify, SshTunnelError> {
    let config_path = dir.join(CONFIG_FILE);
    write_private_file(&config_path, options.config_paths.render().as_bytes())
        .with_context(|| format!("write {}", config_path.display()))?;

    let argv = verify_argv(&config_path, &target.raw, &remote_command());
    let mut child = spawn_ssh_command(
        &options.ssh_bin,
        &argv,
        Stdio::piped(),
        Stdio::piped(),
        Stdio::piped(),
    )?;
    let tail = spawn_stderr_tail(&mut child);
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| SshTunnelError::Local(anyhow!("no stdin pipe on the verify connection")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SshTunnelError::Local(anyhow!("no stdout pipe on the verify connection")))?;

    let deadline = Instant::now() + scaled(budget);
    let exchange = tokio::time::timeout_at(deadline, identify_over(&mut stdin, stdout)).await;
    // The far side is done being asked; letting it see EOF is what lets
    // it exit on its own rather than be killed.
    drop(stdin);

    let exchanged = match exchange {
        Ok(inner) => inner,
        Err(_elapsed) => Err(anyhow!("timed out after {}s", budget.as_secs().max(1))),
    };
    let identity = match exchanged {
        Ok(identity) => identity,
        Err(error) => {
            reap_by(&mut child, Instant::now()).await;
            let tail = tail.await.unwrap_or_default();
            let code = child
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code());
            // A child that exited cleanly and said nothing on stderr did
            // not fail as a *transport* — the fault is in what came back
            // over the pipe, and reporting it as a connection failure
            // would send the reader hunting the network.
            return Err(if code == Some(0) && tail.trim().is_empty() {
                SshTunnelError::Local(error.context(format!("verifying {}", target.raw)))
            } else {
                SshTunnelError::ssh(&target.raw, classify_ssh_failure(code, &tail))
            });
        }
    };
    reap_by(&mut child, deadline).await;

    if identity.session_protocol != SESSION_PROTOCOL_VERSION {
        return Err(SshTunnelError::Local(anyhow!(
            "that session speaks protocol {}, this build speaks {SESSION_PROTOCOL_VERSION}",
            identity.session_protocol
        )));
    }
    Ok(identity)
}

/// One `session.identify` request/response over a child's stdio, in the
/// same envelope [`crate::IpcClient`] would have written.
async fn identify_over<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    stdin: &mut W,
    stdout: R,
) -> Result<SessionIdentify> {
    const ID: i64 = 1;

    let request = RawRequest {
        id: ID,
        op: ops::SESSION_IDENTIFY.to_string(),
        params: serde_json::json!({}),
    };
    write_frame(stdin, &serde_json::to_vec(&request)?).await?;

    let mut reader = FrameReader::new(stdout);
    loop {
        let Some(frame) = reader.read_line().await? else {
            return Err(anyhow!(
                "the remote bridge closed without answering {}",
                ops::SESSION_IDENTIFY
            ));
        };
        let value: serde_json::Value = serde_json::from_slice(&frame)?;
        // A session may push events at a connection it never subscribed;
        // they are not this request's answer.
        if value.get("event").is_some() {
            continue;
        }
        let response: Response = serde_json::from_value(value)?;
        if response.id != ID {
            return Err(anyhow!("answer carried id {}, not {ID}", response.id));
        }
        if !response.ok {
            let error = response.error.unwrap_or(crate::messages::ResponseError {
                code: "internal".into(),
                message: "ok=false with no error body".into(),
            });
            return Err(anyhow!(
                "{}: {} ({})",
                ops::SESSION_IDENTIFY,
                error.message,
                error.code
            ));
        }
        return Ok(serde_json::from_value(
            response.result.unwrap_or(serde_json::Value::Null),
        )?);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // classify
    // ------------------------------------------------------------------

    #[test]
    fn localhost_is_the_local_session() {
        let resolved = classify(LOCALHOST_TARGET).expect("classify localhost");
        assert!(resolved.is_localhost());
        match resolved {
            ResolvedTransport::LocalSession(path) => assert_eq!(
                path,
                BundleProfile::session()
                    .expect("session profile")
                    .socket_path
            ),
            other => panic!("expected LocalSession, got {other:?}"),
        }
    }

    #[test]
    fn absolute_and_dotted_paths_are_unix_sockets() {
        for target in [
            "/tmp/roost.sock",
            "~/roost.sock",
            "./roost.sock",
            "sub/dir/roost.sock",
        ] {
            let resolved = classify(target).expect("classify path");
            assert!(!resolved.is_localhost());
            match resolved {
                ResolvedTransport::UnixSocket(path) => assert_eq!(path, PathBuf::from(target)),
                other => panic!("{target:?}: expected UnixSocket, got {other:?}"),
            }
        }
    }

    /// The documented behavior change from the pre-HS-3 reading: a bare
    /// filename with no path separator used to be treated as a
    /// same-directory socket path. It now reads as an ssh host, because
    /// there is no way to tell it apart from a hostname without one.
    #[test]
    fn a_bare_filename_with_no_slash_is_now_an_ssh_target_not_a_path() {
        let resolved = classify("foo.sock").expect("classify bare filename");
        assert!(!resolved.is_localhost());
        match resolved {
            ResolvedTransport::Ssh(target) => assert_eq!(target.raw, "foo.sock"),
            other => panic!("expected Ssh, got {other:?}"),
        }
    }

    #[test]
    fn user_at_host_and_bare_hostnames_are_ssh_targets() {
        for target in ["user@host", "workbox"] {
            let resolved = classify(target).expect("classify ssh target");
            assert!(!resolved.is_localhost());
            match resolved {
                ResolvedTransport::Ssh(t) => assert_eq!(t.raw, target),
                other => panic!("{target:?}: expected Ssh, got {other:?}"),
            }
        }
    }

    #[test]
    fn ssh_scheme_is_case_insensitive_but_casing_is_preserved() {
        for target in ["ssh://UPPER", "SSH://x", "SsH://Mixed-Case"] {
            let resolved = classify(target).expect("classify ssh scheme");
            match resolved {
                ResolvedTransport::Ssh(t) => assert_eq!(t.raw, target, "casing must be preserved"),
                other => panic!("{target:?}: expected Ssh, got {other:?}"),
            }
        }
    }

    /// The cheap boolean and the full classification must never
    /// disagree — the sidebar reads one and the connect path reads the
    /// other, about the same saved host.
    #[test]
    fn the_cheap_localhost_answer_matches_the_classifier() {
        assert!(target_is_localhost(LOCALHOST_TARGET));
        assert!(target_is_localhost("  localhost \n"), "trimmed like rule 2");
        for target in [
            "ssh://localhost",
            "user@localhost",
            "Localhost",
            "localhost.sock",
            "/tmp/localhost",
            "",
        ] {
            assert!(
                !target_is_localhost(target),
                "{target:?} is not the sentinel"
            );
            assert!(
                !classify(target).map(|t| t.is_localhost()).unwrap_or(false),
                "{target:?} must not classify as the local session either"
            );
        }
    }

    #[test]
    fn ssh_scheme_localhost_is_not_the_local_session() {
        for target in ["ssh://localhost", "user@localhost"] {
            let resolved = classify(target).expect("classify");
            assert!(
                !resolved.is_localhost(),
                "{target:?} must not be read as the localhost sentinel"
            );
            assert!(matches!(resolved, ResolvedTransport::Ssh(_)), "{target:?}");
        }
    }

    #[test]
    fn host_colon_port_without_a_slash_is_rejected_and_names_the_scheme() {
        for target in ["host:22", "::1", "fe80::1"] {
            let error = classify(target).expect_err(&format!("{target:?} must be rejected"));
            let message = error.to_string();
            assert!(
                message.contains("ssh://host:port") && message.contains("ssh://[::1]"),
                "{message} must name both the ssh://host:port spelling and the \
                 bracketed IPv6 form"
            );
        }
    }

    #[test]
    fn a_bare_scheme_with_no_host_is_rejected() {
        for target in ["ssh://", "SSH://", "  ssh://  "] {
            classify(target).expect_err(&format!("{target:?} has no host"));
        }
    }

    #[test]
    fn a_leading_dash_is_rejected_as_an_argv_injection_guard() {
        classify("-oProxyCommand=x").expect_err("a leading '-' must be rejected");
    }

    #[test]
    fn empty_and_whitespace_only_targets_are_rejected() {
        classify("").expect_err("empty target must be rejected");
        classify("   ").expect_err("whitespace-only target must be rejected");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_before_classification() {
        let resolved = classify("  workbox  ").expect("classify with padding");
        match resolved {
            ResolvedTransport::Ssh(t) => assert_eq!(t.raw, "workbox"),
            other => panic!("expected Ssh, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // token sanitize + hash
    // ------------------------------------------------------------------

    #[test]
    fn tokens_sanitize_disallowed_characters() {
        let target = SshTarget::new("user@host:2222/weird chars!");
        // Every character in the token (the sanitized prefix, the
        // separator, and the hex hash) is in the allowed set — the
        // token is itself a legal path component.
        assert!(
            target
                .token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "{:?} contains an unsanitized character",
            target.token
        );
        assert!(
            target.token.starts_with("user-host-2222-weird-chars-"),
            "{:?}",
            target.token
        );
    }

    #[test]
    fn tokens_truncate_the_sanitized_prefix_to_32_characters() {
        let raw = "a".repeat(50);
        let target = SshTarget::new(&raw);
        let (prefix, hash) = target
            .token
            .rsplit_once('-')
            .expect("token has a hash suffix");
        assert_eq!(prefix.len(), 32, "{prefix:?}");
        assert_eq!(hash.len(), 16, "{hash:?}");
        assert!(hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    /// The reason the hash covers the *full* raw string rather than the
    /// truncated prefix: two targets that sanitize to the same short
    /// prefix must not collide on disk.
    #[test]
    fn distinct_targets_that_sanitize_to_the_same_prefix_get_distinct_tokens() {
        let a = SshTarget::new("user@host");
        let b = SshTarget::new("user-host");
        assert_eq!(
            a.token.rsplit_once('-').unwrap().0,
            b.token.rsplit_once('-').unwrap().0,
            "precondition: both sanitize to the same prefix"
        );
        assert_ne!(
            a.token, b.token,
            "the two targets must not collide on a token"
        );
    }

    #[test]
    fn tokens_beyond_32_truncated_characters_still_avoid_collisions() {
        let a = SshTarget::new(&format!("{}suffix-one", "x".repeat(32)));
        let b = SshTarget::new(&format!("{}suffix-two", "x".repeat(32)));
        assert_eq!(
            a.token.rsplit_once('-').unwrap().0,
            b.token.rsplit_once('-').unwrap().0,
            "precondition: both truncate to the same 32-character prefix"
        );
        assert_ne!(a.token, b.token);
    }

    // ------------------------------------------------------------------
    // generated ssh config
    // ------------------------------------------------------------------

    #[test]
    fn config_includes_both_files_when_both_exist() {
        let user = PathBuf::from("/home/charlie/.ssh/config");
        let system = PathBuf::from("/etc/ssh/ssh_config");
        let config = generate_ssh_config(Some(&user), Some(&system));
        assert_eq!(
            config,
            "Include \"/home/charlie/.ssh/config\"\n\
             Include \"/etc/ssh/ssh_config\"\n\
             Host *\n\
             \x20 ServerAliveInterval 15\n\
             \x20 ServerAliveCountMax 4\n\
             \x20 ConnectTimeout 15\n"
        );
    }

    #[test]
    fn config_includes_only_the_user_file_when_system_is_absent() {
        let user = PathBuf::from("/home/charlie/.ssh/config");
        let config = generate_ssh_config(Some(&user), None);
        assert_eq!(
            config,
            "Include \"/home/charlie/.ssh/config\"\n\
             Host *\n\
             \x20 ServerAliveInterval 15\n\
             \x20 ServerAliveCountMax 4\n\
             \x20 ConnectTimeout 15\n"
        );
    }

    #[test]
    fn config_has_no_includes_when_neither_file_exists() {
        let config = generate_ssh_config(None, None);
        assert_eq!(
            config,
            "Host *\n\
             \x20 ServerAliveInterval 15\n\
             \x20 ServerAliveCountMax 4\n\
             \x20 ConnectTimeout 15\n"
        );
        assert!(!config.contains("ControlMaster"));
        assert!(!config.contains("ControlPath"));
        assert!(!config.contains("ControlPersist"));
        assert!(!config.contains("BatchMode"));
    }

    // ------------------------------------------------------------------
    // argv builders
    // ------------------------------------------------------------------

    #[test]
    fn exec_argv_is_the_exact_vector() {
        let cfg = PathBuf::from("/home/charlie/.config/roost/ssh_config");
        let ctl = PathBuf::from("/tmp/roost-ssh-workbox/ctl");
        let argv = exec_argv(&cfg, &ctl, "workbox");
        assert_eq!(
            argv,
            vec![
                "-F",
                "/home/charlie/.config/roost/ssh_config",
                "-S",
                "/tmp/roost-ssh-workbox/ctl",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPersist=60s",
                "-o",
                "RequestTTY=no",
                "-o",
                "BatchMode=yes",
                "-T",
                "workbox",
                &remote_command(),
            ]
        );
    }

    #[test]
    fn establish_argv_is_the_exact_vector() {
        let cfg = PathBuf::from("/home/charlie/.config/roost/ssh_config");
        let ctl = PathBuf::from("/tmp/roost-ssh-workbox/ctl");
        let argv = establish_argv(&cfg, &ctl, "workbox");
        assert_eq!(
            argv,
            vec![
                "-F",
                "/home/charlie/.config/roost/ssh_config",
                "-S",
                "/tmp/roost-ssh-workbox/ctl",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPersist=60s",
                "-o",
                "RequestTTY=no",
                "-o",
                "BatchMode=yes",
                "-T",
                "workbox",
                "true",
            ]
        );
    }

    #[test]
    fn teardown_argv_is_the_exact_vector() {
        let cfg = PathBuf::from("/home/charlie/.config/roost/ssh_config");
        let ctl = PathBuf::from("/tmp/roost-ssh-workbox/ctl");
        let argv = teardown_argv(&cfg, &ctl, "workbox");
        assert_eq!(
            argv,
            vec![
                "-F",
                "/home/charlie/.config/roost/ssh_config",
                "-S",
                "/tmp/roost-ssh-workbox/ctl",
                "-O",
                "exit",
                "-o",
                "BatchMode=yes",
                "workbox",
            ]
        );
        assert!(!argv.contains(&"-T".to_string()));
    }

    #[test]
    fn verify_argv_is_the_exact_vector() {
        let cfg = PathBuf::from("/home/charlie/.config/roost/ssh_config");
        let argv = verify_argv(&cfg, "workbox", &remote_command());
        assert_eq!(
            argv,
            vec![
                "-F",
                "/home/charlie/.config/roost/ssh_config",
                "-o",
                "ControlMaster=no",
                "-o",
                "RequestTTY=no",
                "-o",
                "BatchMode=yes",
                "-T",
                "workbox",
                &remote_command(),
            ]
        );
        assert!(!argv.contains(&"-S".to_string()));
        assert!(!argv.iter().any(|a| a.contains("ControlPersist")));
    }

    #[test]
    fn remote_command_is_the_pinned_one_liner() {
        assert_eq!(
            remote_command(),
            r#"sh -c 'R="$HOME/.local/bin/roost-session"; [ -x "$R" ] || R=roost-session; exec "$R" client-bridge'"#
        );
    }

    // ------------------------------------------------------------------
    // sun_path probe
    // ------------------------------------------------------------------

    #[test]
    fn pick_socket_dir_prefers_the_first_candidate_that_fits() {
        let short = PathBuf::from("/tmp");
        let name = "roost-ssh-workbox-aaaaaaaaaaaaaaaa-4242-0";
        let dir =
            pick_socket_dir(std::slice::from_ref(&short), name).expect("a short TMPDIR must fit");
        assert_eq!(dir, short.join(name));
    }

    /// The live-run regression: a directory where `ctl` fits but
    /// OpenSSH's temporary `ctl.XXXXXXXXXXXXXXXX` bind name does not
    /// must be rejected — measuring the on-disk name passed a macOS
    /// `$TMPDIR` whose real ControlMaster bind then failed inside ssh.
    #[test]
    fn pick_socket_dir_measures_the_ssh_bind_name_not_the_final_one() {
        let name = "roost-ssh-workbox-aaaaaaaaaaaaaaaa-4242-0";
        // Sized so `<dir>/<name>/ctl` fits SUN_PATH_MAX but
        // `<dir>/<name>/ctl.XXXXXXXXXXXXXXXX` does not.
        let parent_len = SUN_PATH_MAX - name.len() - "/ctl".len() - 8;
        let parent = PathBuf::from(format!("/{}", "p".repeat(parent_len - 1)));
        assert!(parent.join(name).join("ctl").as_os_str().len() <= SUN_PATH_MAX);
        assert!(parent.join(name).join(CTL_BIND_PROBE).as_os_str().len() > SUN_PATH_MAX);
        pick_socket_dir(std::slice::from_ref(&parent), name)
            .expect_err("a dir only the renamed ctl fits in is not usable");
    }

    #[test]
    fn pick_socket_dir_falls_back_past_a_candidate_that_does_not_fit() {
        // A macOS-style deep per-user TMPDIR, long enough that
        // `<dir>/<name>/bridge.sock` overruns `SUN_PATH_MAX`.
        let long_tmpdir = PathBuf::from(
            "/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/T/deeply/nested/tmp/dir/that/is/long",
        );
        let short = PathBuf::from("/tmp");
        let name = "roost-ssh-workbox-aaaaaaaaaaaaaaaa-4242-0";
        assert!(
            long_tmpdir.join(name).join("bridge.sock").as_os_str().len() > SUN_PATH_MAX,
            "precondition: the long TMPDIR candidate must not fit"
        );
        let dir = pick_socket_dir(&[long_tmpdir, short.clone()], name)
            .expect("the /tmp fallback must fit");
        assert_eq!(dir, short.join(name));
    }

    #[test]
    fn pick_socket_dir_errors_when_nothing_fits() {
        let name = scratch_dir_name(&("a".repeat(32) + "-0123456789abcdef"));
        let huge = PathBuf::from("/".to_string() + &"x".repeat(200));
        let error = pick_socket_dir(&[huge], &name).expect_err("nothing should fit");
        assert!(error.to_string().contains("103"));
    }

    /// The whole length check is only honest if what it measures is what
    /// gets bound: the pid and sequence are part of the name, so they
    /// have to be part of what was sized.
    #[test]
    fn pick_socket_dir_sizes_the_name_the_tunnel_actually_claims() {
        let name = scratch_dir_name("aabbccdd");
        let dir = pick_socket_dir(&[PathBuf::from("/tmp")], &name).expect("fits");
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some(name.as_str())
        );
    }

    // ------------------------------------------------------------------
    // scratch directory names
    // ------------------------------------------------------------------

    #[test]
    fn a_scratch_name_carries_this_process_and_a_fresh_sequence() {
        let first = scratch_dir_name("aabbccdd");
        let second = scratch_dir_name("aabbccdd");
        assert_ne!(first, second, "two attempts never share a directory");

        let (host, pid, first_seq) =
            parse_scratch_dir_name(&first).expect("a minted name must parse");
        assert_eq!(host, "aabbccdd");
        assert_eq!(pid, std::process::id());
        let (_, _, second_seq) = parse_scratch_dir_name(&second).expect("parse");
        assert!(second_seq > first_seq, "{first_seq} -> {second_seq}");
    }

    /// Parsed from the right, so a host id with a `-` in it survives the
    /// round trip rather than shifting the pid and sequence fields.
    #[test]
    fn a_scratch_name_round_trips_a_host_id_containing_dashes() {
        let name = scratch_dir_name("host-with-dashes");
        let (host, _, _) = parse_scratch_dir_name(&name).expect("parse");
        assert_eq!(host, "host-with-dashes");
    }

    #[test]
    fn names_that_are_not_a_hosts_scratch_directory_do_not_parse() {
        for name in [
            "roost-ssh-aabbccdd",              // the old deterministic shape
            "roost-ssh-aabbccdd-4242",         // no sequence
            "roost-ssh--4242-0",               // no host id
            "roost-ssh-aabbccdd-notapid-0",    // a pid that is not a number
            "roost-ssh-aabbccdd-4242-notaseq", // a sequence that is not a number
            "roost-session-aabbccdd-4242-0",   // not our prefix
            &verify_dir_name(),                // a verify's own directory
        ] {
            assert!(
                parse_scratch_dir_name(name).is_none(),
                "{name:?} must not read as a host's scratch directory"
            );
        }
    }

    // ------------------------------------------------------------------
    // failure classification
    // ------------------------------------------------------------------

    #[test]
    fn classification_table_first_match_wins() {
        let cases: &[(Option<i32>, &str, SshFailure)] = &[
            (
                Some(255),
                "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                 REMOTE HOST IDENTIFICATION HAS CHANGED!\n\
                 Host key verification failed.\n",
                SshFailure::ChangedHostKey,
            ),
            (
                Some(255),
                "Host key verification failed.\n",
                SshFailure::HostKeyUnknown,
            ),
            (
                Some(255),
                "user@workbox: Permission denied (publickey).\n",
                SshFailure::Auth,
            ),
            (
                Some(1),
                "client-bridge: no session\n",
                SshFailure::NoSession,
            ),
            (
                Some(127),
                "bash: roost-session: command not found\n",
                SshFailure::NotFound,
            ),
            (Some(127), "", SshFailure::NotFound),
            (
                Some(255),
                "ssh: connect to host workbox port 22: Connection refused\n",
                SshFailure::Transport(Some(
                    "ssh: connect to host workbox port 22: Connection refused".to_string(),
                )),
            ),
            (None, "", SshFailure::Transport(None)),
        ];
        for (exit_code, stderr_tail, expected) in cases {
            assert_eq!(
                classify_ssh_failure(*exit_code, stderr_tail),
                *expected,
                "exit_code={exit_code:?} stderr={stderr_tail:?}"
            );
        }
    }

    /// A changed-key blob also contains the substring "Host key
    /// verification failed" further down — checking that rule first
    /// would misreport the wary case as the ordinary unknown-key one.
    #[test]
    fn a_changed_key_blob_wins_over_the_unknown_key_substring_it_also_contains() {
        let stderr = "REMOTE HOST IDENTIFICATION HAS CHANGED!\n\
                       Someone could be eavesdropping on you right now.\n\
                       Host key verification failed.\n";
        assert_eq!(
            classify_ssh_failure(Some(255), stderr),
            SshFailure::ChangedHostKey
        );
    }

    #[test]
    fn exit_127_with_empty_stderr_is_not_found() {
        assert_eq!(classify_ssh_failure(Some(127), ""), SshFailure::NotFound);
    }

    #[test]
    fn failure_messages_interpolate_the_target_and_never_suggest_accepting_a_changed_key() {
        let message = SshFailure::ChangedHostKey.message("workbox");
        assert!(message.contains("workbox"));
        assert!(message.to_lowercase().contains("changed"));
        assert!(message.to_lowercase().contains("out-of-band"), "{message}");
        // The guard: it warns the user off accepting, never invites it.
        assert!(
            message.to_lowercase().contains("do not accept"),
            "{message}"
        );

        assert!(SshFailure::HostKeyUnknown
            .message("workbox")
            .contains("ssh workbox"));
        assert!(SshFailure::Auth.message("workbox").contains("ssh workbox"));
        assert!(SshFailure::NoSession
            .message("workbox")
            .contains("roostctl session start"));
        assert!(SshFailure::NotFound.message("workbox").contains("workbox"));
    }
}
