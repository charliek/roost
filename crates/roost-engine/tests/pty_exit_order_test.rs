//! #255: a shell's trailing output must reach the tab before its exit.
//!
//! The supervisor used to publish `Exit` from the reap task the moment
//! `child.wait()` returned, while the reader task was still draining the
//! PTY — two producers on one broadcast channel, so a consumer that
//! stops at `Exit` (which `TabSession` does, and must) lost whatever the
//! reader had not flushed yet. Now the reader publishes `Exit` after its
//! final `Bytes`, so the ordering is structural.
//!
//! The other half of the exit path is session lifetime: the reap task's
//! identity-checked removal is the only thing that takes a session out
//! of the supervisor's map, so it must start after the session is in
//! there and must remove it before anyone learns the child exited.
//!
//! Every test here consumes through the production `TabSession`, i.e.
//! the same path the UIs use.

use std::sync::Arc;
use std::time::{Duration, Instant};

use roost_engine::session::{TabOutput, TabSession};
use roost_engine::{PtyError, PtySupervisor};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::sleep;

/// Clean runs of the multi-chunk ordering check the test demands. The
/// race it guards is timing-dependent, so a single pass proves little.
/// Doubles as the cap on retries after a lagging run (see below).
const ORDERING_RUNS: i64 = 20;
/// Lines of filler ahead of the sentinel: ~54 KiB through the PTY, or a
/// dozen-plus reads at the supervisor's 4 KiB chunk size. The original
/// bug needed output spanning several reads (CI's `env`) to show up at
/// all, so a one-read `echo` would pass this vacuously. Capped well
/// short of `PTY_OUTPUT_BROADCAST_CAPACITY` messages even when a loaded
/// machine makes each read return far less than a full chunk — see the
/// lag handling below.
const FILLER_LINES: usize = 2000;

/// Drain a tab's output until it reports `Exit`, returning the bytes
/// that arrived first, the exit status, and any drain-level error.
async fn drain_until_exit(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<TabOutput>,
    budget: Duration,
) -> (Vec<u8>, Option<i32>, Option<String>) {
    let deadline = Instant::now() + budget;
    let mut bytes = Vec::new();
    let mut status = None;
    let mut error = None;
    while Instant::now() < deadline && status.is_none() {
        match rx.try_recv() {
            Ok(TabOutput::Bytes(b) | TabOutput::Scanned { data: b, .. }) => {
                bytes.extend_from_slice(&b)
            }
            Ok(TabOutput::Exit { status: s, .. }) => status = Some(s),
            Ok(TabOutput::Error(e)) => error = Some(e),
            Err(TryRecvError::Empty) => sleep(Duration::from_millis(5)).await,
            Err(TryRecvError::Disconnected) => break,
        }
    }
    (bytes, status, error)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trailing_output_arrives_before_exit() {
    let socket = std::path::PathBuf::from("/tmp/roost-pty-exit-order.sock");
    let mut completed = 0;
    let mut lagged = 0;
    let mut tab_id = 600;
    while completed < ORDERING_RUNS {
        let run = completed;
        tab_id += 1;
        let sentinel = format!("ROOST_TAIL_SENTINEL_{tab_id}");
        // `exec` so the process that writes is the process that exits:
        // the sentinel is the last of a burst that is still sitting in
        // the tty buffer, unread, when the child is reaped. A trailing
        // `printf` from the shell instead would give the reader a head
        // start and let the race pass vacuously.
        let script = format!(
            "exec awk 'BEGIN{{for(i=1;i<={FILLER_LINES};i++) printf \"line %06d filler filler\\n\", i; \
             print \"{sentinel}\"}}'"
        );
        let sup = Arc::new(PtySupervisor::new());
        let pty_rx = sup
            .spawn(
                tab_id,
                "/tmp",
                &["/bin/sh".into(), "-c".into(), script],
                80,
                24,
                &socket,
            )
            .expect("spawn");
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let _session = TabSession::attach_with_receiver(sup.clone(), tab_id, pty_rx, out_tx, None);

        let (bytes, status, error) = drain_until_exit(&mut out_rx, Duration::from_secs(20)).await;
        sup.close(tab_id);

        // A lagging drain is the *other* truncation path — broadcast
        // capacity, not exit ordering — which this commit deliberately
        // leaves open (see `PTY_OUTPUT_BROADCAST_CAPACITY`). It shows up
        // when a loaded machine starves the drain task, and it would
        // fake a failure of the ordering assertion below, so retry the
        // run instead of reading anything into it.
        if let Some(err) = error {
            lagged += 1;
            assert!(
                lagged <= ORDERING_RUNS,
                "run {run}: every attempt lagged, ordering never got a clean run: {err}"
            );
            continue;
        }
        assert_eq!(status, Some(0), "run {run}: no clean Exit event");
        let text = String::from_utf8_lossy(&bytes);
        let tail = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(200)..]).into_owned();
        assert!(
            text.contains(&sentinel),
            "run {run}: the shell's last line was dropped before Exit; \
             captured {} bytes ending in:\n{tail}",
            bytes.len(),
        );
        completed += 1;
    }
}

/// A background descendant keeps the slave fd open, so the reader task
/// never reaches EOF — `read()` on the master blocks indefinitely. The
/// bounded fallback in `pty.rs` must publish `Exit` anyway, or the tab
/// would never report the shell's exit and auto-close would never fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_arrives_while_a_descendant_holds_the_pty_open() {
    let sup = Arc::new(PtySupervisor::new());
    let socket = std::path::PathBuf::from("/tmp/roost-pty-exit-grandchild.sock");
    let pty_rx = sup
        .spawn(
            700,
            "/tmp",
            &[
                "/bin/sh".into(),
                "-c".into(),
                // The `sleep` inherits the slave fds and outlives the
                // shell. Bounded so a failed pid parse below can't leave
                // the reader blocked for long (dropping the tokio runtime
                // waits on in-flight blocking tasks).
                "sleep 5 & printf 'ROOST_HOLDER_PID=%s\\n' $!; exit 0".into(),
            ],
            80,
            24,
            &socket,
        )
        .expect("spawn");
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
    let _session = TabSession::attach_with_receiver(sup.clone(), 700, pty_rx, out_tx, None);

    let started = Instant::now();
    let (bytes, status, _) = drain_until_exit(&mut out_rx, Duration::from_secs(10)).await;
    let elapsed = started.elapsed();
    sup.close(700);
    let text = String::from_utf8_lossy(&bytes).into_owned();

    // Assert the bound BEFORE signalling anything. `sleep 5` is only
    // guaranteed to still be alive while the drain stayed inside its
    // deadline; if the drain burned its full 10s budget the holder has
    // long exited and its pid may have been recycled onto an unrelated
    // process, so the SIGKILL below must not run.
    assert_eq!(
        status,
        Some(0),
        "Exit never arrived while a descendant held the PTY open; got:\n{text}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "Exit took {elapsed:?}, well past the bounded deadline"
    );

    // Bounded path only: release the PTY so the reader's blocking read
    // can return. On the failing path the assertions above already
    // panicked and `sleep 5` reaps itself.
    if let Some(pid) = text
        .split("ROOST_HOLDER_PID=")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse::<i32>().ok())
    {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// A child that exits essentially immediately must not leave its
/// session behind.
///
/// The reap task used to be spawned before `spawn()` promoted the
/// session into the supervisor's map. Its identity-checked removal
/// (#80) is the only thing that ever takes a session back out, so a
/// child reaped inside that window removed nothing and the promotion
/// then installed a session for an already-dead PTY: `has()` kept
/// answering yes and `write()` kept accepting input for a PTY nobody
/// was reading. The session is installed before the reap task starts
/// now, so every session has a waiter that will remove it.
///
/// The assertions are deterministic, not timing-based: the reap task
/// removes the session *before* it hands the status to the reader, so
/// observing `Exit` already implies the removal happened. What is not
/// deterministic is provoking the original race — it needs the child
/// reaped inside a promotion window microseconds wide, which is why the
/// loop runs the fast-exit path repeatedly rather than once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fast_child_leaves_no_session_behind() {
    let socket = std::path::PathBuf::from("/tmp/roost-pty-exit-fast-child.sock");
    let sup = Arc::new(PtySupervisor::new());
    for tab_id in 800..825 {
        let pty_rx = sup
            .spawn(
                tab_id,
                "/tmp",
                &["/bin/sh".into(), "-c".into(), "exit 7".into()],
                80,
                24,
                &socket,
            )
            .expect("spawn");
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let _session = TabSession::attach_with_receiver(sup.clone(), tab_id, pty_rx, out_tx, None);

        let (_, status, _) = drain_until_exit(&mut out_rx, Duration::from_secs(10)).await;
        assert_eq!(status, Some(7), "tab {tab_id}: no Exit event");
        assert!(
            !sup.has(tab_id),
            "tab {tab_id}: the session outlived its reaped child"
        );
        assert!(
            matches!(
                sup.write(tab_id, b"x".to_vec()).await,
                Err(PtyError::NotFound(id)) if id == tab_id
            ),
            "tab {tab_id}: write() accepted input for an exited PTY"
        );
    }
}
