//! The contract between whoever *launches* a host session and the
//! session itself: one env hint on the way in, one verdict line on the
//! way out.
//!
//! It lives here rather than in `roost-session` because both ends need
//! it and only one end can afford the other's dependencies. `roostctl`
//! spawns `roost-session` and parses what it says; depending on the
//! session crate to learn the format would drag the whole engine —
//! workspace, PTY supervisor, `portable-pty` — into a shell-integration
//! binary. `roost-ipc` is the crate both already depend on, and the
//! verdict line is wire format like everything else in here.
//!
//! # The verdict
//!
//! A start attempt has three outcomes and a caller needs no more than
//! that: the session is up (and which pid it is), somebody else was
//! already up, or the start failed and why. That is one line of ASCII,
//! written once, on a channel that closes immediately afterwards so the
//! reader is never left guessing whether more is coming.
//!
//! Two transports, one format. Daemonized, the line goes down the
//! readiness pipe to the forking parent, which relays it to **stdout**
//! and exits with the matching status. Under `--foreground` the session
//! writes it to stdout itself. Either way stdout carries this line and
//! nothing else — the log lives in a file and the console tee is on
//! stderr — so a caller can read stdout without a parser.
//!
//! # The spawn ladder
//!
//! Everything below the verdict format — finding the binary, spawning
//! it, reading its one line, and confirming a session actually answers
//! — is the *ladder* `roostctl session start` climbs. It lives here
//! because HS-2's client climbs the identical ladder: an explicit
//! Connect on a localhost host spawns a missing session exactly the way
//! the CLI does, and two implementations of "which `roost-session` is
//! this user's" is one more than the contract can survive.
//!
//! # The stop ladder
//!
//! Its mirror image lives here for the same reason: `roostctl session
//! stop` and the UI's upgrade/restart flow (plan 037 §3.7 — stop, wait
//! for the socket to go, spawn, reconnect) both need "ask the session to
//! stop, then wait until it really left", and the waiting half has
//! fail-safe rules ([`stop_completed`]) that must not be written twice.
//!
//! The budgets stay with the caller: every entry point takes its
//! `Duration` rather than reading a constant of its own, so a CLI
//! invocation and a UI's background connect can want different
//! patience. The two *defaults* live here anyway
//! ([`DEFAULT_VERDICT_BUDGET`], [`DEFAULT_CONFIRM_BUDGET`]) because
//! they are not a caller's preference — they are read off the daemon's
//! own waits, and a client that picks a smaller number reports a
//! timeout for a start that was still going to answer.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::messages::{
    ops, SessionIdentify, SessionIdentifyParams, SessionStopParams, SessionStopResult,
};
use crate::socket_state::{self, SocketState};
use crate::IpcClient;

/// Consumed-once hint naming the directory the user ran `roostctl
/// session start` from.
///
/// The daemon `chdir("/")`s before it does anything else, so the launch
/// cwd cannot be recovered later — and it is what seeds the first
/// project on a fresh state file. It travels as an env var because the
/// fork happens before any IPC exists, and it is removed from the
/// environment the instant it is read so no PTY can inherit it.
pub const LAUNCH_CWD_ENV: &str = "ROOST_SESSION_LAUNCH_CWD";

/// Cap on a verdict line, newline excluded. A verdict is tens of bytes;
/// anything past this is a session writing garbage down the pipe.
///
/// Both ends are held to it: a reader stops here rather than buffering
/// without bound, and [`Verdict`]'s `Display` truncates so the **whole**
/// line — prefix included — fits. If only the reason were capped, the
/// formatter could emit a line the reader is required to reject.
pub const MAX_VERDICT_BYTES: usize = 8 * 1024;

/// Narrowest and widest [`timeout_scale`] accepted.
///
/// Both ends exist to keep a budget a budget. Above the ceiling
/// `Duration::mul_f64` overflows and **panics** (`1e300` on a 30s budget
/// is not a representable `Duration`), so an env var could crash the
/// process it was meant to slow down. Below the floor a scaled budget
/// rounds toward zero and every timeout fires instantly, which disarms
/// them just as thoroughly as a zero would. The range still spans
/// 100x faster to 1000x slower than shipped — far past any real runner.
const MIN_TIMEOUT_SCALE: f64 = 0.01;
const MAX_TIMEOUT_SCALE: f64 = 1000.0;

/// Multiplier for every budget that waits on the other end of this
/// contract — the daemon's own waits and `roostctl session`'s waits on
/// it alike. The same `ROOST_TEST_TIMEOUT_SCALE` the Python harness
/// reads, so a loaded CI runner widens every side together.
pub fn timeout_scale() -> f64 {
    parse_timeout_scale(std::env::var("ROOST_TEST_TIMEOUT_SCALE").ok().as_deref())
}

/// [`timeout_scale`]'s rule, over an already-read value.
///
/// Anything unparseable, non-finite, or outside
/// `MIN_TIMEOUT_SCALE..=MAX_TIMEOUT_SCALE` falls back to 1.0 rather than
/// being clamped: a value that far out is a typo or an attack, and the
/// shipped budget is the safe reading of both. Pure, so both crates can
/// pin the policy without mutating process-global env.
pub fn parse_timeout_scale(raw: Option<&str>) -> f64 {
    raw.and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|factor| {
            factor.is_finite() && (MIN_TIMEOUT_SCALE..=MAX_TIMEOUT_SCALE).contains(factor)
        })
        .unwrap_or(1.0)
}

/// What a start attempt turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Serving. The pid is the session process's — the daemonized
    /// child's, not the forking parent's.
    Ready(u32),
    /// Another session already owns this profile's socket. The pid is
    /// the winner's, read from the lock file it holds; `None` when that
    /// file could not be read, which is a diagnostic loss and not a
    /// different outcome — so the word stays and only the suffix goes.
    AlreadyRunning(Option<i32>),
    /// The start failed. The reason is a one-line rendering of the
    /// error chain.
    Error(String),
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(pid) => write!(f, "ready pid={pid}"),
            Self::AlreadyRunning(Some(pid)) => write!(f, "already-running pid={pid}"),
            Self::AlreadyRunning(None) => f.write_str("already-running"),
            // Newlines would turn one verdict into two frames, and the
            // reader stops at the first. The reason's budget is what is
            // left of the line's after the prefix, so the emitted line
            // is one the reader will accept.
            Self::Error(reason) => write!(
                f,
                "{ERROR_PREFIX}{}",
                one_line(reason, MAX_VERDICT_BYTES - ERROR_PREFIX.len())
            ),
        }
    }
}

impl Verdict {
    /// The exit status a *parent* relaying this verdict should take.
    /// Losing a race is a successful no-op, not a failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Ready(_) | Self::AlreadyRunning(_) => 0,
            Self::Error(_) => 1,
        }
    }

    /// Parse a verdict line back. The forking parent needs this to
    /// decide its own exit status and `roostctl` needs it to decide
    /// whether to confirm; an unrecognized line is reported as an error
    /// rather than guessed at.
    pub fn parse(line: &str) -> Self {
        let line = line.trim();
        if let Some(pid) = line.strip_prefix("ready pid=") {
            if let Ok(pid) = pid.trim().parse() {
                return Self::Ready(pid);
            }
        }
        if line == "already-running" {
            return Self::AlreadyRunning(None);
        }
        if let Some(pid) = line.strip_prefix("already-running pid=") {
            return Self::AlreadyRunning(pid.trim().parse().ok());
        }
        if let Some(reason) = line.strip_prefix("error: ") {
            return Self::Error(reason.to_string());
        }
        Self::Error(format!("unrecognized readiness verdict: {line:?}"))
    }
}

/// The one prefix that costs a verdict line part of its byte budget.
const ERROR_PREFIX: &str = "error: ";

/// Collapse an error chain to one line of at most `budget` bytes: the
/// frame format is newline-delimited, so an embedded newline would
/// truncate the verdict at the reader.
fn one_line(reason: &str, budget: usize) -> String {
    let flattened = reason
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if flattened.len() > budget {
        // Truncate on a char boundary — the budget is in bytes and the
        // reason may be UTF-8.
        let mut cut = budget;
        while cut > 0 && !flattened.is_char_boundary(cut) {
            cut -= 1;
        }
        return flattened[..cut].to_string();
    }
    flattened
}

// ============================================================================
// Resolving a saved host's target
// ============================================================================

/// The `target` spelling that means "this machine's own session".
///
/// Re-exported from [`crate::ssh`], where [`crate::ssh::classify`]
/// documents the full rule table this sentinel is rule 2 of — that
/// module is the pure classification layer `resolve_target` is now
/// built on, so the constant's canonical home moved with it. Kept as a
/// re-export here so every existing `session_launch::LOCALHOST_TARGET`
/// import keeps compiling unchanged.
pub use crate::ssh::LOCALHOST_TARGET;

/// Resolve a saved host's `target` to a socket path, and say whether it
/// is this machine's own session.
///
/// The localhost/remote answer is load-bearing three times over: it is
/// the only host a client may spawn, the only one whose drops
/// auto-retry, and the only one the macOS platform gate hides. Both the
/// UI (`host_conn`) and `roostctl host add --verify` resolve through
/// here so a target can never mean two different sockets depending on
/// which binary read it.
///
/// A thin front door onto [`crate::ssh::classify`], kept at this
/// `(PathBuf, bool)` shape so every existing caller compiles untouched.
/// It cannot keep that shape for an ssh target — there is no socket
/// path to hand back until C3's tunnel runtime dials one — so an ssh
/// target is a hard error here. [`crate::ssh::classify`] is the new
/// front door for a caller that needs to branch on all three kinds of
/// target; this one stays for the two kinds it can still answer.
pub fn resolve_target(target: &str) -> Result<(PathBuf, bool)> {
    match crate::ssh::classify(target)? {
        crate::ssh::ResolvedTransport::LocalSession(path) => Ok((path, true)),
        crate::ssh::ResolvedTransport::UnixSocket(path) => Ok((path, false)),
        crate::ssh::ResolvedTransport::Ssh(ssh_target) => Err(anyhow!(
            "{:?} resolves through the ssh tunnel, not a socket path — HS-3",
            ssh_target.raw
        )),
    }
}

/// Resolve a prospective host's target, dial it, and check it is a
/// session this build can talk to at all.
///
/// Coarse on purpose, and the same bar wherever it is offered: both
/// `roostctl host add --verify` and the Add Host dialog's
/// "Add & Connect" promise exactly "is something answering, and does it
/// speak this protocol". The exact-build half of the compatibility gate
/// belongs to the attach path, which turns a mismatch into an upgrade
/// prompt — refusing the *save* over it would leave the user with no way
/// to record the host at all.
///
/// One definition for the same reason [`resolve_target`] is one: this
/// bar moves when the attach path grows its upgrade flow, and a second
/// copy would be the one nobody updated.
pub async fn verify_target(target: &str, budget: Duration) -> Result<SessionIdentify> {
    let (socket, _localhost) = resolve_target(target)?;
    verify_socket(&socket, budget).await
}

/// [`verify_target`]'s second half, for a caller that has already
/// resolved the socket — [`crate::ssh::verify_transport`] classifies
/// once and dispatches on the answer, and re-deriving the path from the
/// raw string would be the second resolve this module exists to prevent.
pub async fn verify_socket(socket: &Path, budget: Duration) -> Result<SessionIdentify> {
    let identity = identify(socket, budget)
        .await
        .with_context(|| format!("{} did not answer", socket.display()))?;
    if identity.session_protocol != crate::messages::SESSION_PROTOCOL_VERSION {
        return Err(anyhow!(
            "that session speaks protocol {}, this build speaks {}",
            identity.session_protocol,
            crate::messages::SESSION_PROTOCOL_VERSION,
        ));
    }
    Ok(identity)
}

// ============================================================================
// Locating the daemon binary
// ============================================================================

/// The daemon binary's name, as installed next to `roostctl`
/// (`/usr/bin/roost-session` from the deb).
pub const BIN_NAME: &str = "roost-session";

/// Override naming the daemon binary outright. First rung of
/// [`locate_session_binary`]; the tests and a from-source `cargo run`
/// both need it.
pub const BIN_ENV: &str = "ROOST_SESSION_BIN";

/// Which rung of [`locate_session_binary`]'s ladder produced a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOrigin {
    /// `ROOST_SESSION_BIN`.
    Env,
    /// Next to the running `roostctl` — the packaged layout, where both
    /// binaries land in the same directory.
    Sibling,
    /// Found on `PATH`.
    Path,
}

impl BinOrigin {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Env => "$ROOST_SESSION_BIN",
            Self::Sibling => "next to roostctl",
            Self::Path => "$PATH",
        }
    }
}

/// Where the daemon binary was found, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedBin {
    pub path: PathBuf,
    pub origin: BinOrigin,
}

/// First hit *this user can actually run* wins: `ROOST_SESSION_BIN`,
/// then a sibling of the running `roostctl`, then `PATH`.
///
/// The inputs arrive as arguments rather than being read here so the
/// precedence is testable without mutating process-global env that
/// every other test in this binary also reads.
///
/// The two rungs that are guesses — sibling and `PATH` — fall through
/// anything unusable: a root-owned `0700` `roost-session` next to
/// `roostctl` is not this user's program, and stopping the search on it
/// would trade a working `PATH` hit for an `EACCES` at spawn.
///
/// The rung that is not a guess behaves the other way: an explicit
/// `ROOST_SESSION_BIN` that cannot be run is a hard error, because
/// silently starting a *different* binary than the one the user named
/// would look like success while pointing at the wrong session.
pub fn locate_session_binary(
    env_override: Option<&OsStr>,
    roostctl_exe: Option<&Path>,
    path_env: Option<&OsStr>,
) -> Result<LocatedBin> {
    let env_override = env_override.filter(|v| !v.is_empty());
    if let Some(raw) = env_override {
        let path = PathBuf::from(raw);
        if is_executable_file(&path) {
            return Ok(LocatedBin {
                path,
                origin: BinOrigin::Env,
            });
        }
        return Err(anyhow!(
            "{BIN_ENV}={} is not a file this user can execute",
            path.display()
        ));
    }

    let sibling = roostctl_exe
        .and_then(Path::parent)
        .map(|dir| dir.join(BIN_NAME));
    if let Some(path) = sibling.clone().filter(|p| is_executable_file(p)) {
        return Ok(LocatedBin {
            path,
            origin: BinOrigin::Sibling,
        });
    }

    let path_dirs: Vec<PathBuf> = path_env
        .map(|raw| std::env::split_paths(raw).collect())
        .unwrap_or_default();
    for dir in &path_dirs {
        // An empty PATH element means "the current directory" to the
        // shell; joining it here would produce a bare `roost-session`
        // that resolves against our cwd. Skip it — an implicit
        // cwd-relative daemon is not something to start.
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(BIN_NAME);
        if is_executable_file(&candidate) {
            return Ok(LocatedBin {
                path: candidate,
                origin: BinOrigin::Path,
            });
        }
    }

    Err(anyhow!(
        "cannot find the {BIN_NAME} binary. Tried:\n  \
         {}: unset\n  \
         {}: {}\n  \
         {}: {}",
        BinOrigin::Env.describe(),
        BinOrigin::Sibling.describe(),
        sibling
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unknown (cannot resolve this binary's path)".into()),
        BinOrigin::Path.describe(),
        if path_dirs.is_empty() {
            "unset".to_string()
        } else {
            path_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(":")
        }
    ))
}

/// Can *this* process exec this path?
///
/// `access(X_OK)` rather than a permission-bit read, because the bits
/// alone do not answer the question: a `0700` file owned by root has an
/// execute bit set and is still `EACCES` for everyone else, and the same
/// read gets ACLs and read-only mounts wrong. The kernel already knows;
/// ask it.
///
/// The `is_file` check stays in front of it because `X_OK` on a
/// *directory* means "traversable", which every `0755` directory is —
/// without it a directory named `roost-session` would satisfy the
/// search.
fn is_executable_file(path: &Path) -> bool {
    if !std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        // An interior NUL cannot name a real file.
        return false;
    };
    // SAFETY: a NUL-terminated pointer this call does not retain.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

// ============================================================================
// Spawning it and reading the verdict
// ============================================================================

/// How long to wait for a spawned `roost-session start` to print its
/// verdict line.
///
/// Must comfortably exceed the daemon's own `parent_ready_timeout()`
/// (30s), because that parent is who the caller is reading: expiring
/// first would report a timeout for a start that was still going to
/// answer. On expiry the child is killed — it is the *forking parent*,
/// so killing it never touches a session that did come up.
pub const DEFAULT_VERDICT_BUDGET: Duration = Duration::from_secs(45);

/// How long to poll `session.identify` before declaring that the
/// verdict lied. The winner of the socket-lock race has already bound
/// by the time it writes `ready`, so this only ever covers the
/// `already-running` loser overtaking the winner — microseconds in
/// practice.
pub const DEFAULT_CONFIRM_BUDGET: Duration = Duration::from_secs(10);

/// What came back on the launcher's stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictRead {
    /// A newline-terminated line within the cap. The newline is not
    /// included.
    Line(String),
    /// The stream ended before any newline. The daemon's own reader
    /// (`daemonize::read_verdict`) calls this no-verdict, and so does
    /// this one: an unterminated frame is not a verdict, however much
    /// it looks like one.
    Eof,
    /// The cap was reached with no newline in sight.
    TooLong,
    /// The read itself failed.
    Io(String),
}

/// Read one verdict line, refusing to buffer more than `cap` bytes of
/// it.
///
/// Generic over the reader so the four outcomes are unit-testable
/// without a subprocess. `take(cap + 1)` is what bounds it: the extra
/// byte is the newline a maximal legal line ends with, so hitting the
/// limit without one is unambiguously [`VerdictRead::TooLong`] rather
/// than a line that happened to end exactly at the cap.
pub async fn read_verdict_line<R: AsyncRead + Unpin>(reader: R, cap: usize) -> VerdictRead {
    let mut buf = Vec::new();
    if let Err(error) = BufReader::new(reader.take(cap as u64 + 1))
        .read_until(b'\n', &mut buf)
        .await
    {
        return VerdictRead::Io(error.to_string());
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        return VerdictRead::Line(String::from_utf8_lossy(&buf).into_owned());
    }
    // Unterminated. Reading `cap + 1` bytes means the limiter stopped
    // us, not the writer; anything shorter is a stream that ended.
    if buf.len() > cap {
        VerdictRead::TooLong
    } else {
        VerdictRead::Eof
    }
}

/// Spawn `<bin> start` with the launch-cwd hint and read the one line
/// it prints, everything bounded by `budget`.
///
/// The child here is the *launcher* — for a real `roost-session` it is
/// the forking parent, which exits the moment its daemonized child
/// reports. That is why every wait on it may be cut short and the
/// process killed: killing the launcher never touches a session that
/// did come up, and a launcher that will not exit must not be able to
/// hold its caller open past its budget.
pub async fn spawn_and_read_verdict(bin: &Path, cwd: &Path, budget: Duration) -> Result<Verdict> {
    let deadline = Instant::now() + budget;
    let mut child = Command::new(bin)
        .arg("start")
        .env(LAUNCH_CWD_ENV, cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Inherited: the session tees its startup log there, and a
        // failed start is far easier to read with it in front of you.
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no stdout pipe on the spawned {BIN_NAME}"))?;

    let read =
        tokio::time::timeout_at(deadline, read_verdict_line(stdout, MAX_VERDICT_BYTES)).await;

    // One reap for every path, bounded by what is left of the same
    // budget. A launcher still alive at the deadline is killed —
    // including on the timeout path, where nothing is left of it.
    let result = match read {
        Ok(VerdictRead::Line(line)) => Ok(Verdict::parse(&line)),
        Ok(VerdictRead::Eof) => Err(anyhow!(
            "{BIN_NAME} closed its output without a complete readiness line; \
             see the session log"
        )),
        Ok(VerdictRead::TooLong) => Err(anyhow!(
            "{BIN_NAME} wrote more than {MAX_VERDICT_BYTES} bytes with no newline; \
             that is not a readiness verdict"
        )),
        Ok(VerdictRead::Io(error)) => Err(anyhow!("reading {BIN_NAME}'s readiness line: {error}")),
        Err(_elapsed) => Err(anyhow!(
            "{BIN_NAME} did not report readiness within {}s",
            budget.as_secs().max(1)
        )),
    };
    reap_by(&mut child, deadline).await;
    result
}

/// Wait for the launcher to exit, but no later than `deadline`; kill and
/// reap it if it outlives that. Never returns a zombie and never
/// outlives the budget.
///
/// `pub(crate)` for [`crate::ssh`]'s tunnel runtime, which reaps its
/// `ssh` children under exactly this discipline — the shape of "bounded
/// wait, then SIGKILL" is not worth having two of.
pub(crate) async fn reap_by(child: &mut Child, deadline: Instant) {
    if tokio::time::timeout_at(deadline, child.wait())
        .await
        .is_err()
    {
        // `kill` is SIGKILL + reap, so the wait behind it is the one
        // the kernel is about to satisfy.
        let _ = child.kill().await;
    }
}

/// The floor [`drain_tail`] gives a drain whose deadline is already
/// spent — which, after a [`reap_by`] that ran out of budget, is every
/// time.
///
/// Not scaled: it is a local scheduler allowance for a pipe that has
/// already reached EOF, not a remote round trip.
const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(200);

/// A drained stderr tail, and whether any of it was lost on the way.
///
/// The flag is the honest half. A caller that reads its *verdict* out of
/// this text — which family of failure this was — needs to know when the
/// text may be missing the line that would have decided it, so that
/// "nothing matched" can be told apart from "we never saw it". Two
/// things set it: a drain that ran out of time here, and the reader's
/// own byte cap discarding leading bytes
/// ([`crate::ssh::spawn_stderr_tail`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Tail {
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

impl Tail {
    /// A tail nothing could be read out of.
    fn lost() -> Self {
        Self {
            text: String::new(),
            truncated: true,
        }
    }
}

/// Collect a stderr tail task, bounded by `deadline` — or by
/// [`STDERR_DRAIN_GRACE`] from now, when the deadline is already spent.
///
/// **Reaping a child does not close its stderr pipe.** The write end is
/// inherited by whatever the child left behind — a remote `sh -c` that
/// forked rather than exec'd its command, an `ssh` `ProxyCommand` helper
/// that outlives the client it was started for — so an unbounded
/// `tail.await` after the kill waits on *that* process instead of on our
/// own budget: a 1s wait against a far side that slept for 10 took the
/// whole 10, the deadline having stopped the exchange on time and the
/// drain then given the clamp straight back.
///
/// The abort on expiry is what releases our read end, rather than
/// leaving a detached reader holding it for the grandchild's life.
pub(crate) async fn drain_tail(mut tail: JoinHandle<Tail>, deadline: Instant) -> Tail {
    let deadline = deadline.max(Instant::now() + STDERR_DRAIN_GRACE);
    match tokio::time::timeout_at(deadline, &mut tail).await {
        Ok(Ok(tail)) => tail,
        // A reader that panicked or was cancelled took its buffer with
        // it: nothing arrived, which is the same fact an expiry reports.
        Ok(Err(_joined)) => Tail::lost(),
        Err(_elapsed) => {
            tail.abort();
            Tail::lost()
        }
    }
}

// ============================================================================
// Confirming a session actually answers
// ============================================================================

/// Interval for the confirmation poll. Short: these are local
/// `connect(2)` calls, and the thing being waited for usually lands
/// immediately.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A single IPC leg's budget. [`IpcClient`] has no timeout of its own
/// and "the session is wedged" must render as an error, not a hung
/// caller.
pub const IPC_TIMEOUT: Duration = Duration::from_secs(10);

/// [`IPC_TIMEOUT`] under the ambient scale — what one leg of a poll
/// round actually gets. The polls here spend several legs inside an
/// outer deadline, so they read it once per call rather than per round.
pub fn leg_budget() -> Duration {
    IPC_TIMEOUT.mul_f64(timeout_scale())
}

/// Dial a session socket, bounded.
pub async fn dial(socket: &Path, timeout: Duration) -> Result<IpcClient> {
    tokio::time::timeout(timeout, IpcClient::connect(socket))
        .await
        .map_err(|_| anyhow!("connecting to {} timed out", socket.display()))?
        .with_context(|| format!("connect to {}", socket.display()))
}

/// `session.identify` on an already-dialed client, bounded.
pub async fn identify_on(client: &mut IpcClient, timeout: Duration) -> Result<SessionIdentify> {
    tokio::time::timeout(
        timeout,
        client.call(ops::SESSION_IDENTIFY, SessionIdentifyParams {}),
    )
    .await
    .map_err(|_| anyhow!("{} timed out", ops::SESSION_IDENTIFY))?
    .map_err(Into::into)
}

/// Dial and `session.identify` in one step, each leg bounded by
/// `timeout`.
pub async fn identify(socket: &Path, timeout: Duration) -> Result<SessionIdentify> {
    let mut client = dial(socket, timeout).await?;
    identify_on(&mut client, timeout).await
}

/// Poll `session.identify` until a session answers or the budget runs
/// out. This is what turns a printed verdict into a fact.
///
/// The outer `timeout_at` is what makes `budget` a real bound. A single
/// round can block for a connect plus a call — two [`IPC_TIMEOUT`]s —
/// so a loop that only checked the deadline *between* rounds would
/// overrun the budget it advertises by more than double.
pub async fn confirm_serving(socket: &Path, budget: Duration) -> Result<SessionIdentify> {
    let deadline = Instant::now() + budget;
    // The caller scaled `budget`; the legs inside must widen with it or
    // a loaded runner spends the whole budget on one timed-out dial.
    let leg = leg_budget();
    let polling = async {
        loop {
            let error = match identify(socket, leg).await {
                Ok(identity) => return Ok(identity),
                Err(error) => error,
            };
            if Instant::now() + POLL_INTERVAL >= deadline {
                return Err(anyhow!(
                    "gave up after {}s: {error:#}",
                    budget.as_secs().max(1)
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    };
    tokio::time::timeout_at(deadline, polling)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow!(
                "gave up after {}s: no session answered",
                budget.as_secs().max(1)
            ))
        })
}

// ============================================================================
// Stopping a session, and waiting for it to go
// ============================================================================

/// How long a stop waits for the reap report. Generous: the reply does
/// not come back until every child is accounted for, which is the
/// engine's 5s soft deadline plus the post-SIGKILL tail.
pub const DEFAULT_STOP_CALL_BUDGET: Duration = Duration::from_secs(60);

/// How long a stop waits, after the reap report, for the socket to
/// actually go away. The session unlinks it in a detached finalizer that
/// runs *after* the reply, so "stopped" is only true once the poll below
/// says so.
pub const DEFAULT_STOP_GONE_BUDGET: Duration = Duration::from_secs(30);

/// The two socket states that prove nothing can be listening. Same pair
/// [`socket_state`] calls safe-to-unlink, read here as "not running" —
/// every other state, including `Indeterminate`, means assume-live.
fn socket_gone(state: &SocketState) -> bool {
    matches!(state, SocketState::Missing | SocketState::Stale)
}

/// Probe once and read off whether nothing is listening.
pub async fn probe_gone(socket: &Path) -> bool {
    socket_gone(&socket_state::probe(socket, socket_state::PROBE_TIMEOUT).await)
}

/// One round of the post-stop poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollObservation<'a> {
    /// The socket is gone or refuses connections — nothing is listening.
    Unreachable,
    /// A session answered `session.identify` with this id.
    Identified(&'a str),
    /// Something is at the path but it could not be asked (a full accept
    /// backlog, a permission error, a timeout).
    Indeterminate,
}

/// Has the session we asked to stop finished stopping?
///
/// A *different* session id counts as stopped: between our reap report
/// and this poll, something (a spawn-if-missing client, a supervisor)
/// may have started a fresh session on the same socket. Ours is gone,
/// which is what was asked — reporting a failure there would be a lie.
///
/// [`PollObservation::Indeterminate`] keeps waiting, mirroring
/// [`socket_state`]'s fail-safe rule: the one thing we must never do is
/// call a live session dead.
fn stop_completed(observation: PollObservation<'_>, stopping: &str) -> bool {
    match observation {
        PollObservation::Unreachable => true,
        PollObservation::Identified(id) => id != stopping,
        PollObservation::Indeterminate => false,
    }
}

/// A stop that reached a live session: who answered, and what it reaped.
///
/// The identity is not a courtesy — [`await_stopped`] needs it to tell
/// "our session left" from "a fresh one took the socket".
#[derive(Debug, Clone)]
pub struct StopReport {
    pub identity: SessionIdentify,
    pub reap: SessionStopResult,
}

/// Ask the session at `socket` to stop, and hand back its reap report.
///
/// `Ok(None)` means nothing was listening — a success in the
/// `systemctl stop` sense: the caller asked for a state and it already
/// holds. The session is identified *before* the stop so the caller can
/// wait for the right process to leave.
///
/// This and [`await_stopped`] are the two rungs of a stop, split so a
/// caller can report the reap before spending the socket-gone budget.
/// `roostctl session stop` and the UI's client-side restart composition
/// (plan 037 §3.7) climb the same pair.
pub async fn stop_session(socket: &Path, budget: Duration) -> Result<Option<StopReport>> {
    if probe_gone(socket).await {
        return Ok(None);
    }
    // `budget` is the *reap* budget and is deliberately generous; the two
    // legs before it are ordinary control-plane calls and get the
    // ordinary leg budget, so a wedged socket fails in seconds rather
    // than holding the whole minute a reap is allowed.
    let leg = leg_budget();
    let mut client = dial(socket, leg).await?;
    let identity = identify_on(&mut client, leg).await.with_context(|| {
        format!(
            "a socket exists at {} but no session answered {}",
            socket.display(),
            ops::SESSION_IDENTIFY
        )
    })?;
    let reap: SessionStopResult =
        tokio::time::timeout(budget, client.call(ops::SESSION_STOP, SessionStopParams {}))
            .await
            .map_err(|_| anyhow!("{} did not answer within {budget:?}", ops::SESSION_STOP))?
            .context(ops::SESSION_STOP)?;
    Ok(Some(StopReport { identity, reap }))
}

/// Wait for the stopped session to actually leave, bounded by `budget`.
///
/// Bounded the same way [`confirm_serving`] is, and for the same reason:
/// one round can spend two [`IPC_TIMEOUT`]s plus a probe, so only the
/// outer `timeout_at` makes the advertised budget true.
pub async fn await_stopped(socket: &Path, stopping: &str, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    // What the last completed round saw — the timeout message has to
    // say which of the two very different failures happened.
    let mut last = LastSeen::StillAnswering;
    let leg = leg_budget();
    let polling = async {
        loop {
            // `None` = nothing is listening; `Some(None)` = a socket
            // that would not answer, which is the ambiguous case.
            let answered = if probe_gone(socket).await {
                None
            } else {
                Some(identify(socket, leg).await.ok().map(|i| i.session_id))
            };
            let observation = match &answered {
                None => PollObservation::Unreachable,
                Some(None) => PollObservation::Indeterminate,
                Some(Some(id)) => PollObservation::Identified(id),
            };
            last = match observation {
                PollObservation::Indeterminate => LastSeen::SocketWontAnswer,
                _ => LastSeen::StillAnswering,
            };
            if stop_completed(observation, stopping) {
                return Ok(());
            }
            if Instant::now() + POLL_INTERVAL >= deadline {
                return Err(last);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    };
    // An outer expiry means the round in flight never finished, which is
    // the unanswerable case by definition.
    match tokio::time::timeout_at(deadline, polling).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(last)) => Err(last.into_error(socket, stopping, budget)),
        Err(_) => Err(LastSeen::SocketWontAnswer.into_error(socket, stopping, budget)),
    }
}

/// How the last poll round saw the socket. Only exists so the timeout
/// error says something true: "still answering as the same session" and
/// "left a socket that will not answer" are different faults, and
/// reporting the first for the second sends the reader hunting a process
/// that is already gone.
#[derive(Debug, Clone, Copy)]
enum LastSeen {
    StillAnswering,
    SocketWontAnswer,
}

impl LastSeen {
    fn into_error(self, socket: &Path, stopping: &str, budget: Duration) -> anyhow::Error {
        let seconds = budget.as_secs().max(1);
        match self {
            Self::StillAnswering => anyhow!(
                "session {stopping} reported its reap but was still answering at {} \
                 after {seconds}s",
                socket.display()
            ),
            Self::SocketWontAnswer => anyhow!(
                "session {stopping} reported its reap but left a socket at {} that \
                 would not answer session.identify after {seconds}s",
                socket.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one sentinel, resolved in one place. A path that merely
    /// *contains* the word is an ordinary socket path — the comparison
    /// is the whole string, because a directory called `localhost` is a
    /// perfectly legal place to keep a socket.
    #[test]
    fn only_the_bare_localhost_spelling_means_this_machines_session() {
        let (socket, localhost) = resolve_target(LOCALHOST_TARGET).expect("resolve localhost");
        assert!(localhost);
        assert_eq!(
            socket,
            crate::paths::BundleProfile::session()
                .expect("session profile")
                .socket_path
        );

        for target in ["/tmp/roost-popos.sock", "/var/run/localhost/roost.sock"] {
            let (socket, localhost) = resolve_target(target).expect("resolve a path target");
            assert!(!localhost, "{target:?} is a socket path, not the sentinel");
            assert_eq!(socket, PathBuf::from(target));
        }

        // HS-3: a bare token with no path separator is now classified
        // as an ssh target (`crate::ssh::classify` rule 6), not a
        // same-directory socket path — `resolve_target` cannot produce
        // a `PathBuf` for one and errors instead of silently treating
        // it as a filename. Empty targets are likewise now a hard
        // error (classify rule 1) rather than an empty-string path.
        for target in ["localhost.sock", "Localhost", ""] {
            resolve_target(target)
                .expect_err(&format!("{target:?} must not resolve to a socket path"));
        }
    }

    #[test]
    fn verdicts_round_trip_through_their_wire_line() {
        for verdict in [
            Verdict::Ready(4321),
            Verdict::AlreadyRunning(Some(99)),
            Verdict::AlreadyRunning(None),
            Verdict::Error("state dir is busy".into()),
        ] {
            assert_eq!(Verdict::parse(&verdict.to_string()), verdict);
        }
    }

    #[test]
    fn exit_codes_treat_a_lost_race_as_success() {
        assert_eq!(Verdict::Ready(1).exit_code(), 0);
        assert_eq!(Verdict::AlreadyRunning(Some(1)).exit_code(), 0);
        assert_eq!(Verdict::AlreadyRunning(None).exit_code(), 0);
        assert_eq!(Verdict::Error("x".into()).exit_code(), 1);
    }

    /// An anyhow chain rendered with `{:#}` is single-line already, but
    /// a `Display` impl somewhere in it need not be — and a newline
    /// would truncate the verdict at the reader.
    #[test]
    fn an_error_verdict_is_always_one_line() {
        let verdict = Verdict::Error("bind failed\ncaused by: EADDRINUSE\n".into());
        let line = verdict.to_string();
        assert!(!line.contains('\n'), "{line}");
        assert_eq!(line, "error: bind failed; caused by: EADDRINUSE");
    }

    /// The prefix comes out of the same budget as the reason: a reader
    /// that rejects anything over `MAX_VERDICT_BYTES` must never be
    /// handed a line this formatter produced.
    #[test]
    fn a_whole_error_line_fits_the_budget_the_reader_enforces() {
        for reason in ["é".repeat(MAX_VERDICT_BYTES), "x".repeat(MAX_VERDICT_BYTES)] {
            let line = Verdict::Error(reason).to_string();
            assert!(
                line.len() <= MAX_VERDICT_BYTES,
                "{} bytes exceeds the cap",
                line.len()
            );
            assert!(line.starts_with(ERROR_PREFIX));
        }
        // Cut on a char boundary, so the truncated reason is still the
        // multi-byte text it started as.
        assert!(Verdict::Error("é".repeat(MAX_VERDICT_BYTES))
            .to_string()
            .starts_with("error: éé"));
    }

    #[test]
    fn a_reason_that_fits_is_not_touched() {
        assert_eq!(
            Verdict::Error("state dir is busy".into()).to_string(),
            "error: state dir is busy"
        );
    }

    #[test]
    fn an_unrecognized_line_parses_as_an_error() {
        assert!(matches!(Verdict::parse("who knows"), Verdict::Error(_)));
        // A malformed pid is not a `ready` — the caller must not learn a
        // pid of 0 for a live session.
        assert!(matches!(Verdict::parse("ready pid=abc"), Verdict::Error(_)));
        // Nor is an empty line, which is what a reader gets from a
        // channel that closed without a verdict.
        assert!(matches!(Verdict::parse(""), Verdict::Error(_)));
    }

    #[test]
    fn a_verdict_line_survives_the_trailing_newline_it_travels_with() {
        assert_eq!(Verdict::parse("ready pid=7\n"), Verdict::Ready(7));
        assert_eq!(
            Verdict::parse("already-running pid=7\r\n"),
            Verdict::AlreadyRunning(Some(7))
        );
    }

    /// The launch hint's name is a cross-process contract: `roostctl`
    /// sets it, `roost-session` consumes it, and nothing renames it
    /// without both ends moving.
    #[test]
    fn the_launch_cwd_env_name_is_frozen() {
        assert_eq!(LAUNCH_CWD_ENV, "ROOST_SESSION_LAUNCH_CWD");
    }

    #[test]
    fn a_plausible_scale_is_taken_at_face_value() {
        assert_eq!(parse_timeout_scale(Some("2.5")), 2.5);
        assert_eq!(parse_timeout_scale(Some(" 3 ")), 3.0);
        // The endpoints are inclusive.
        assert_eq!(parse_timeout_scale(Some("0.01")), MIN_TIMEOUT_SCALE);
        assert_eq!(parse_timeout_scale(Some("1000")), MAX_TIMEOUT_SCALE);
    }

    #[test]
    fn an_unusable_scale_falls_back_to_the_shipped_budget() {
        for raw in [
            None,
            Some(""),
            Some("nope"),
            Some("0"),
            Some("-2"),
            Some("NaN"),
            Some("inf"),
            // Over the ceiling: `Duration::mul_f64` panics on these.
            Some("1e300"),
            Some("1001"),
            // Under the floor: these round every budget to zero.
            Some("5e-324"),
            Some("0.0001"),
        ] {
            assert_eq!(parse_timeout_scale(raw), 1.0, "{raw:?}");
        }
    }

    /// The reason the range exists: every accepted scale must survive
    /// the multiplication it is read for, and leave a budget that is
    /// still a budget.
    #[test]
    fn every_accepted_scale_scales_a_budget_without_panicking() {
        let budget = std::time::Duration::from_secs(60);
        for raw in ["1e300", "5e-324", "0.01", "1000", "nope", "2.5", "1"] {
            let scaled = budget.mul_f64(parse_timeout_scale(Some(raw)));
            assert!(!scaled.is_zero(), "{raw} rounded the budget to zero");
        }
    }

    #[test]
    fn nothing_listening_is_the_only_state_read_as_stopped() {
        assert!(socket_gone(&SocketState::Missing));
        assert!(socket_gone(&SocketState::Stale));
        assert!(!socket_gone(&SocketState::Live));
        assert!(!socket_gone(&SocketState::Indeterminate("backlog".into())));
        assert!(!socket_gone(&SocketState::NotASocket("regular file")));
    }

    /// The poll's whole decision table. The interesting row is the
    /// third: a *different* session on the socket means ours left, which
    /// is what was asked for.
    #[test]
    fn the_stop_poll_finishes_only_when_our_session_is_the_one_that_left() {
        assert!(stop_completed(PollObservation::Unreachable, "sess-1"));
        assert!(stop_completed(
            PollObservation::Identified("sess-2"),
            "sess-1"
        ));
        assert!(!stop_completed(
            PollObservation::Identified("sess-1"),
            "sess-1"
        ));
        // Fail-safe: an unanswerable socket is never called dead.
        assert!(!stop_completed(PollObservation::Indeterminate, "sess-1"));
    }

    /// Stopping a path nothing is listening at is a success that costs
    /// no round trip — the rung that lets the UI's restart flow run
    /// against an already-dead session without special-casing it.
    #[tokio::test]
    async fn stopping_a_socket_nobody_is_serving_reports_nothing_to_stop() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("absent.sock");
        assert!(stop_session(&socket, DEFAULT_STOP_CALL_BUDGET)
            .await
            .expect("an absent socket is not a failure")
            .is_none());
        // And the wait that follows it has nothing to wait for.
        await_stopped(&socket, "sess-1", DEFAULT_STOP_GONE_BUDGET)
            .await
            .expect("an absent socket is already gone");
    }

    /// A listener that never answers `session.identify` is the
    /// ambiguous case, and the wait must spend its budget rather than
    /// calling it stopped.
    #[tokio::test]
    async fn a_socket_that_will_not_answer_is_never_called_stopped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("mute.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");

        let error = await_stopped(&socket, "sess-1", Duration::from_millis(150))
            .await
            .expect_err("a live socket is not a stopped session");
        assert!(
            format!("{error:#}").contains("would not answer"),
            "{error:#}"
        );
        drop(listener);
    }
}
