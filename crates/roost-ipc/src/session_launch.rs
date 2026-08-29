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
use tokio::time::Instant;

use crate::messages::{ops, SessionIdentify, SessionIdentifyParams};
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
async fn reap_by(child: &mut Child, deadline: Instant) {
    if tokio::time::timeout_at(deadline, child.wait())
        .await
        .is_err()
    {
        // `kill` is SIGKILL + reap, so the wait behind it is the one
        // the kernel is about to satisfy.
        let _ = child.kill().await;
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
    let leg = IPC_TIMEOUT.mul_f64(timeout_scale());
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
