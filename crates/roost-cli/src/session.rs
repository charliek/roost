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
//! ([`locate_session_binary`], [`classify_verdict`], and the stop poll's
//! own `stop_completed` next to it in [`session_launch`]) are pure and
//! table-tested; the I/O around them is thin.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;

use roost_ipc::messages::{ops, SessionIdentify, SessionStopResult, TabListResult};
use roost_ipc::paths::BundleProfile;
use roost_ipc::session_launch::{
    self, await_stopped, confirm_serving, locate_session_binary, probe_gone,
    spawn_and_read_verdict, stop_session, timeout_scale, Verdict, BIN_ENV, BIN_NAME, IPC_TIMEOUT,
};
use roost_ipc::IpcClient;

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

// ============================================================================
// stop
// ============================================================================

async fn stop() -> Result<i32> {
    let socket = BundleProfile::session()
        .context("resolve the session socket path")?
        .socket_path;

    let Some(report) = stop_session(&socket, scaled(STOP_CALL_TIMEOUT)).await? else {
        // Stop of a stopped session is a success, `systemctl stop`
        // style: the caller asked for a state, and it holds.
        println!("not running (no session at {})", socket.display());
        return Ok(0);
    };

    println!("stopping session {}", report.identity.session_id);
    print_reap_report(&report.reap);

    // The socket is unlinked by a finalizer that runs *after* the reply
    // above, so the session is only really gone once this poll says so.
    await_stopped(
        &socket,
        &report.identity.session_id,
        scaled(STOP_GONE_TIMEOUT),
    )
    .await?;
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
    session_launch::dial(socket, scaled(IPC_TIMEOUT)).await
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

async fn identify_on(client: &mut IpcClient) -> Result<SessionIdentify> {
    session_launch::identify_on(client, scaled(IPC_TIMEOUT)).await
}

#[cfg(test)]
mod tests {
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
