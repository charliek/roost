//! Pure classification, config, and argv shaping for the SSH transport
//! (host-sessions HS-3, plan 038).
//!
//! Nothing in this module spawns a process, writes a file, or binds a
//! socket — that is C3's job (the tunnel runtime) and, downstream of
//! it, C4's (wiring the UI/`roostctl` onto it). What lives here is the
//! part that has no side effects and therefore has no excuse not to be
//! unit-tested exhaustively: deciding what kind of target a string
//! names, building the generated `ssh_config` bytes, building the argv
//! for each of the four `ssh` invocations the tunnel needs, sizing the
//! control-socket directory so `sun_path` fits, and turning a failed
//! connection's exit code + stderr into copy a user can act on.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::paths::BundleProfile;

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

/// The two file names created under a host's control-socket directory.
/// `ctl` is the `-S` mux control socket; `bridge.sock` is the local
/// bridge's own listening socket. Both are `AF_UNIX` paths and both
/// must fit `sun_path`, so the probe below checks the longer of the
/// two against every candidate.
const SOCKET_FILE_NAMES: [&str; 2] = ["ctl", "bridge.sock"];

/// The historical BSD/Linux/macOS `sockaddr_un.sun_path` size: 104
/// bytes total, one of which is the mandatory NUL terminator, leaving
/// 103 usable path bytes.
pub const SUN_PATH_MAX: usize = 103;

/// Find the first of `candidate_dirs` under which
/// `<dir>/roost-ssh-<host_token>/<file>` fits [`SUN_PATH_MAX`] for
/// every name in [`SOCKET_FILE_NAMES`] — checking the longer file name
/// is sufficient since both live at the same depth.
///
/// Directory creation is C3's job; this only picks *where*. Candidates
/// are tried in order (typically `$TMPDIR` then `/tmp`) because a
/// shorter fallback is only worth using when the preferred one does
/// not fit — `/tmp` is shared and less private than a per-user
/// `$TMPDIR`.
pub fn pick_socket_dir(candidate_dirs: &[PathBuf], host_token: &str) -> Result<PathBuf> {
    for dir in candidate_dirs {
        let base = dir.join(format!("roost-ssh-{host_token}"));
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
         under roost-ssh-{host_token}; tried: {}",
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
        let dir = pick_socket_dir(std::slice::from_ref(&short), "workbox-aaaaaaaaaaaaaaaa")
            .expect("a short TMPDIR must fit");
        assert_eq!(dir, short.join("roost-ssh-workbox-aaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn pick_socket_dir_falls_back_past_a_candidate_that_does_not_fit() {
        // A macOS-style deep per-user TMPDIR, long enough that
        // `<dir>/roost-ssh-<token>/bridge.sock` overruns `SUN_PATH_MAX`.
        let long_tmpdir = PathBuf::from(
            "/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/T/deeply/nested/tmp/dir/that/is/long",
        );
        let short = PathBuf::from("/tmp");
        let token = "workbox-aaaaaaaaaaaaaaaa";
        assert!(
            long_tmpdir
                .join(format!("roost-ssh-{token}"))
                .join("bridge.sock")
                .as_os_str()
                .len()
                > SUN_PATH_MAX,
            "precondition: the long TMPDIR candidate must not fit"
        );
        let dir = pick_socket_dir(&[long_tmpdir, short.clone()], token)
            .expect("the /tmp fallback must fit");
        assert_eq!(dir, short.join(format!("roost-ssh-{token}")));
    }

    #[test]
    fn pick_socket_dir_errors_when_nothing_fits() {
        let token = "a".repeat(32) + "-0123456789abcdef";
        let huge = PathBuf::from("/".to_string() + &"x".repeat(200));
        let error = pick_socket_dir(&[huge], &token).expect_err("nothing should fit");
        assert!(error.to_string().contains("103"));
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
