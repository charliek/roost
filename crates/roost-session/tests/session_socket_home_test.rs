//! Plan 083 M2 through the real binary: where `roost-session start` binds
//! by default, what it refuses to bind, and what it refuses to run
//! beside.
//!
//! Every daemon here runs under `ROOST_TEST_MODE=1` with either a private
//! `ROOST_SESSION_SOCKET_DIR` or a scratch `ROOST_TEST_PASSWD_HOME`, so a
//! case that lost its isolation fails closed on the product's own guard
//! instead of reaching this user's real session — and a release build,
//! which ignores the scratch home, fails closed the same way. The one
//! exception is the CI-only case, which runs only on a GitHub runner.

#![cfg(target_os = "linux")]

mod support;

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use roost_ipc::paths::{SESSION_SOCKET_DIR_ENV, TEST_OLD_SESSION_DIR_ENV, TEST_PASSWD_HOME_ENV};
use roost_ipc::session_launch;
use support::{old_path_sentinel, scaled};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

/// The debug build's own namespace: these tests run the binary built
/// beside them.
const NS: &str = "roost-session-dev";

/// `/tmp`, short: every socket here must fit `sun_path`.
fn scratch() -> TempDir {
    tempfile::Builder::new()
        .prefix("r083-")
        .tempdir_in("/tmp")
        .expect("tempdir")
}

fn owner_only_dir(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path.to_path_buf()
}

/// `roost-session <args>` with nothing inherited but what is named: a
/// stray `XDG_RUNTIME_DIR`, `HOME` or session knob from the test's own
/// environment must not decide where it binds.
fn session(args: &[&str], env: &[(&str, &OsStr)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_roost-session"));
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("SHELL", "/bin/sh")
        .env("ROOST_SHELL_FEATURES", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(scale) = std::env::var_os("ROOST_TEST_TIMEOUT_SCALE") {
        command.env("ROOST_TEST_TIMEOUT_SCALE", scale);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    command
}

/// A foreground daemon and its one verdict line.
async fn start(env: &[(&str, &OsStr)]) -> (Child, String) {
    let mut child = session(&["start", "--foreground"], env)
        .spawn()
        .expect("spawn roost-session start");
    let stdout: ChildStdout = child.stdout.take().expect("stdout");
    let mut line = String::new();
    tokio::time::timeout(
        scaled(Duration::from_secs(30)),
        BufReader::new(stdout).read_line(&mut line),
    )
    .await
    .expect("a verdict within the budget")
    .expect("read the verdict");
    (child, line.trim().to_string())
}

/// Stop the daemon over `socket` and reap it.
async fn stop(mut child: Child, socket: &Path) {
    session_launch::stop_session(socket, scaled(Duration::from_secs(60)))
        .await
        .expect("stop the session");
    tokio::time::timeout(scaled(Duration::from_secs(30)), child.wait())
        .await
        .expect("the daemon exits after its stop")
        .expect("reap");
}

async fn identify(socket: &Path) -> roost_ipc::messages::SessionIdentify {
    session_launch::identify(socket, scaled(Duration::from_secs(10)))
        .await
        .unwrap_or_else(|error| panic!("nothing answered at {}: {error:#}", socket.display()))
}

fn default_socket(home: &Path) -> PathBuf {
    home.join(".local/state")
        .join(NS)
        .join("run")
        .join("roost.sock")
}

/// Plan 083 A-1, end to end: a daemon started with no `XDG_RUNTIME_DIR`,
/// another `$HOME` and no `XDG_STATE_HOME`, and a client — the bridge —
/// that has all three set, meet at one socket: the passwd home's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_and_a_client_that_disagree_on_home_and_xdg_meet_at_the_passwd_home() {
    if !cfg!(debug_assertions) {
        eprintln!("skip: the scratch passwd home is honoured by debug builds only");
        return;
    }
    let root = scratch();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let data = root.path().join("data");

    let (daemon, verdict) = start(&[
        ("HOME", elsewhere.as_os_str()),
        (TEST_PASSWD_HOME_ENV, home.as_os_str()),
        ("XDG_DATA_HOME", data.as_os_str()),
        ("ROOST_TEST_MODE", OsStr::new("1")),
    ])
    .await;
    assert!(verdict.starts_with("ready pid="), "{verdict}");
    let socket = default_socket(&home);
    let served = identify(&socket).await;
    let run = socket.parent().unwrap();
    assert_eq!(
        std::fs::metadata(run).unwrap().permissions().mode() & 0o7777,
        0o700
    );
    assert!(
        !elsewhere.join(".local/state").join(NS).join("run").exists(),
        "$HOME moved the socket"
    );

    let runtime = owner_only_dir(&root.path().join("rt"));
    let state = root.path().join("state");
    let mut bridge = session(
        &["client-bridge"],
        &[
            ("HOME", home.as_os_str()),
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("XDG_STATE_HOME", state.as_os_str()),
            ("XDG_DATA_HOME", data.as_os_str()),
            (TEST_PASSWD_HOME_ENV, home.as_os_str()),
            ("ROOST_TEST_MODE", OsStr::new("1")),
        ],
    )
    .stdin(Stdio::piped())
    .spawn()
    .expect("spawn the bridge");
    let mut stdin = bridge.stdin.take().unwrap();
    stdin
        .write_all(b"{\"id\":\"1\",\"op\":\"session.identify\",\"params\":{}}\n")
        .await
        .unwrap();
    let mut answer = String::new();
    tokio::time::timeout(
        scaled(Duration::from_secs(10)),
        BufReader::new(bridge.stdout.take().unwrap()).read_line(&mut answer),
    )
    .await
    .expect("the bridge answers")
    .unwrap();
    let answer: serde_json::Value = serde_json::from_str(&answer).expect("a response line");
    assert_eq!(
        answer["result"]["session_id"], served.session_id,
        "{answer}"
    );
    drop(stdin);
    let _ = bridge.wait().await;

    stop(daemon, &socket).await;
}

/// A GitHub-hosted runner: a VM thrown away after the job, so its real
/// passwd home holds no session anyone lives in. `CI` and
/// `GITHUB_ACTIONS` alone are also what a self-hosted runner on a
/// developer's own machine sets; `RUNNER_ENVIRONMENT=github-hosted` is
/// GitHub's word that it is not one of those. The `rust` job's
/// `ubuntu-latest` leg runs this file's `cargo test` on one.
fn on_a_disposable_runner(getenv: &dyn Fn(&str) -> Option<String>) -> bool {
    getenv("CI").as_deref() == Some("true")
        && getenv("GITHUB_ACTIONS").as_deref() == Some("true")
        && getenv("RUNNER_ENVIRONMENT").as_deref() == Some("github-hosted")
}

#[test]
fn only_a_github_hosted_runner_counts_as_disposable() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    };
    const HOSTED: &[(&str, &str)] = &[
        ("CI", "true"),
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
    ];
    assert!(on_a_disposable_runner(&env(HOSTED)));
    for not_disposable in [
        &[("CI", "true"), ("GITHUB_ACTIONS", "true")][..],
        &[
            ("CI", "true"),
            ("GITHUB_ACTIONS", "true"),
            ("RUNNER_ENVIRONMENT", "self-hosted"),
        ],
        &[
            ("GITHUB_ACTIONS", "true"),
            ("RUNNER_ENVIRONMENT", "github-hosted"),
        ],
        &[],
    ] {
        assert!(
            !on_a_disposable_runner(&env(not_disposable)),
            "{not_disposable:?}"
        );
    }
}

/// Plan 083 A-1's CI-only half: no scratch home, no test mode — the
/// real passwd home of a disposable runner, the case the knob stands in
/// for everywhere else. Never anywhere a developer's own session lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_ci_runner_the_daemon_binds_under_the_real_passwd_home() {
    if !on_a_disposable_runner(&|name| std::env::var(name).ok()) || !cfg!(debug_assertions) {
        eprintln!("skip: binds under this user's real home; GitHub-hosted runners only");
        return;
    }
    // SAFETY: `getpwuid` returns static storage or null, read at once.
    let home = unsafe {
        let entry = libc::getpwuid(libc::getuid());
        assert!(!entry.is_null());
        PathBuf::from(
            std::ffi::CStr::from_ptr((*entry).pw_dir)
                .to_string_lossy()
                .into_owned(),
        )
    };
    let root = scratch();
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let data = root.path().join("data");
    let (daemon, verdict) = start(&[
        ("HOME", elsewhere.as_os_str()),
        ("XDG_DATA_HOME", data.as_os_str()),
    ])
    .await;
    assert!(verdict.starts_with("ready pid="), "{verdict}");
    let socket = default_socket(&home);
    identify(&socket).await;
    stop(daemon, &socket).await;
}

/// Plan 083 D-A4: a socket file left at the new path by a session that
/// is gone — bound, its listener dropped, the file kept, as a reboot
/// leaves one — is replaced by the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_socket_at_the_new_path_is_replaced() {
    if !cfg!(debug_assertions) {
        eprintln!("skip: the scratch passwd home is honoured by debug builds only");
        return;
    }
    let root = scratch();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    roost_ipc::session_dir::prepare_session_socket_dir(&home.join(".local/state"), NS)
        .expect("the run dir");
    let socket = default_socket(&home);
    drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind the previous boot's socket"));
    assert!(socket.exists(), "the stale file is left behind");

    let (daemon, verdict) = start(&[
        ("HOME", home.as_os_str()),
        (TEST_PASSWD_HOME_ENV, home.as_os_str()),
        ("ROOST_TEST_MODE", OsStr::new("1")),
    ])
    .await;
    assert!(verdict.starts_with("ready pid="), "{verdict}");
    identify(&socket).await;
    stop(daemon, &socket).await;
}

/// Plan 083 A-3: a session answering on an old path stops a start
/// before its locks — this one has a state dir of its own, so no lock
/// would ever have met it — and the refusal names the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_refuses_while_a_session_answers_on_an_old_path() {
    let root = scratch();
    let old = owner_only_dir(&root.path().join("old"));
    let old_socket = old.join("roost.sock");
    let _sentinel = old_path_sentinel(&old_socket);
    let new = root.path().join("new");
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();

    let (mut daemon, verdict) = start(&[
        ("HOME", home.as_os_str()),
        (SESSION_SOCKET_DIR_ENV, new.as_os_str()),
        (TEST_OLD_SESSION_DIR_ENV, old.as_os_str()),
        ("ROOST_TEST_MODE", OsStr::new("1")),
    ])
    .await;
    if !cfg!(debug_assertions) {
        // A release build ignores the test candidate: no probe, a start.
        stop(daemon, &new.join("roost.sock")).await;
        return;
    }
    assert!(verdict.starts_with("error: "), "{verdict}");
    assert!(
        verdict.contains(&format!("--old-path {}", old_socket.display())),
        "{verdict}"
    );
    assert!(verdict.contains("old-1"), "{verdict}");
    let status = daemon.wait().await.expect("reap");
    assert!(!status.success(), "{status}");
    assert!(!new.join("roost.sock").exists(), "nothing was bound");
    assert!(
        !new.join("roost.lock").exists(),
        "refused before the locks were taken"
    );
}

/// Plan 083 A-3: a state lock someone holds, with no socket this build
/// can find for them, is reported with the holder the kernel names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_state_lock_with_no_socket_names_its_holder() {
    let root = scratch();
    let state = owner_only_dir(&root.path().join("state"));
    let _held = roost_engine::single_instance::acquire(state.join("state.lock"))
        .expect("hold the state lock");
    // A pid in the file that is not the holder's: only the kernel's own
    // table can name who holds it.
    std::fs::write(state.join("state.lock"), "999999\n").unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (mut daemon, verdict) = start(&[
        ("HOME", home.as_os_str()),
        (SESSION_SOCKET_DIR_ENV, root.path().join("new").as_os_str()),
        ("ROOST_STATE_DIR", state.as_os_str()),
        ("ROOST_TEST_MODE", OsStr::new("1")),
    ])
    .await;
    assert!(verdict.starts_with("error: "), "{verdict}");
    assert!(
        verdict.contains(&format!(
            "a session holds the state lock; its socket wasn't found (pid {}",
            std::process::id()
        )),
        "{verdict}"
    );
    assert!(!daemon.wait().await.expect("reap").success());
}

/// Plan 083 D-A2: a socket directory something else created at the
/// umask's `0755` first is refused by a start, on both paths — the
/// override's and the default's — naming it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_dir_made_at_0755_is_refused_on_both_paths() {
    let root = scratch();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();

    let overridden = root.path().join("ovr");
    std::fs::create_dir(&overridden).unwrap();
    std::fs::set_permissions(&overridden, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (mut daemon, verdict) = start(&[
        ("HOME", home.as_os_str()),
        (SESSION_SOCKET_DIR_ENV, overridden.as_os_str()),
        ("ROOST_TEST_MODE", OsStr::new("1")),
    ])
    .await;
    assert!(verdict.starts_with("error: "), "{verdict}");
    assert!(
        verdict.contains(&overridden.display().to_string()),
        "{verdict}"
    );
    assert!(verdict.contains("0755"), "{verdict}");
    assert!(!daemon.wait().await.expect("reap").success());

    if !cfg!(debug_assertions) {
        return;
    }
    let run = default_socket(&home).parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(
        run.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (mut daemon, verdict) = start(&[
        ("HOME", home.as_os_str()),
        (TEST_PASSWD_HOME_ENV, home.as_os_str()),
        ("ROOST_TEST_MODE", OsStr::new("1")),
    ])
    .await;
    assert!(verdict.starts_with("error: "), "{verdict}");
    assert!(verdict.contains(&run.display().to_string()), "{verdict}");
    assert!(verdict.contains(SESSION_SOCKET_DIR_ENV), "{verdict}");
    assert!(!daemon.wait().await.expect("reap").success());
}
