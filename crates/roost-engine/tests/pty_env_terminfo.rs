//! TERMINFO hygiene: Roost forces TERM=xterm-256color, so an inherited
//! TERMINFO (the launching terminal's private DB — e.g. Ghostty's, which
//! has no xterm-256color entry) must be stripped from the child env.
//!
//! Lives in its own integration-test binary because it mutates the
//! process environment: every other test file spawns PTYs (which read the
//! environment) from parallel test threads, and POSIX env mutation is not
//! thread-safe. A separate file means a separate process with only this
//! one test in it.

use std::time::{Duration, Instant};

use roost_engine::pty::{PtyOutputEvent, PtySupervisor};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::sleep;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inherited_terminfo_is_stripped_from_child_env() {
    std::env::set_var("TERMINFO", "/tmp/roost-test-terminfo");

    let sup = PtySupervisor::new();
    let socket = std::path::PathBuf::from("/tmp/roost-pty-terminfo.sock");
    let mut output = sup
        .spawn(11, "/tmp", &["/usr/bin/env".into()], 80, 24, &socket)
        .expect("spawn");

    // Same budget-bounded drain rationale as pty_smoke's
    // collect_until_closed: content assertions below are what prove the
    // capture wasn't truncated.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    let mut exit_status = None;
    while Instant::now() < deadline {
        match output.try_recv() {
            Ok(PtyOutputEvent::Bytes(bytes)) => collected.extend_from_slice(&bytes),
            Ok(PtyOutputEvent::Exit(status)) => exit_status = Some(status),
            Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Empty) => sleep(Duration::from_millis(50)).await,
            Err(TryRecvError::Lagged(dropped)) => {
                panic!("output receiver lagged; {dropped} message(s) dropped")
            }
        }
    }

    assert_eq!(exit_status, Some(0), "expected clean exit");
    let text = String::from_utf8_lossy(&collected);
    assert!(
        text.contains("ROOST_TAB_ID=11"),
        "expected a complete env capture, got:\n{text}"
    );
    assert!(
        !text.contains("TERMINFO="),
        "expected inherited TERMINFO stripped from child env, got:\n{text}"
    );
}
