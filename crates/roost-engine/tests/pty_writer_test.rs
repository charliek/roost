//! The PTY writer against a child that stops reading (#409).
//!
//! On Linux a master `write(2)` parked on a full slave input buffer is
//! never released — not by the child dying, not by the slave closing —
//! so a writer that blocked there held a runtime worker for good and the
//! runtime could not shut down. The writer now waits on the reactor
//! instead. These tests are featureless: the writer loop is shared by
//! the plain reader path and the server-VT tab task alike.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use roost_engine::{PtyOutputEvent, PtySupervisor};
use tokio::sync::broadcast;
use tokio::time::timeout;

fn socket() -> PathBuf {
    PathBuf::from("/tmp/roost-pty-writer.sock")
}

fn spawn_tab(
    sup: &PtySupervisor,
    tab_id: i64,
    script: &str,
) -> broadcast::Receiver<PtyOutputEvent> {
    sup.spawn(
        tab_id,
        "/tmp",
        &["/bin/sh".into(), "-c".into(), script.into()],
        80,
        24,
        &socket(),
    )
    .expect("spawn")
}

/// Wait for the child's readiness byte, so nothing is written before
/// its `stty` has run.
async fn wait_ready(rx: &mut broadcast::Receiver<PtyOutputEvent>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, rx.recv()).await {
            Ok(Ok(PtyOutputEvent::Bytes { data, .. })) if data.contains(&b'R') => return Ok(()),
            Ok(Ok(PtyOutputEvent::Bytes { .. })) => continue,
            other => return Err(format!("waiting for the child's R, got {other:?}")),
        }
    }
}

/// A writer stuck on a child that never reads must not hold the runtime
/// hostage: `close()` hangs the child up, and the runtime then shuts
/// down promptly.
///
/// The runtime is built by hand so its shutdown is the observable.
/// `shutdown_timeout` stops waiting at its deadline and leaks whatever
/// is still parked, so on the old blocking writer this takes the full
/// five seconds on Linux (macOS released the write with EIO and passed
/// either way). The body returns a `Result` rather than panicking: a
/// panic inside `block_on` would drop the runtime and wait on that
/// parked worker instead of failing. The two-second bound also relies on
/// the `KILL_GRACE` watchdog being a plain thread the pool never waits
/// on, which it is.
#[test]
fn a_parked_writer_never_holds_the_runtime_shutdown() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let outcome: Result<usize, String> = rt.block_on(async {
        let sup = PtySupervisor::new();
        let mut rx = spawn_tab(&sup, 1, "stty -icanon -echo; printf R; exec sleep 30");
        wait_ready(&mut rx).await?;

        // `sleep` never reads, so the kernel buffer fills, the writer
        // stalls mid-chunk, and the 64-slot command channel backs up
        // behind it until a send no longer completes. That is the state
        // under test; ~133 sends reach it on Linux (68 KiB of kernel
        // buffer plus the channel), ~66 on macOS. The channel alone
        // absorbs 64 sends whether or not the writer ever ran, so a
        // stall before the 65th proves nothing about the writer and
        // fails the test rather than passing it.
        const CHANNEL_SLOTS: usize = 64;
        let chunk = vec![b'x'; 1024];
        let mut sent = 0usize;
        loop {
            if sent == 400 {
                return Err(format!("the writer never stalled after {sent} KiB"));
            }
            match timeout(Duration::from_millis(200), sup.write(1, chunk.clone())).await {
                Ok(Ok(())) => sent += 1,
                Ok(Err(err)) => return Err(format!("write failed after {sent} KiB: {err}")),
                Err(_) if sent <= CHANNEL_SLOTS => {
                    return Err(format!(
                        "a send stalled after {sent} KiB, before the writer had drained anything"
                    ))
                }
                Err(_) => break,
            }
        }
        sup.close(1);
        Ok(sent)
    });

    let start = Instant::now();
    rt.shutdown_timeout(Duration::from_secs(5));
    let took = start.elapsed();
    let sent = outcome.expect("setup");
    assert!(
        took < Duration::from_secs(2),
        "runtime shutdown took {took:?} with {sent} KiB queued: a writer is still parked in write(2)"
    );
}

/// A payload larger than the slave's input buffer, written while the
/// child is not yet reading, arrives complete and in order once it does.
/// Every byte past the first stall goes through the writer's partial
/// write path; nothing else in the tree exercises it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payload_past_the_pty_buffer_arrives_intact() {
    let sup = PtySupervisor::new();
    let mut rx = spawn_tab(&sup, 2, "stty raw -echo; printf R; sleep 1; exec cat");
    // Close on every exit path: a failed assertion that left `cat`
    // alive would keep the slave open, the reader thread in `poll`, and
    // the runtime drop waiting on it — a hang where a red run belongs.
    struct CloseOnDrop<'a>(&'a PtySupervisor, i64);
    impl Drop for CloseOnDrop<'_> {
        fn drop(&mut self) {
            self.0.close(self.1);
        }
    }
    let _close = CloseOnDrop(&sup, 2);
    wait_ready(&mut rx).await.expect("ready");

    let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    sup.write(2, payload.clone())
        .await
        .expect("queue the payload");

    let mut got = Vec::with_capacity(payload.len());
    let deadline = Instant::now() + Duration::from_secs(20);
    while got.len() < payload.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, rx.recv()).await {
            Ok(Ok(PtyOutputEvent::Bytes { data, .. })) => got.extend_from_slice(&data),
            Ok(Ok(PtyOutputEvent::Exit { .. })) => {
                panic!(
                    "cat exited with {} of {} bytes echoed",
                    got.len(),
                    payload.len()
                )
            }
            Ok(Err(err)) => panic!("broadcast: {err}"),
            Err(_) => panic!(
                "timed out with {} of {} bytes echoed",
                got.len(),
                payload.len()
            ),
        }
    }
    assert_eq!(got.len(), payload.len(), "cat echoed more than it was sent");
    assert!(got == payload, "the echoed bytes differ from the payload");
}
