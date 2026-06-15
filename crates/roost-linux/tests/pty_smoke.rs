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
    // `spawn` returns a Receiver subscribed BEFORE the reader task
    // starts producing — no risk of losing early bytes/exit events.
    let mut output = sup
        .spawn(
            7,
            "/tmp",
            &["/bin/sh".into(), "-c".into(), "printf 'hi\\n'".into()],
            80,
            24,
            &socket,
        )
        .expect("spawn");

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

    // Lifecycle channel should also have an Exit event. Use the
    // same 5s budget as the output polling above — on slow CI
    // runners the reap can land well after the byte stream
    // closes.
    let life_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut life_status = None;
    while std::time::Instant::now() < life_deadline {
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
    let mut output = sup
        .spawn(99, "/tmp", &["/usr/bin/env".into()], 80, 24, &socket)
        .expect("spawn");

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
    // Advertise OSC 8 hyperlink support so `supports-hyperlinks`-gated
    // CLIs (Claude Code et al.) emit clickable links instead of plain
    // text — "Roost" isn't on their TERM_PROGRAM allowlist.
    assert!(
        text.contains("FORCE_HYPERLINK=1"),
        "expected FORCE_HYPERLINK=1 in env, got:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_spawn_for_same_tab_id_is_rejected() {
    let sup = PtySupervisor::new();
    let socket = std::path::PathBuf::from("/tmp/roost-pty-dup.sock");
    let _first = sup
        .spawn(
            42,
            "/tmp",
            &["/bin/sh".into(), "-c".into(), "sleep 1".into()],
            80,
            24,
            &socket,
        )
        .expect("first spawn");

    let err = sup
        .spawn(
            42,
            "/tmp",
            &["/bin/sh".into(), "-c".into(), "true".into()],
            80,
            24,
            &socket,
        )
        .expect_err("duplicate spawn must error");
    // The error is anyhow-wrapped, so we walk the source chain to
    // assert the underlying PtyError variant rather than scraping
    // the Display string.
    let pty_err = err
        .downcast_ref::<roost_linux::daemon::PtyError>()
        .expect("expected PtyError in anyhow chain");
    assert!(
        matches!(pty_err, roost_linux::daemon::PtyError::DuplicateTab(42)),
        "expected DuplicateTab(42), got {pty_err:?}"
    );

    // Closing the original lets a subsequent spawn succeed.
    // `close()` removes the session from the map synchronously
    // before invoking the killer, so the slot is free immediately
    // — no `sleep()` needed. Pre-fix this test used a 50ms wait
    // that's flaky on slow CI runners; CR-flagged on PR #78.
    sup.close(42);
    let _second = sup
        .spawn(
            42,
            "/tmp",
            &["/bin/sh".into(), "-c".into(), "true".into()],
            80,
            24,
            &socket,
        )
        .expect("post-close spawn");
}

/// #80 A1: once the child exits, the wait task must remove the
/// session so a subsequent `write` returns `NotFound` rather than
/// silently succeeding against a dead PTY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_after_exit_returns_not_found() {
    let sup = PtySupervisor::new();
    let socket = std::path::PathBuf::from("/tmp/roost-pty-after-exit.sock");
    let mut output = sup
        .spawn(
            7,
            "/tmp",
            &["/bin/sh".into(), "-c".into(), "exit 0".into()],
            80,
            24,
            &socket,
        )
        .expect("spawn");

    // Wait for the child to exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match output.try_recv() {
            Ok(PtyOutputEvent::Exit(_)) => break,
            Ok(_) => {}
            Err(TryRecvError::Closed) => break, // senders gone → child exited
            Err(TryRecvError::Empty) => {
                assert!(std::time::Instant::now() < deadline, "child never exited");
                sleep(Duration::from_millis(20)).await;
            }
            Err(other) => panic!("output recv error: {other:?}"),
        }
    }

    // The wait task removes the dead session right after publishing
    // Exit; poll until the write is rejected with NotFound.
    let probe_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match sup.write(7, b"x".to_vec()).await {
            Err(roost_linux::daemon::PtyError::NotFound(7)) => break,
            other => {
                assert!(
                    std::time::Instant::now() < probe_deadline,
                    "write to a dead tab never became NotFound: {other:?}"
                );
                sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// #80 A2: a shell that ignores SIGHUP must still be reaped by
/// `close()`'s SIGKILL escalation. Without the fallback, `close()`
/// only delivers SIGHUP (portable-pty's cloned killer) and the child
/// would outlive it, so `TabExited` would never arrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_force_kills_sighup_ignoring_child() {
    let sup = PtySupervisor::new();
    let mut lifecycle = sup.subscribe_lifecycle();
    let socket = std::path::PathBuf::from("/tmp/roost-pty-sighup.sock");
    let _output = sup
        .spawn(
            7,
            "/tmp",
            &[
                "/bin/sh".into(),
                "-c".into(),
                "trap '' HUP; sleep 30".into(),
            ],
            80,
            24,
            &socket,
        )
        .expect("spawn");

    // Let the shell install the trap before we signal it.
    sleep(Duration::from_millis(200)).await;
    sup.close(7);

    // The SIGKILL fallback (~200ms grace) must reap the child well
    // inside this budget; without it the `sleep 30` would outlast it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        match lifecycle.try_recv() {
            Ok(SupervisorEvent::TabExited { tab_id: 7, .. }) => {
                exited = true;
                break;
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) => sleep(Duration::from_millis(20)).await,
            Err(other) => panic!("lifecycle recv error: {other:?}"),
        }
    }
    assert!(
        exited,
        "SIGHUP-ignoring child was not force-killed within the grace window"
    );
}
