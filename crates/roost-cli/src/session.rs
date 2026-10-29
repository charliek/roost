//! `roostctl session` — start, stop, and inspect the headless host
//! session daemon (`roost-session`).
//!
//! # Why this never dials the UI socket
//!
//! Every other `roostctl` subcommand resolves a *UI* target and dials
//! it. These three verbs are the pre-connect exception: they address
//! the session profile's socket directly and ignore `--target` /
//! `ROOST_BUNDLE_PROFILE` / auto-detect entirely, because `start` must
//! work when nothing is listening at all — so like `doctor` and
//! `claude-hook` they are served before `main.rs` ever builds its
//! `UiSocket`. A *generic* op can reach a session too, but only
//! explicitly — `--target session` or `--socket` (#475, HS-1 in
//! `roost_ipc::target`); `ROOST_BUNDLE_PROFILE=session` and
//! auto-detect stay refused (the HS-0 fence in `roost_ipc::target`).
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
//! ([`locate_session_binary`], [`classify_verdict`], and the stop poll's
//! own `stop_completed` next to it in [`session_launch`]) are pure and
//! table-tested; the I/O around them is thin.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;

use crate::error::CliError;
use roost_ipc::messages::{ops, SessionIdentify, SessionStopResult, TabListResult};
use roost_ipc::old_session::{self, OldSession};
use roost_ipc::paths::{derived_session_state_dir, BundleProfile, STATE_DIR_ENV};
use roost_ipc::session_launch::{
    self, await_stopped, confirm_serving, locate_session_binary, probe_gone,
    spawn_and_read_verdict, stop_session, stop_session_if, timeout_scale, FirstProject, Stop,
    Verdict, BIN_ENV, BIN_NAME, IPC_TIMEOUT,
};

/// How long to wait for the spawned `roost-session start` to print its
/// verdict line, and how long to poll before declaring the verdict a
/// lie. Both are read off the daemon's own waits rather than chosen
/// here, so they live with the ladder that climbs them.
const VERDICT_TIMEOUT: Duration = session_launch::DEFAULT_VERDICT_BUDGET;
const CONFIRM_TIMEOUT: Duration = session_launch::DEFAULT_CONFIRM_BUDGET;

/// The two halves of a stop's patience, read off the daemon's own waits
/// exactly as the start budgets above are.
const STOP_CALL_TIMEOUT: Duration = session_launch::DEFAULT_STOP_CALL_BUDGET;
const STOP_GONE_TIMEOUT: Duration = session_launch::DEFAULT_STOP_GONE_BUDGET;

/// `session status` when nothing is running. Nonzero because that is
/// what a status verb is *for* — `systemctl status` exits 3 on a
/// stopped unit, and a script asking "is my session up?" should be able
/// to branch on the status code alone.
pub const STATUS_NOT_RUNNING_EXIT: i32 = 3;

/// Scale every budget above by [`timeout_scale`] — the same reader
/// `roost-session` uses, so a loaded CI runner widens the driver's
/// waits alongside the daemon's.
pub(crate) fn scaled(budget: Duration) -> Duration {
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
    ///
    /// Only this build's own socket, unless `--old-path` names another.
    Stop {
        /// Stop the session listening at this socket instead: one that
        /// `session status` reports running at an old path, left by a
        /// build that kept its socket elsewhere. It must identify itself
        /// with a session id, and is stopped only if that id still
        /// answers when the stop is sent.
        #[arg(long, value_name = "SOCKET")]
        old_path: Option<PathBuf>,
    },
    /// Print the running session's identity and workspace size, or
    /// report that none is running (exit 3, `not-running`).
    Status,
}

/// Run a `session` verb, returning its exit code or its failure to the
/// caller's one renderer.
pub async fn run(cmd: &SessionCmd, json: bool) -> Result<i32, CliError> {
    match cmd {
        SessionCmd::Start => start(json).await,
        SessionCmd::Stop { old_path: None } => stop(json).await,
        SessionCmd::Stop {
            old_path: Some(socket),
        } => stop_old_path(socket, json).await,
        SessionCmd::Status => status(json).await,
    }
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

async fn start(json: bool) -> Result<i32, CliError> {
    let bin = locate_session_binary(
        std::env::var_os(BIN_ENV).as_deref(),
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
    .map_err(CliError::failed)?;
    let cwd = std::env::current_dir().map_err(|e| {
        CliError::Failed(format!(
            "read the working directory to tell the session about: {e}"
        ))
    })?;

    let socket = session_socket()?;
    // Before anything is spawned: a session on an old path would run
    // beside a new one.
    old_session::check_start(&socket)
        .await
        .map_err(CliError::Failed)?;

    // Said before the spawn, and phrased for what is true at that
    // moment: this start may yet fail, or find an `already-running`
    // session that was started elsewhere — possibly directly, which
    // honours the seam verbatim — so the derived directory is what a
    // session started *now* would use, not a claim about whichever
    // session ends up answering.
    let seam = std::env::var_os(STATE_DIR_ENV);
    if let Some(derived) = derived_session_state_dir(seam.as_deref()) {
        eprintln!(
            "roostctl session: {STATE_DIR_ENV} is set; a session started now would use {}",
            derived.display()
        );
    }

    let verdict =
        // `roostctl session start` is a person asking for a session,
        // with nothing queued to fill it — so it seeds, exactly as it
        // did before the switch's spawn had a reason not to.
        spawn_and_read_verdict(
            &bin.path,
            &cwd,
            seam.as_deref(),
            FirstProject::Seed,
            scaled(VERDICT_TIMEOUT),
        )
        .await
        .map_err(CliError::failed)?;
    let (pid, fresh) = match classify_verdict(&verdict) {
        StartStep::Confirm { pid, fresh } => (pid, fresh),
        StartStep::Failed(message) => return Err(CliError::Failed(message)),
    };

    let identity = confirm_serving(&socket, scaled(CONFIRM_TIMEOUT))
        .await
        .map_err(|e| {
            CliError::Connection(format!(
                "{BIN_NAME} reported `{verdict}` but no session answered at {}: {e:#}",
                socket.display()
            ))
        })?;

    let outcome = if fresh { "started" } else { "already-running" };
    if json {
        crate::print_json(&serde_json::json!({
            "outcome": outcome,
            "socket": socket.display().to_string(),
            "identity": identity,
            "launcher_reported_pid": pid,
        }))?;
        return Ok(0);
    }
    println!("{outcome}");
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

// ============================================================================
// stop
// ============================================================================

async fn stop(json: bool) -> Result<i32, CliError> {
    let socket = session_socket()?;

    let Some(report) = stop_session(&socket, scaled(STOP_CALL_TIMEOUT))
        .await
        .map_err(CliError::connection)?
    else {
        // Stop of a stopped session is a success, `systemctl stop`
        // style: the caller asked for a state, and it holds.
        if json {
            crate::print_json(&serde_json::json!({
                "socket": socket.display().to_string(),
                "session_id": null,
                "reap": null,
            }))?;
            return Ok(0);
        }
        println!("not running (no session at {})", socket.display());
        return Ok(0);
    };

    if !json {
        println!("stopping session {}", report.identity.session_id);
        print_reap_report(&report.reap);
    }

    // The socket is unlinked by a finalizer that runs *after* the reply
    // above, so the session is only really gone once this poll says so.
    await_stopped(
        &socket,
        &report.identity.session_id,
        scaled(STOP_GONE_TIMEOUT),
    )
    .await
    .map_err(CliError::failed)?;
    if json {
        crate::print_json(&serde_json::json!({
            "socket": socket.display().to_string(),
            "session_id": report.identity.session_id,
            "reap": report.reap,
        }))?;
        return Ok(0);
    }
    println!("stopped");
    Ok(0)
}

/// `session stop --old-path`: one session on a path this build no
/// longer uses, re-probed, named, and stopped only if it is still the
/// session it named.
///
/// Through the same gate the discovery runs behind, so a test-shaped
/// process can name only its own scratch candidate, and never this
/// build's own socket — that one is plain `session stop`'s.
async fn stop_old_path(socket: &Path, json: bool) -> Result<i32, CliError> {
    stop_old_path_among(
        socket,
        &old_session::Gate::from_process().candidates(),
        &session_socket()?,
        json,
    )
    .await
}

/// [`stop_old_path`] with the gate's answer and this build's own socket
/// handed in.
async fn stop_old_path_among(
    socket: &Path,
    gate: &old_session::Candidates,
    own_socket: &Path,
    json: bool,
) -> Result<i32, CliError> {
    if !socket.is_absolute() {
        return Err(CliError::Failed(format!(
            "--old-path must be an absolute socket path (got {})",
            socket.display()
        )));
    }
    gate.permit(socket).map_err(CliError::Failed)?;
    if old_session::same_socket(own_socket, socket) {
        return Err(CliError::Failed(format!(
            "{} is this build's own session socket; stop it with 'roostctl session stop'",
            socket.display()
        )));
    }

    let session_id = match old_session::probe_socket(socket).await {
        None => {
            if json {
                crate::print_json(&serde_json::json!({
                    "socket": socket.display().to_string(),
                    "session_id": null,
                    "reap": null,
                }))?;
            } else {
                println!("not running (no session at {})", socket.display());
            }
            return Ok(0);
        }
        Some(OldSession::Found {
            session_id: Some(id),
            ..
        }) => id,
        Some(old) => {
            return Err(CliError::Failed(format!(
                "can't identify the session at {}; stop it from its own build",
                old.describe()
            )))
        }
    };

    if !json {
        println!("stopping session {session_id} at {}", socket.display());
    }
    let report = match stop_session_if(socket, Some(&session_id), scaled(STOP_CALL_TIMEOUT))
        .await
        .map_err(CliError::connection)?
    {
        Stop::Stopped(report) => report,
        Stop::Gone => {
            if json {
                crate::print_json(&serde_json::json!({
                    "socket": socket.display().to_string(),
                    "session_id": session_id,
                    "reap": null,
                }))?;
            } else {
                println!("stopped (it went away first)");
            }
            return Ok(0);
        }
        Stop::NotExpected(other) => {
            return Err(CliError::Failed(format!(
                "the session at {} is now {other}, not {session_id}; nothing was stopped",
                socket.display()
            )))
        }
    };
    if !json {
        print_reap_report(&report.reap);
    }
    await_stopped(socket, &session_id, scaled(STOP_GONE_TIMEOUT))
        .await
        .map_err(CliError::failed)?;
    if json {
        crate::print_json(&serde_json::json!({
            "socket": socket.display().to_string(),
            "session_id": session_id,
            "reap": report.reap,
        }))?;
        return Ok(0);
    }
    println!("stopped");
    Ok(0)
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

async fn status(json: bool) -> Result<i32, CliError> {
    let socket = session_socket()?;
    let old = old_session::discover(&socket).await;

    if probe_gone(&socket).await {
        // The line stays on stdout as the status report it has always
        // been; the error is what carries the exit code.
        let (report, error) = not_running_report(&socket, &old);
        if json {
            crate::print_json(&report)?;
        } else {
            println!("not running (no session at {})", socket.display());
            print_old_paths(&old);
        }
        return Err(error);
    }

    let leg = scaled(IPC_TIMEOUT);
    let mut client = session_launch::dial(&socket, leg)
        .await
        .map_err(CliError::connection)?;
    // A socket that will not answer is a real fault, not a clean
    // "stopped" — say so and exit 1, not 3.
    let identity = session_launch::identify_on(&mut client, leg)
        .await
        .map_err(|e| {
            CliError::Connection(format!(
                "a socket exists at {} but no session answered: {e:#}",
                socket.display()
            ))
        })?;

    let list: TabListResult =
        tokio::time::timeout(leg, client.call(ops::TAB_LIST, serde_json::json!({})))
            .await
            .map_err(|_| CliError::Connection(format!("{} timed out", ops::TAB_LIST)))??;
    let tabs: usize = list.projects.iter().map(|p| p.tabs.len()).sum();

    if json {
        crate::print_json(&serde_json::json!({
            "socket": socket.display().to_string(),
            "identity": identity,
            "projects": list.projects.len(),
            "tabs": tabs,
            "old_paths": old_paths_json(&old),
        }))?;
        return Ok(0);
    }
    print_identity(&identity, &socket);
    println!("projects={}\ntabs={tabs}", list.projects.len());
    print_old_paths(&old);
    Ok(0)
}

/// Not running *here*: a session on an old path is not one this build
/// can reach, so it is named in the message rather than counted.
fn not_running(socket: &Path, old: &[OldSession]) -> CliError {
    let mut message = format!("no session at {}", socket.display());
    for old in old {
        message.push_str(&format!("; running at an old path: {}", old.describe()));
    }
    CliError::NotRunning(message)
}

/// `session status` with nothing at this build's socket: the `--json`
/// report — which, in the upgrade case, is where the session on an old
/// path is listed — and the exit-3 error beside it.
fn not_running_report(socket: &Path, old: &[OldSession]) -> (serde_json::Value, CliError) {
    let report = serde_json::json!({
        "socket": socket.display().to_string(),
        "identity": null,
        "old_paths": old_paths_json(old),
    });
    (report, not_running(socket, old))
}

fn print_old_paths(old: &[OldSession]) {
    for old in old {
        println!("running at an old path: {}", old.describe());
    }
}

fn old_paths_json(old: &[OldSession]) -> serde_json::Value {
    old.iter()
        .map(|old| match old {
            OldSession::Found {
                socket,
                protocol_version,
                pid,
                session_id,
            } => serde_json::json!({
                "socket": socket.display().to_string(),
                "session_protocol": protocol_version,
                "pid": pid,
                "session_id": session_id,
            }),
            OldSession::Unidentified { socket, reason } => serde_json::json!({
                "socket": socket.display().to_string(),
                "unidentified": reason,
            }),
        })
        .collect()
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

fn session_socket() -> Result<std::path::PathBuf, CliError> {
    BundleProfile::session()
        .map(|profile| profile.socket_path)
        .map_err(|e| CliError::Failed(format!("resolve the session socket path: {e:#}")))
}

#[cfg(test)]
pub(crate) mod fixtures {
    use std::path::{Path, PathBuf};

    /// An executable at `dir/name` that runs `body`.
    ///
    /// A symlink to the committed fixture plus `body` in a plain
    /// `<name>.conf` file beside it, never a script of our own: this
    /// binary's cases run in parallel threads, and an executable written
    /// while a sibling is forking races `execve` — the fork inherits our
    /// still-open write descriptor, and the exec answers ETXTBSY on
    /// Linux (#431). The fixture sources the conf, which has no such
    /// window, and pins the `PATH` the body runs under — see its header.
    pub(crate) fn fake_launcher(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(dir.join(format!("{name}.conf")), format!("{body}\n")).unwrap();
        std::os::unix::fs::symlink(launcher_fixture(), &path).unwrap();
        path
    }

    fn launcher_fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/roosttest/fixtures/fake-roost-session.sh")
            .canonicalize()
            .expect("the fake-roost-session fixture must exist")
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::fake_launcher;
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use roost_ipc::session_launch::{
        read_verdict_line, BinOrigin, LocatedBin, VerdictRead, MAX_VERDICT_BYTES,
    };

    /// A file that exists and is executable, in a directory of its own.
    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
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
        for expected in ["$ROOST_SESSION_BIN", "next to this program", "$PATH"] {
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
                IPC_TIMEOUT,
            ] {
                assert!(!budget.mul_f64(factor).is_zero(), "{raw} on {budget:?}");
            }
        }
    }

    #[test]
    fn the_ambient_scale_leaves_every_budget_usable() {
        for budget in [
            VERDICT_TIMEOUT,
            CONFIRM_TIMEOUT,
            STOP_CALL_TIMEOUT,
            STOP_GONE_TIMEOUT,
        ] {
            assert!(!scaled(budget).is_zero(), "{budget:?}");
        }
    }

    /// `session status` with nothing listening: exit 3 through the
    /// shared renderer, naming the socket it looked at.
    #[test]
    fn no_session_is_not_running_with_its_own_exit_code() {
        let error = not_running(Path::new("/run/roost/roost-session.sock"), &[]);
        assert_eq!(error.exit_code(), 3);
        assert_eq!(error.code(), "not-running");
        assert_eq!(
            error.render(false),
            "roostctl: not-running: no session at /run/roost/roost-session.sock\n"
        );
    }

    /// Plan 083 A-3: a session on an old path is reported, not counted —
    /// `status` still exits 3, and names every one it found.
    #[test]
    fn a_session_on_an_old_path_is_named_by_the_not_running_report() {
        let old = [
            OldSession::Found {
                socket: PathBuf::from("/tmp/old-a/roost.sock"),
                protocol_version: 6,
                pid: None,
                session_id: Some("s-a".into()),
            },
            OldSession::Unidentified {
                socket: PathBuf::from("/tmp/old-b/roost.sock"),
                reason: "no session.identify answer within 2s".into(),
            },
        ];
        let error = not_running(Path::new("/tmp/new/roost.sock"), &old);
        assert_eq!(error.exit_code(), 3);
        let rendered = error.render(false);
        for want in [
            "running at an old path: /tmp/old-a/roost.sock (session s-a, protocol 6)",
            "running at an old path: /tmp/old-b/roost.sock (not identified",
        ] {
            assert!(rendered.contains(want), "{rendered}");
        }
        let json = old_paths_json(&old);
        assert_eq!(json[0]["session_id"], "s-a");

        // `--json` lists them too, beside the same exit-3 error.
        let (report, error) = not_running_report(Path::new("/tmp/new/roost.sock"), &old);
        assert_eq!(error.exit_code(), 3);
        assert_eq!(report["socket"], "/tmp/new/roost.sock");
        assert_eq!(
            report["old_paths"].as_array().map(Vec::len),
            Some(2),
            "{report}"
        );
        assert_eq!(report["old_paths"][0]["socket"], "/tmp/old-a/roost.sock");
        assert_eq!(
            json[1]["unidentified"],
            "no session.identify answer within 2s"
        );
    }

    #[test]
    fn reap_ids_render_as_a_comma_list() {
        assert_eq!(join_ids(&[3, 1, 2]), "3,1,2");
        assert_eq!(join_ids(&[]), "");
    }

    // ------------------------------------------------------------------
    // `session stop --old-path`
    // ------------------------------------------------------------------

    /// A scratch session on an old path. Its first `session.identify`
    /// answers as `first`, every later one as `then` (`None`: no id at
    /// all), and a `session.stop` takes its socket away. Returns the ops
    /// it was sent.
    fn old_session(
        socket: &Path,
        first: Option<&'static str>,
        then: Option<&'static str>,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let dir = socket.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = std::sync::Arc::clone(&seen);
        let path = socket.to_path_buf();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let record = std::sync::Arc::clone(&record);
                let path = path.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                        let op = request["op"].as_str().unwrap().to_string();
                        let identifies = {
                            let mut seen = record.lock().unwrap();
                            seen.push(op.clone());
                            seen.iter()
                                .filter(|seen| *seen == ops::SESSION_IDENTIFY)
                                .count()
                        };
                        let result = if op == ops::SESSION_STOP {
                            let _ = std::fs::remove_file(&path);
                            serde_json::json!({"reaped": [], "killed": [], "abandoned": []})
                        } else {
                            let id = if identifies == 1 { first } else { then };
                            let mut identity =
                                serde_json::to_value(SessionIdentify::default()).unwrap();
                            identity["session_protocol"] = 6.into();
                            identity["session_id"] = id.unwrap_or("").into();
                            identity
                        };
                        let reply = serde_json::json!({
                            "id": request["id"], "ok": true, "result": result,
                        });
                        let mut bytes = serde_json::to_vec(&reply).unwrap();
                        bytes.push(b'\n');
                        if write.write_all(&bytes).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        seen
    }

    /// [`crate::tests::short_socket_dir`], removed however the test ends.
    struct ShortRoot(PathBuf);

    impl ShortRoot {
        fn new() -> Self {
            Self(crate::tests::short_socket_dir("old-path"))
        }

        fn join(&self, path: &str) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for ShortRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn only(socket: &Path) -> old_session::Candidates {
        old_session::Candidates::Test(socket.to_path_buf())
    }

    /// Plan 083 A-3: an old-path session that identifies is named and
    /// stopped over its own socket.
    #[tokio::test]
    async fn an_old_path_session_that_identifies_is_stopped_there() {
        let root = ShortRoot::new();
        let socket = root.join("old/roost.sock");
        let seen = old_session(&socket, Some("old-1"), Some("old-1"));
        let code = stop_old_path_among(&socket, &only(&socket), &root.join("new.sock"), true)
            .await
            .expect("stopped");
        assert_eq!(code, 0);
        assert_eq!(
            seen.lock().unwrap().last().map(String::as_str),
            Some(ops::SESSION_STOP)
        );
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn an_old_path_session_with_no_session_id_is_refused() {
        let root = ShortRoot::new();
        let socket = root.join("old/roost.sock");
        let seen = old_session(&socket, None, None);
        let error = stop_old_path_among(&socket, &only(&socket), &root.join("new.sock"), true)
            .await
            .expect_err("refused");
        let text = error.render(false);
        assert!(text.contains("can't identify"), "{text}");
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|op| op == ops::SESSION_STOP));
    }

    /// The id named before the stop is the only one stopped: a session
    /// that answers as another by the time the stop is sent is left.
    #[tokio::test]
    async fn a_session_replaced_between_identify_and_stop_is_left_running() {
        let root = ShortRoot::new();
        let socket = root.join("old/roost.sock");
        let seen = old_session(&socket, Some("old-1"), Some("someone-else"));
        let error = stop_old_path_among(&socket, &only(&socket), &root.join("new.sock"), true)
            .await
            .expect_err("refused");
        let text = error.render(false);
        assert!(text.contains("nothing was stopped"), "{text}");
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|op| op == ops::SESSION_STOP));
        assert!(socket.exists());
    }

    /// The gate decides which sockets may be named at all, and this
    /// build's own is never an old path.
    #[tokio::test]
    async fn only_a_socket_the_gate_permits_and_never_this_builds_own_is_stopped() {
        let root = ShortRoot::new();
        let socket = root.join("old/roost.sock");
        let seen = old_session(&socket, Some("old-1"), Some("old-1"));
        for gate in [
            old_session::Candidates::Closed("ROOST_TEST_MODE is set"),
            only(&root.join("another/roost.sock")),
        ] {
            stop_old_path_among(&socket, &gate, &root.join("new.sock"), true)
                .await
                .expect_err("not permitted");
        }
        stop_old_path_among(
            &socket,
            &old_session::Candidates::Production(Vec::new()),
            &root.join("new.sock"),
            true,
        )
        .await
        .expect_err("no candidates, nothing to name");
        stop_old_path_among(&socket, &only(&socket), &socket, true)
            .await
            .expect_err("this build's own socket");
        // However it is spelled.
        stop_old_path_among(
            &root.join("old/../old/roost.sock"),
            &only(&socket),
            &socket,
            true,
        )
        .await
        .expect_err("this build's own socket, spelled another way");
        stop_old_path_among(Path::new("relative.sock"), &only(&socket), &socket, true)
            .await
            .expect_err("relative");
        assert!(seen.lock().unwrap().is_empty(), "nothing was even probed");
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

    async fn run_launcher(tag: &str, body: &str) -> (anyhow::Result<Verdict>, Duration) {
        let dir = scratch(tag);
        let bin = fake_launcher(&dir, BIN_NAME, body);
        let started = std::time::Instant::now();
        let verdict =
            spawn_and_read_verdict(&bin, &dir, None, FirstProject::Seed, TEST_BUDGET).await;
        // Every case is about what the launcher did once it ran. A spawn
        // that never happened must not read as a launcher verdict: five
        // of these cases expect an `Err`, and without this four of them
        // fail on the text with no mention of the spawn while the fifth
        // passes outright.
        if let Err(error) = &verdict {
            assert!(
                !error.to_string().starts_with("spawn "),
                "the launcher was never run: {error:#}"
            );
        }
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
