//! A spawned tab's real environment (plan 085 D1): exactly its opener's
//! opener-set variables whatever this process carries, the passwd shell
//! as `SHELL` rather than this process's `$SHELL`, the derived
//! `XDG_RUNTIME_DIR`, and Roost's own variables over any inherited ones.
//!
//! Its own integration-test binary for the reason `pty_env_terminfo.rs`
//! is: it sets this process's environment, which every other test that
//! spawns a PTY reads from parallel threads.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::time::{Duration, Instant};

use roost_engine::pty::{PtyOutputEvent, PtySupervisor};
use roost_engine::TabOpener;
use roost_ipc::opener::OPENER_SET;
use roost_ipc::paths::TEST_USER_RUNTIME_DIR_ENV;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::sleep;

const STARTER_SHELL: &str = "/nonexistent/starter-shell";
const STARTER_RUNTIME_DIR: &str = "/nonexistent/starter-run";

async fn child_env(
    sup: &PtySupervisor,
    tab_id: i64,
    opener: &TabOpener,
) -> BTreeMap<String, String> {
    let socket = std::path::PathBuf::from("/tmp/roost-pty-env-opener.sock");
    let mut output = sup
        .spawn(
            tab_id,
            "/tmp",
            &["/usr/bin/env".into()],
            200,
            24,
            &socket,
            opener,
        )
        .expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    let mut exit_status = None;
    while Instant::now() < deadline && exit_status.is_none() {
        match output.try_recv() {
            Ok(PtyOutputEvent::Bytes { data, .. }) => collected.extend_from_slice(&data),
            Ok(PtyOutputEvent::Grid { .. }) => {}
            Ok(PtyOutputEvent::Exit { code, .. }) => exit_status = Some(code),
            Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Empty) => sleep(Duration::from_millis(50)).await,
            Err(TryRecvError::Lagged(dropped)) => {
                panic!("output receiver lagged; {dropped} message(s) dropped")
            }
        }
    }
    assert_eq!(exit_status, Some(0), "expected a clean exit");
    let text = String::from_utf8_lossy(&collected);
    let env: BTreeMap<String, String> = text
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    assert_eq!(
        env.get("ROOST_TAB_ID").map(String::as_str),
        Some(tab_id.to_string().as_str()),
        "expected a complete env capture, got:\n{text}"
    );
    env
}

fn opener_set(env: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    env.iter()
        .filter(|(name, _)| OPENER_SET.contains(&name.as_str()))
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

fn passwd_shell() -> String {
    // SAFETY: `getpwuid` returns a pointer into static storage this
    // single-threaded read copies out of before anything else calls it.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        assert!(!entry.is_null(), "this uid has no passwd entry");
        CStr::from_ptr((*entry).pw_shell)
            .to_string_lossy()
            .into_owned()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tab_gets_its_openers_variables_and_none_of_this_processs() {
    let runtime_dir = tempfile::tempdir().unwrap();
    for name in OPENER_SET {
        std::env::set_var(name, format!("starter-{name}"));
    }
    std::env::set_var("SHELL", STARTER_SHELL);
    std::env::set_var("XDG_RUNTIME_DIR", STARTER_RUNTIME_DIR);
    std::env::set_var(TEST_USER_RUNTIME_DIR_ENV, runtime_dir.path());
    // The knob in a release-profile test build too.
    std::env::set_var("ROOST_TEST_MODE", "1");
    std::env::set_var("TERM", "dumb");
    std::env::set_var("ROOST_TAB_ID", "999");
    std::env::set_var("ROOST_SOCKET", "/nonexistent/elsewhere.sock");
    let expected_runtime_dir = if cfg!(target_os = "macos") {
        STARTER_RUNTIME_DIR.to_string()
    } else {
        runtime_dir.path().to_string_lossy().into_owned()
    };

    let sup = PtySupervisor::new();
    let local = TabOpener::Local(
        [("DISPLAY", ":77"), ("SSH_AUTH_SOCK", "")]
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .into(),
    );
    let remote = TabOpener::Remote {
        ssh_connection: Some("10.9.8.7 51000 10.1.2.3 22".into()),
        ssh_client: None,
    };
    for (tab_id, opener, expected) in [
        (
            21,
            local,
            BTreeMap::from([("DISPLAY", ":77"), ("SSH_AUTH_SOCK", "")]),
        ),
        (
            22,
            remote,
            BTreeMap::from([("SSH_CONNECTION", "10.9.8.7 51000 10.1.2.3 22")]),
        ),
        (23, TabOpener::None, BTreeMap::new()),
    ] {
        let env = child_env(&sup, tab_id, &opener).await;
        assert_eq!(opener_set(&env), expected, "{opener:?}");
        assert_eq!(env["SHELL"], passwd_shell(), "{opener:?}: SHELL");
        assert_eq!(
            env["XDG_RUNTIME_DIR"], expected_runtime_dir,
            "{opener:?}: XDG_RUNTIME_DIR"
        );
        assert_eq!(env["TERM"], "xterm-256color");
        assert_eq!(env["ROOST_SOCKET"], "/tmp/roost-pty-env-opener.sock");
    }
}
