//! `roostctl session` — start, stop, and inspect the headless host
//! session daemon (`roost-session`).
//!
//! # Why this never goes through the connect prologue
//!
//! Every other `roostctl` subcommand resolves a *UI* target and dials
//! it. A session is not a UI and is deliberately not reachable by
//! `--target` / `ROOST_BUNDLE_PROFILE` / auto-detect (the HS-0 fences in
//! `roost_ipc::target` pin that). These three verbs address the session
//! profile's socket directly, and `start` must work when nothing is
//! listening at all — so they run as a pre-connect carve-out alongside
//! `doctor` and `claude-hook`.
//!
//! # Why `start` confirms rather than trusting the verdict
//!
//! `roost-session start` prints one line and exits. `already-running`
//! is written by the process that *lost* the socket lock, and the
//! loser can reach its `println!` before the winner has finished
//! binding — so a caller that treated the line as proof would hand the
//! user a socket that is not there yet. Both success verdicts are
//! therefore followed by a bounded `session.identify` poll, and exit 0
//! means "a session answered", never "a line was printed".
//!
//! The split here is the doctor precedent: the decisions
//! ([`locate_session_binary`], [`classify_verdict`], [`stop_completed`])
//! are pure and table-tested; the I/O around them is thin.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::Instant;

use roost_ipc::messages::{
    ops, SessionIdentify, SessionIdentifyParams, SessionStopParams, SessionStopResult,
    TabListResult,
};
use roost_ipc::paths::BundleProfile;
use roost_ipc::session_launch::{timeout_scale, Verdict, LAUNCH_CWD_ENV, MAX_VERDICT_BYTES};
use roost_ipc::socket_state::{self, SocketState};
use roost_ipc::IpcClient;

/// The daemon binary's name, as installed next to `roostctl`
/// (`/usr/bin/roost-session` from the deb).
const BIN_NAME: &str = "roost-session";

/// Override naming the daemon binary outright. First rung of
/// [`locate_session_binary`]; the tests and a from-source `cargo run`
/// both need it.
const BIN_ENV: &str = "ROOST_SESSION_BIN";

/// How long to wait for the spawned `roost-session start` to print its
/// verdict line.
///
/// Must comfortably exceed the daemon's own `parent_ready_timeout()`
/// (30s), because that parent is who we are reading: expiring first
/// would report a timeout for a start that was still going to answer.
/// On expiry the child is killed — it is the *forking parent*, so
/// killing it never touches a session that did come up.
const VERDICT_TIMEOUT: Duration = Duration::from_secs(45);

/// How long `start` polls `session.identify` before declaring that the
/// verdict lied. The winner of the socket-lock race has already bound
/// by the time it writes `ready`, so this only ever covers the
/// `already-running` loser overtaking the winner — microseconds in
/// practice.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `stop` waits for the reap report. Generous: the reply does
/// not come back until every child is accounted for, which is the
/// engine's 5s soft deadline plus the post-SIGKILL tail.
const STOP_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long `stop` waits, after the reap report, for the socket to
/// actually go away. The session unlinks it in a detached finalizer
/// that runs *after* the reply, so "stopped" is only true once the poll
/// below says so.
const STOP_GONE_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval for both bounded polls. Short: these are local `connect(2)`
/// calls, and the thing being waited for usually lands immediately.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A single IPC leg's budget. `IpcClient` has no timeout of its own and
/// "the session is wedged" must render as an error, not a hung CLI.
const IPC_TIMEOUT: Duration = Duration::from_secs(10);

/// `session status` when nothing is running. Nonzero because that is
/// what a status verb is *for* — `systemctl status` exits 3 on a
/// stopped unit, and a script asking "is my session up?" should be able
/// to branch on the status code alone.
const STATUS_NOT_RUNNING_EXIT: i32 = 3;

/// Scale every budget above by [`timeout_scale`] — the same reader
/// `roost-session` uses, so a loaded CI runner widens the driver's
/// waits alongside the daemon's.
fn scaled(budget: Duration) -> Duration {
    budget.mul_f64(timeout_scale())
}

#[derive(Subcommand, Debug)]
pub enum SessionCmd {
    /// Start the headless host session for this machine, or confirm the
    /// one already running. Exits 0 only once a session answers
    /// `session.identify` on its socket.
    ///
    /// The session inherits this command's working directory as the
    /// seed for its first project (a fresh state file only).
    Start,
    /// Stop the running session: every tab's shell is hung up and
    /// reaped, then the socket goes away. Prints the reap report.
    /// Stopping something that is not running succeeds.
    Stop,
    /// Print the running session's identity and workspace size, or
    /// report that none is running (exit 3).
    Status,
}

/// Run a `session` verb. Returns the process exit code rather than
/// exiting, so the caller keeps one exit point.
pub async fn run(cmd: &SessionCmd) -> i32 {
    let result = match cmd {
        SessionCmd::Start => start().await,
        SessionCmd::Stop => stop().await,
        SessionCmd::Status => status().await,
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("roostctl session: {error:#}");
            1
        }
    }
}

// ============================================================================
// Locating the daemon binary
// ============================================================================

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
    fn describe(self) -> &'static str {
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
// start
// ============================================================================

/// What a verdict line means for `roostctl session start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartStep {
    /// The session claims to be serving (fresh, `Some(pid)`) or claims
    /// somebody else already is (`None` when the winner's pid was
    /// unreadable). Either way: confirm before reporting success.
    Confirm { pid: Option<u32>, fresh: bool },
    /// The start failed. Carries the message to print.
    Failed(String),
}

/// Pure reading of a verdict line. Both success verdicts fall through
/// to the same confirmation — see the module docs for why
/// `already-running` cannot be trusted on its own.
pub fn classify_verdict(verdict: &Verdict) -> StartStep {
    match verdict {
        Verdict::Ready(pid) => StartStep::Confirm {
            pid: Some(*pid),
            fresh: true,
        },
        Verdict::AlreadyRunning(pid) => StartStep::Confirm {
            // The lock file's pid is an `i32` because that is what it
            // holds on disk; a negative or zero value there is a
            // corrupt file, not a process.
            pid: pid.and_then(|p| u32::try_from(p).ok()).filter(|p| *p > 0),
            fresh: false,
        },
        Verdict::Error(reason) => StartStep::Failed(format!("{BIN_NAME} start failed: {reason}")),
    }
}

async fn start() -> Result<i32> {
    let bin = locate_session_binary(
        std::env::var_os(BIN_ENV).as_deref(),
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )?;
    let cwd = std::env::current_dir().context("read the working directory to seed the session")?;

    let verdict = spawn_and_read_verdict(&bin.path, &cwd, scaled(VERDICT_TIMEOUT)).await?;
    let (pid, fresh) = match classify_verdict(&verdict) {
        StartStep::Confirm { pid, fresh } => (pid, fresh),
        StartStep::Failed(message) => {
            eprintln!("roostctl session: {message}");
            return Ok(1);
        }
    };

    let socket = BundleProfile::session()
        .context("resolve the session socket path")?
        .socket_path;
    let identity = confirm_serving(&socket, scaled(CONFIRM_TIMEOUT))
        .await
        .with_context(|| {
            format!(
                "{BIN_NAME} reported `{verdict}` but no session answered at {}",
                socket.display()
            )
        })?;

    println!("{}", if fresh { "started" } else { "already-running" });
    print_identity(&identity, &socket);
    if let Some(pid) = pid {
        // Deliberately not `pid=`. The confirmation above asks the
        // *socket* who it is, not the launcher's child — and
        // `session.identify` carries no pid, so nothing here ties this
        // number to the session just identified. Under a stop/start
        // race they can genuinely be two processes; the name says whose
        // word it is so a reader is never misled into pairing them.
        println!("launcher_reported_pid={pid}");
    }
    Ok(0)
}

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
async fn read_verdict_line<R: AsyncRead + Unpin>(reader: R, cap: usize) -> VerdictRead {
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
/// hold `roostctl` open past its budget.
async fn spawn_and_read_verdict(bin: &Path, cwd: &Path, budget: Duration) -> Result<Verdict> {
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

/// Poll `session.identify` until a session answers or the budget runs
/// out. This is what turns a printed verdict into a fact.
///
/// The outer `timeout_at` is what makes `budget` a real bound. A single
/// round can block for a connect plus a call — two [`IPC_TIMEOUT`]s —
/// so a loop that only checked the deadline *between* rounds would
/// overrun the budget it advertises by more than double.
async fn confirm_serving(socket: &Path, budget: Duration) -> Result<SessionIdentify> {
    let deadline = Instant::now() + budget;
    let polling = async {
        loop {
            let error = match identify(socket).await {
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
// stop
// ============================================================================

/// One round of the post-stop poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollObservation<'a> {
    /// The socket is gone or refuses connections — nothing is listening.
    Unreachable,
    /// A session answered `session.identify` with this id.
    Identified(&'a str),
    /// Something is at the path but it could not be asked (a full
    /// accept backlog, a permission error, a timeout).
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
/// `socket_state`'s fail-safe rule: the one thing we must never do is
/// call a live session dead.
pub fn stop_completed(observation: PollObservation<'_>, stopping: &str) -> bool {
    match observation {
        PollObservation::Unreachable => true,
        PollObservation::Identified(id) => id != stopping,
        PollObservation::Indeterminate => false,
    }
}

/// The two socket states that prove nothing can be listening. Same pair
/// `socket_state` calls safe-to-unlink, read here as "not running" —
/// every other state, including `Indeterminate`, means assume-live.
fn socket_gone(state: &SocketState) -> bool {
    matches!(state, SocketState::Missing | SocketState::Stale)
}

/// Probe once and read off whether nothing is listening.
async fn probe_gone(socket: &Path) -> bool {
    socket_gone(&socket_state::probe(socket, socket_state::PROBE_TIMEOUT).await)
}

async fn stop() -> Result<i32> {
    let socket = BundleProfile::session()
        .context("resolve the session socket path")?
        .socket_path;

    if probe_gone(&socket).await {
        // Stop of a stopped session is a success, `systemctl stop`
        // style: the caller asked for a state, and it holds.
        println!("not running (no session at {})", socket.display());
        return Ok(0);
    }

    let mut client = connect(&socket).await?;
    let identity = identify_on(&mut client).await.with_context(|| {
        format!(
            "a socket exists at {} but no session answered session.identify",
            socket.display()
        )
    })?;

    let report: SessionStopResult = tokio::time::timeout(
        scaled(STOP_CALL_TIMEOUT),
        client.call(ops::SESSION_STOP, SessionStopParams {}),
    )
    .await
    .map_err(|_| anyhow!("session.stop did not answer within {STOP_CALL_TIMEOUT:?}"))?
    .context("session.stop")?;

    println!("stopping session {}", identity.session_id);
    print_reap_report(&report);

    // The socket is unlinked by a finalizer that runs *after* the reply
    // above, so the session is only really gone once this poll says so.
    await_gone(&socket, &identity.session_id, scaled(STOP_GONE_TIMEOUT)).await?;
    println!("stopped");
    Ok(0)
}

/// Wait for the stopped session to actually leave, bounded by `budget`.
///
/// Bounded the same way [`confirm_serving`] is, and for the same reason:
/// one round can spend two [`IPC_TIMEOUT`]s plus a probe, so only the
/// outer `timeout_at` makes the advertised budget true.
async fn await_gone(socket: &Path, stopping: &str, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    // What the last completed round saw — the timeout message has to
    // say which of the two very different failures happened.
    let mut last = LastSeen::StillAnswering;
    let polling = async {
        loop {
            // `None` = nothing is listening; `Some(None)` = a socket
            // that would not answer, which is the ambiguous case.
            let answered = if probe_gone(socket).await {
                None
            } else {
                Some(identify(socket).await.ok().map(|i| i.session_id))
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
/// reporting the first for the second sends the reader hunting a
/// process that is already gone.
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

fn print_reap_report(report: &SessionStopResult) {
    println!(
        "reaped={} killed={} abandoned={}",
        report.reaped.len(),
        report.killed.len(),
        report.abandoned.len()
    );
    for (label, ids) in [
        ("reaped", &report.reaped),
        ("killed", &report.killed),
        ("abandoned", &report.abandoned),
    ] {
        if !ids.is_empty() {
            println!("  {label}: {}", join_ids(ids));
        }
    }
}

fn join_ids(ids: &[i64]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

// ============================================================================
// status
// ============================================================================

async fn status() -> Result<i32> {
    let socket = BundleProfile::session()
        .context("resolve the session socket path")?
        .socket_path;

    if probe_gone(&socket).await {
        println!("not running (no session at {})", socket.display());
        return Ok(STATUS_NOT_RUNNING_EXIT);
    }

    let mut client = connect(&socket).await?;
    let identity = match identify_on(&mut client).await {
        Ok(identity) => identity,
        Err(error) => {
            // A socket that will not answer is a real fault, not a
            // clean "stopped" — say so and exit 1, not 3.
            return Err(error.context(format!(
                "a socket exists at {} but no session answered",
                socket.display()
            )));
        }
    };

    let list: TabListResult = call(&mut client, ops::TAB_LIST, serde_json::json!({}))
        .await
        .context("tab.list")?;
    let tabs: usize = list.projects.iter().map(|p| p.tabs.len()).sum();

    print_identity(&identity, &socket);
    println!("projects={}\ntabs={tabs}", list.projects.len());
    Ok(0)
}

/// Everything here comes from the session that answered on the socket —
/// the only authority on what is running.
fn print_identity(identity: &SessionIdentify, socket: &Path) {
    println!(
        "session_id={}\nsocket={}\nstarted_at={}\nversion={}\nsession_protocol={}",
        identity.session_id,
        socket.display(),
        identity.started_at,
        identity.app_version,
        identity.session_protocol
    );
}

// ============================================================================
// Thin IPC
// ============================================================================

async fn connect(socket: &Path) -> Result<IpcClient> {
    tokio::time::timeout(scaled(IPC_TIMEOUT), IpcClient::connect(socket))
        .await
        .map_err(|_| anyhow!("connecting to {} timed out", socket.display()))?
        .with_context(|| format!("connect to {}", socket.display()))
}

async fn call<P: serde::Serialize, R: serde::de::DeserializeOwned>(
    client: &mut IpcClient,
    op: &str,
    params: P,
) -> Result<R> {
    tokio::time::timeout(scaled(IPC_TIMEOUT), client.call(op, params))
        .await
        .map_err(|_| anyhow!("{op} timed out"))?
        .map_err(Into::into)
}

async fn identify(socket: &Path) -> Result<SessionIdentify> {
    let mut client = connect(socket).await?;
    identify_on(&mut client).await
}

async fn identify_on(client: &mut IpcClient) -> Result<SessionIdentify> {
    call(client, ops::SESSION_IDENTIFY, SessionIdentifyParams {}).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A file that exists and is executable, in a directory of its own.
    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// An executable shell script that behaves like a `roost-session`
    /// launcher — well or badly, per `body`.
    ///
    /// The script states its own `PATH` because a spawned launcher
    /// inherits this process's environment, and `doctor`'s tests point
    /// the process-global `PATH` at an empty directory while they run.
    /// Without this, a body using `sleep` or `head` silently becomes a
    /// body that exits instantly, and these tests pass or fail on which
    /// suite they were run with.
    fn fake_launcher(dir: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(BIN_NAME);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nPATH=/usr/bin:/bin\nexport PATH\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A unique scratch directory. `std::env::temp_dir()` plus the test
    /// name keeps the cases from colliding when the suite runs in
    /// parallel; no crate-level tempdir dependency exists here.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "roostctl-session-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_env_override_wins_over_a_sibling_and_the_path() {
        let root = scratch("env-wins");
        let chosen = fake_bin(&root.join("explicit"), "whatever");
        let sibling_dir = root.join("bin");
        fake_bin(&sibling_dir, BIN_NAME);
        let path_dir = root.join("usr-bin");
        fake_bin(&path_dir, BIN_NAME);

        let found = locate_session_binary(
            Some(chosen.as_os_str()),
            Some(&sibling_dir.join("roostctl")),
            Some(path_dir.as_os_str()),
        )
        .unwrap();
        assert_eq!(
            found,
            LocatedBin {
                path: chosen,
                origin: BinOrigin::Env
            }
        );
    }

    #[test]
    fn a_broken_env_override_is_an_error_not_a_fallback() {
        let root = scratch("env-broken");
        let path_dir = root.join("usr-bin");
        fake_bin(&path_dir, BIN_NAME);

        let missing = root.join("nope");
        let error =
            locate_session_binary(Some(missing.as_os_str()), None, Some(path_dir.as_os_str()))
                .expect_err("a named-but-missing override must not silently start another binary");
        assert!(error.to_string().contains(BIN_ENV), "{error}");
    }

    #[test]
    fn an_empty_env_override_falls_through_to_the_sibling() {
        // The launchd-inherited empty-env case, same as target.rs's.
        let root = scratch("env-empty");
        let sibling_dir = root.join("bin");
        let sibling = fake_bin(&sibling_dir, BIN_NAME);

        let found = locate_session_binary(
            Some(OsStr::new("")),
            Some(&sibling_dir.join("roostctl")),
            None,
        )
        .unwrap();
        assert_eq!(found.path, sibling);
        assert_eq!(found.origin, BinOrigin::Sibling);
    }

    #[test]
    fn the_sibling_wins_over_the_path() {
        let root = scratch("sibling-wins");
        let sibling_dir = root.join("bin");
        let sibling = fake_bin(&sibling_dir, BIN_NAME);
        let path_dir = root.join("usr-bin");
        fake_bin(&path_dir, BIN_NAME);

        let found = locate_session_binary(
            None,
            Some(&sibling_dir.join("roostctl")),
            Some(path_dir.as_os_str()),
        )
        .unwrap();
        assert_eq!(found.path, sibling);
        assert_eq!(found.origin, BinOrigin::Sibling);
    }

    #[test]
    fn the_path_is_the_last_rung_and_is_searched_in_order() {
        let root = scratch("path-last");
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let first = root.join("first");
        let hit = fake_bin(&first, BIN_NAME);
        let second = root.join("second");
        fake_bin(&second, BIN_NAME);

        let path_env = std::env::join_paths([&empty, &first, &second]).unwrap();
        let found = locate_session_binary(
            None,
            // A roostctl with no session binary beside it.
            Some(&empty.join("roostctl")),
            Some(path_env.as_os_str()),
        )
        .unwrap();
        assert_eq!(found.path, hit);
        assert_eq!(found.origin, BinOrigin::Path);
    }

    #[test]
    fn a_non_executable_candidate_does_not_count_as_a_hit() {
        let root = scratch("not-executable");
        let sibling_dir = root.join("bin");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        // Present but mode 0644 — a stray file, not a program.
        let dud = sibling_dir.join(BIN_NAME);
        std::fs::write(&dud, b"not a program").unwrap();
        std::fs::set_permissions(&dud, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path_dir = root.join("usr-bin");
        let real = fake_bin(&path_dir, BIN_NAME);

        let found = locate_session_binary(
            None,
            Some(&sibling_dir.join("roostctl")),
            Some(path_dir.as_os_str()),
        )
        .unwrap();
        assert_eq!(found.path, real);
    }

    /// The permission-bit test this replaced said yes to a file its own
    /// owner cannot run: `0o001` has an execute bit, but the owner class
    /// is checked first and has none. Such a sibling must fall through
    /// to a usable `$PATH` hit rather than shadow it and fail at spawn
    /// with `EACCES`.
    #[test]
    fn a_candidate_this_user_cannot_execute_falls_through() {
        // SAFETY: a plain getter with no arguments.
        if unsafe { libc::geteuid() } == 0 {
            // root bypasses the permission check, so for root the file
            // genuinely is executable and there is nothing to assert.
            return;
        }
        let root = scratch("not-ours-to-execute");
        let sibling_dir = root.join("bin");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        let others_only = sibling_dir.join(BIN_NAME);
        std::fs::write(&others_only, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&others_only, std::fs::Permissions::from_mode(0o001)).unwrap();
        let path_dir = root.join("usr-bin");
        let usable = fake_bin(&path_dir, BIN_NAME);

        let found = locate_session_binary(
            None,
            Some(&sibling_dir.join("roostctl")),
            Some(path_dir.as_os_str()),
        )
        .unwrap();
        assert_eq!(found.path, usable);
        assert_eq!(found.origin, BinOrigin::Path);

        // Named outright, the same file is a hard error — the user gets
        // told, rather than silently getting a different binary.
        let error = locate_session_binary(Some(others_only.as_os_str()), None, None)
            .expect_err("an unusable explicit override must be reported");
        assert!(error.to_string().contains(BIN_ENV), "{error}");
    }

    #[test]
    fn a_directory_named_like_the_binary_is_not_a_hit() {
        let root = scratch("dir-named-bin");
        let sibling_dir = root.join("bin");
        // Directories are 0755 — executable-bit set, but not a program.
        std::fs::create_dir_all(sibling_dir.join(BIN_NAME)).unwrap();

        let error = locate_session_binary(None, Some(&sibling_dir.join("roostctl")), None)
            .expect_err("a directory must not satisfy the search");
        assert!(error.to_string().contains("cannot find"), "{error}");
    }

    #[test]
    fn nothing_found_names_all_three_rungs() {
        let root = scratch("nothing-found");
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        let error =
            locate_session_binary(None, Some(&empty.join("roostctl")), Some(empty.as_os_str()))
                .expect_err("no candidate exists");
        let message = error.to_string();
        for expected in ["$ROOST_SESSION_BIN", "next to roostctl", "$PATH"] {
            assert!(
                message.contains(expected),
                "{expected} missing from:\n{message}"
            );
        }
    }

    #[test]
    fn an_empty_path_element_is_not_searched_as_the_cwd() {
        let root = scratch("empty-path-element");
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        // `":"` is two empty elements — neither may resolve to `./roost-session`.
        let error =
            locate_session_binary(None, Some(&empty.join("roostctl")), Some(OsStr::new(":")))
                .expect_err("empty PATH elements must not be searched");
        assert!(error.to_string().contains("cannot find"), "{error}");
    }

    #[test]
    fn both_success_verdicts_are_confirmed_before_success_is_reported() {
        assert_eq!(
            classify_verdict(&Verdict::Ready(4321)),
            StartStep::Confirm {
                pid: Some(4321),
                fresh: true
            }
        );
        assert_eq!(
            classify_verdict(&Verdict::AlreadyRunning(Some(99))),
            StartStep::Confirm {
                pid: Some(99),
                fresh: false
            }
        );
        // A lock file we could not read loses the pid, not the verdict.
        assert_eq!(
            classify_verdict(&Verdict::AlreadyRunning(None)),
            StartStep::Confirm {
                pid: None,
                fresh: false
            }
        );
    }

    #[test]
    fn a_corrupt_lock_pid_is_dropped_rather_than_printed() {
        for corrupt in [0, -1] {
            assert_eq!(
                classify_verdict(&Verdict::AlreadyRunning(Some(corrupt))),
                StartStep::Confirm {
                    pid: None,
                    fresh: false
                },
                "pid {corrupt}"
            );
        }
    }

    #[test]
    fn an_error_verdict_carries_its_reason_into_the_message() {
        let step = classify_verdict(&Verdict::Error("state dir is busy".into()));
        match step {
            StartStep::Failed(message) => {
                assert!(message.contains("state dir is busy"), "{message}")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// An unparseable line reaches `classify_verdict` as an error, so a
    /// garbled stdout can never be read as a successful start.
    #[test]
    fn a_garbled_verdict_line_is_a_failed_start() {
        let step = classify_verdict(&Verdict::parse("Segmentation fault"));
        assert!(matches!(step, StartStep::Failed(_)), "{step:?}");
    }

    #[test]
    fn the_stop_poll_ends_when_nothing_is_listening() {
        assert!(stop_completed(PollObservation::Unreachable, "sess-1"));
    }

    #[test]
    fn a_different_session_id_means_ours_stopped() {
        // A racing spawn-if-missing bound the socket after our reap.
        // That is a new session, not a failed stop.
        assert!(stop_completed(
            PollObservation::Identified("sess-2"),
            "sess-1"
        ));
    }

    #[test]
    fn the_same_session_id_keeps_the_poll_running() {
        assert!(!stop_completed(
            PollObservation::Identified("sess-1"),
            "sess-1"
        ));
    }

    /// Fail-safe, like `socket_state`: an unanswerable socket is never
    /// reported as stopped.
    #[test]
    fn an_indeterminate_socket_keeps_the_poll_running() {
        assert!(!stop_completed(PollObservation::Indeterminate, "sess-1"));
    }

    #[test]
    fn only_missing_and_stale_read_as_not_running() {
        assert!(socket_gone(&SocketState::Missing));
        assert!(socket_gone(&SocketState::Stale));
        assert!(!socket_gone(&SocketState::Live));
        assert!(!socket_gone(&SocketState::Indeterminate("backlog".into())));
        assert!(!socket_gone(&SocketState::NotASocket("regular file")));
    }

    /// `scaled` is the one place this crate's budgets meet the shared
    /// scale reader, and `Duration::mul_f64` panics on a big enough
    /// factor — so drive the real function over the values that used to
    /// get through, not a local copy of its predicate.
    #[test]
    fn no_env_scale_can_panic_or_zero_one_of_this_crates_budgets() {
        for raw in ["1e300", "5e-324", "nope", "0", "-2", "inf", "1000", "0.01"] {
            let factor = roost_ipc::session_launch::parse_timeout_scale(Some(raw));
            for budget in [
                VERDICT_TIMEOUT,
                CONFIRM_TIMEOUT,
                STOP_CALL_TIMEOUT,
                STOP_GONE_TIMEOUT,
                POLL_INTERVAL,
                IPC_TIMEOUT,
            ] {
                assert!(!budget.mul_f64(factor).is_zero(), "{raw} on {budget:?}");
            }
        }
    }

    #[test]
    fn the_ambient_scale_leaves_every_budget_usable() {
        for budget in [VERDICT_TIMEOUT, CONFIRM_TIMEOUT, POLL_INTERVAL] {
            assert!(!scaled(budget).is_zero(), "{budget:?}");
        }
    }

    #[test]
    fn reap_ids_render_as_a_comma_list() {
        assert_eq!(join_ids(&[3, 1, 2]), "3,1,2");
        assert_eq!(join_ids(&[]), "");
    }

    // ------------------------------------------------------------------
    // Reading the verdict line
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn a_terminated_line_within_the_cap_is_the_verdict() {
        assert_eq!(
            read_verdict_line(&b"ready pid=4321\n"[..], MAX_VERDICT_BYTES).await,
            VerdictRead::Line("ready pid=4321".into())
        );
        // Only the first line is ours; the rest of the stream is not
        // read, so a second line can never be mistaken for a verdict.
        assert_eq!(
            read_verdict_line(&b"already-running\nnoise\n"[..], MAX_VERDICT_BYTES).await,
            VerdictRead::Line("already-running".into())
        );
    }

    /// The daemon's own reader calls EOF-before-newline no-verdict
    /// (`daemonize::read_verdict` returns `Ok(None)`), and so must this
    /// one — an unterminated frame is not a verdict however much it
    /// looks like one.
    #[tokio::test]
    async fn an_unterminated_stream_is_never_a_verdict() {
        assert_eq!(
            read_verdict_line(&b"ready pid=4321"[..], MAX_VERDICT_BYTES).await,
            VerdictRead::Eof
        );
        assert_eq!(
            read_verdict_line(&b""[..], MAX_VERDICT_BYTES).await,
            VerdictRead::Eof
        );
    }

    #[tokio::test]
    async fn a_line_exactly_at_the_cap_still_counts() {
        let line = "x".repeat(MAX_VERDICT_BYTES);
        let stream = format!("{line}\n").into_bytes();
        assert_eq!(
            read_verdict_line(&stream[..], MAX_VERDICT_BYTES).await,
            VerdictRead::Line(line)
        );
    }

    #[tokio::test]
    async fn one_byte_past_the_cap_is_refused_rather_than_buffered() {
        let stream = format!("{}\n", "x".repeat(MAX_VERDICT_BYTES + 1)).into_bytes();
        assert_eq!(
            read_verdict_line(&stream[..], MAX_VERDICT_BYTES).await,
            VerdictRead::TooLong
        );
        // The unbounded case this cap exists for: bytes with no newline
        // anywhere. The reader must stop, not grow.
        let flood = vec![b'x'; MAX_VERDICT_BYTES * 8];
        assert_eq!(
            read_verdict_line(&flood[..], MAX_VERDICT_BYTES).await,
            VerdictRead::TooLong
        );
    }

    #[tokio::test]
    async fn invalid_utf8_still_yields_a_line_to_parse() {
        // Lossy rather than an error: a garbled line has to reach
        // `Verdict::parse`, which reports it as a failed start.
        let VerdictRead::Line(line) =
            read_verdict_line(&b"ready pid=\xff\xfe\n"[..], MAX_VERDICT_BYTES).await
        else {
            panic!("a terminated line must decode");
        };
        assert!(matches!(
            classify_verdict(&Verdict::parse(&line)),
            StartStep::Failed(_)
        ));
    }

    // ------------------------------------------------------------------
    // The spawned launcher's lifecycle
    // ------------------------------------------------------------------

    /// Short enough that a hang is unmistakable, long enough that a
    /// loaded runner does not trip it.
    const TEST_BUDGET: Duration = Duration::from_millis(750);

    /// The bound every lifecycle case must respect. Well clear of
    /// `TEST_BUDGET` (which a killed launcher costs in full) and far
    /// below the 30s the scripts sleep for.
    const HANG_TRIPWIRE: Duration = Duration::from_secs(15);

    async fn run_launcher(tag: &str, body: &str) -> (Result<Verdict>, Duration) {
        let dir = scratch(tag);
        let bin = fake_launcher(&dir, body);
        let started = std::time::Instant::now();
        let verdict = spawn_and_read_verdict(&bin, &dir, TEST_BUDGET).await;
        (verdict, started.elapsed())
    }

    #[tokio::test]
    async fn a_well_behaved_launcher_is_read_and_reaped_immediately() {
        let (verdict, elapsed) = run_launcher("launcher-ok", "echo 'ready pid=4321'").await;
        assert_eq!(verdict.unwrap(), Verdict::Ready(4321));
        assert!(elapsed < HANG_TRIPWIRE, "{elapsed:?}");
    }

    /// The verdict is still good — but a launcher that prints and then
    /// refuses to exit must not hold `roostctl` open past its budget.
    #[tokio::test]
    async fn a_launcher_that_prints_then_hangs_is_killed_at_the_deadline() {
        let (verdict, elapsed) =
            run_launcher("launcher-hangs-after", "echo 'ready pid=7'; sleep 30").await;
        assert_eq!(verdict.unwrap(), Verdict::Ready(7));
        assert!(elapsed < HANG_TRIPWIRE, "{elapsed:?}");
    }

    /// Closing stdout is EOF for the reader, but the process is still
    /// there. The reap has to be bounded too, or this hangs forever.
    #[tokio::test]
    async fn a_launcher_that_closes_stdout_and_lives_on_does_not_hang_the_cli() {
        let (verdict, elapsed) =
            run_launcher("launcher-closes-stdout", "exec 1>&-; sleep 30").await;
        let error = verdict.expect_err("no verdict was printed").to_string();
        assert!(error.contains("closed its output"), "{error}");
        assert!(elapsed < HANG_TRIPWIRE, "{elapsed:?}");
    }

    #[tokio::test]
    async fn a_launcher_that_says_nothing_at_all_fails_the_start() {
        let (verdict, elapsed) = run_launcher("launcher-silent", "exit 0").await;
        let error = verdict.expect_err("no verdict was printed").to_string();
        assert!(error.contains("closed its output"), "{error}");
        assert!(elapsed < HANG_TRIPWIRE, "{elapsed:?}");
    }

    /// A verdict-shaped line with no newline is not a verdict — taking
    /// it would accept a launcher that died mid-write.
    #[tokio::test]
    async fn an_unterminated_verdict_is_not_accepted_from_a_real_process() {
        let (verdict, elapsed) =
            run_launcher("launcher-unterminated", r"printf 'ready pid=4321'").await;
        assert!(verdict.is_err(), "an unterminated line must not be a start");
        assert!(elapsed < HANG_TRIPWIRE, "{elapsed:?}");
    }

    #[tokio::test]
    async fn a_launcher_flooding_stdout_is_cut_off_rather_than_buffered() {
        let (verdict, elapsed) =
            run_launcher("launcher-floods", "head -c 200000 /dev/zero | tr '\\0' 'x'").await;
        let error = verdict.expect_err("a flood is not a verdict").to_string();
        assert!(error.contains("no newline"), "{error}");
        assert!(elapsed < HANG_TRIPWIRE, "{elapsed:?}");
    }

    #[tokio::test]
    async fn a_launcher_that_never_speaks_expires_on_the_budget() {
        let (verdict, elapsed) = run_launcher("launcher-mute", "sleep 30").await;
        let error = verdict.expect_err("nothing was printed").to_string();
        assert!(error.contains("did not report readiness"), "{error}");
        assert!(elapsed < HANG_TRIPWIRE, "{elapsed:?}");
    }
}
