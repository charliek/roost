//! PTY supervisor smoke test. Spawns `/bin/sh -c "echo hi"` and
//! confirms the byte and exit signals propagate.

use std::time::Duration;

use roost_linux::daemon::{PtyOutputEvent, PtySupervisor, SupervisorEvent};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::sleep;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pty_echo_emits_bytes_and_exit() {
    let sup = PtySupervisor::new();
    let mut lifecycle = sup.subscribe_lifecycle();
    let socket = std::path::PathBuf::from("/tmp/roost-pty-smoke.sock");
    sup.spawn(
        7,
        "/tmp",
        &["/bin/sh".into(), "-c".into(), "printf 'hi\\n'".into()],
        80,
        24,
        &socket,
    )
    .expect("spawn");

    // Subscribe to output AFTER spawn; some output may arrive
    // before we subscribe — that's fine, the exit event is what
    // we rely on for completion.
    let mut output = sup.subscribe_output(7).expect("subscribe");

    // Pump until we see Exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_bytes = false;
    let mut exit_status: Option<i32> = None;
    while std::time::Instant::now() < deadline {
        match output.try_recv() {
            Ok(PtyOutputEvent::Bytes(_)) => saw_bytes = true,
            Ok(PtyOutputEvent::Exit(status)) => {
                exit_status = Some(status);
                break;
            }
            Err(TryRecvError::Empty) => sleep(Duration::from_millis(50)).await,
            Err(other) => panic!("output recv error: {other:?}"),
        }
    }
    assert!(saw_bytes, "expected at least one chunk of PTY output");
    assert_eq!(exit_status, Some(0), "expected clean exit");

    // Lifecycle channel should also have an Exit event.
    let mut life_status = None;
    for _ in 0..20 {
        match lifecycle.try_recv() {
            Ok(SupervisorEvent::TabExited { tab_id: 7, status }) => {
                life_status = Some(status);
                break;
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) => sleep(Duration::from_millis(50)).await,
            Err(other) => panic!("lifecycle recv error: {other:?}"),
        }
    }
    assert_eq!(life_status, Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pty_injects_roost_env_vars() {
    // Spawn `env` and capture its output. Should see ROOST_TAB_ID
    // and ROOST_SOCKET in the listed env.
    let sup = PtySupervisor::new();
    let socket = std::path::PathBuf::from("/tmp/roost-pty-env.sock");
    sup.spawn(99, "/tmp", &["/usr/bin/env".into()], 80, 24, &socket)
        .expect("spawn");

    let mut output = sup.subscribe_output(99).expect("subscribe");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    while std::time::Instant::now() < deadline {
        match output.try_recv() {
            Ok(PtyOutputEvent::Bytes(b)) => collected.extend_from_slice(&b),
            Ok(PtyOutputEvent::Exit(_)) => break,
            Err(TryRecvError::Empty) => sleep(Duration::from_millis(50)).await,
            Err(other) => panic!("output recv error: {other:?}"),
        }
    }
    let text = String::from_utf8_lossy(&collected);
    assert!(
        text.contains("ROOST_TAB_ID=99"),
        "expected ROOST_TAB_ID in env, got:\n{text}"
    );
    assert!(
        text.contains("ROOST_SOCKET=/tmp/roost-pty-env.sock"),
        "expected ROOST_SOCKET in env, got:\n{text}"
    );
}
