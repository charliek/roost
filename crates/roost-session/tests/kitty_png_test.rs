//! A Kitty PNG query on a tab of the real `roost-session` binary. Its
//! `main` installs the PNG decoder, which the in-process `serve` the
//! other tests drive never runs, so this one starts the binary.
//!
//! The daemon runs under `ROOST_TEST_MODE=1` with a private
//! `ROOST_SESSION_SOCKET_DIR` and a scratch home and XDG set, built from
//! an empty environment.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use roost_ipc::paths::{SESSION_SOCKET_DIR_ENV, TEST_PASSWD_HOME_ENV, TEST_USER_RUNTIME_DIR_ENV};
use roost_ipc::session_launch;
use support::scaled;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// A 1x1 RGB PNG, base64.
const ONE_PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

fn owner_only(path: PathBuf) -> PathBuf {
    std::fs::create_dir_all(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// A foreground daemon over `root`, and its socket, once it says ready.
async fn start(root: &Path) -> (Child, PathBuf) {
    let home = owner_only(root.join("home"));
    let sockets = owner_only(root.join("ss"));
    let runtime = owner_only(root.join("rt"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_roost-session"));
    command
        .args(["start", "--foreground"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("SHELL", "/bin/sh")
        .env("ROOST_SHELL_FEATURES", "")
        .env("ROOST_TEST_MODE", "1")
        .env("HOME", &home)
        .env(TEST_PASSWD_HOME_ENV, &home)
        .env(SESSION_SOCKET_DIR_ENV, &sockets)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env(TEST_USER_RUNTIME_DIR_ENV, &runtime)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(scale) = std::env::var_os("ROOST_TEST_TIMEOUT_SCALE") {
        command.env("ROOST_TEST_TIMEOUT_SCALE", scale);
    }
    let mut child = command.spawn().expect("spawn roost-session start");
    let mut verdict = String::new();
    tokio::time::timeout(
        scaled(Duration::from_secs(30)),
        BufReader::new(child.stdout.take().expect("stdout")).read_line(&mut verdict),
    )
    .await
    .expect("a verdict within the budget")
    .expect("read the verdict");
    assert!(verdict.starts_with("ready pid="), "{verdict}");
    (child, sockets.join("roost.sock"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_png_query_is_answered_ok_exactly_once() {
    let tmp = tempfile::Builder::new()
        .prefix("roost-kitty-")
        .tempdir_in("/tmp")
        .expect("tempdir");
    // The daemon refuses a socket dir under a symlinked ancestor, and
    // macOS's /tmp is one.
    let root = support::canonical(tmp.path());
    let (mut daemon, socket) = start(&root).await;
    let mut client = support::connect(&socket).await;
    let project_id = support::tab_list(&mut client).await.projects[0].id;
    let tab = support::open_tab(
        &mut client,
        project_id,
        &root,
        "",
        &["/bin/sh", "-c", "exec sleep 300"],
    )
    .await;

    let query = format!("\x1b_Gi=32,a=q,t=d,f=100;{ONE_PIXEL_PNG}\x1b\\\x1b[c");
    support::feed_pty_bytes(&mut client, tab.id, query.as_bytes()).await;
    let deadline = support::deadline();
    let mut captured = Vec::new();
    while !String::from_utf8_lossy(&captured).ends_with('c') {
        assert!(
            std::time::Instant::now() < deadline,
            "no DA1 behind the answer: {:?}",
            String::from_utf8_lossy(&captured)
        );
        support::tick().await;
        captured.extend(support::capture_pty_input(&mut client, tab.id).await);
    }
    let text = String::from_utf8_lossy(&captured);
    let answer = text.split("\x1b[?").next().unwrap_or_default();
    assert_eq!(answer, "\x1b_Gi=32;OK\x1b\\", "{text:?}");

    drop(client);
    session_launch::stop_session(&socket, scaled(Duration::from_secs(60)))
        .await
        .expect("stop the session");
    tokio::time::timeout(scaled(Duration::from_secs(30)), daemon.wait())
        .await
        .expect("the daemon exits after its stop")
        .expect("reap");
}
