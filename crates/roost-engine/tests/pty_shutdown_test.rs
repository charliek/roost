//! `PtySupervisor::shutdown_all`: tear every PTY down, account for every
//! tab, and never outlive the deadline by more than the SIGKILL tail.
//!
//! The report is keyed on the session map — the per-spawn wait task
//! removes a tab's entry when `child.wait()` returns — rather than on the
//! lifecycle broadcast, whose capacity is 64. `every_tab_is_accounted_for_
//! past_the_lifecycle_channel_capacity` is the test that pins the
//! difference: with more simultaneous exits than the channel holds, a
//! report built from events would lose tabs, and one built from the map
//! cannot.
//!
//! Children are `exec`'d so the PTY's direct child is the only process
//! holding the slave fd. A surviving descendant would keep the reader
//! task blocked on `read()`, and dropping the tokio runtime waits on
//! in-flight blocking tasks — the test would hang instead of failing.

use std::time::{Duration, Instant};

use roost_engine::{PtyError, PtyOutputEvent, PtySupervisor, ShutdownReport};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::time::sleep;

/// A shell that dies on SIGHUP, as one process (no descendant to hold
/// the PTY open).
const COOPERATIVE: &str = "exec sleep 100";
/// A shell that ignores SIGHUP and stays ignoring it across `exec`
/// (POSIX: an ignored disposition survives the new process image), so
/// only SIGKILL ends it. `printf` first so the test can wait for the
/// trap to be installed before hanging the tab up — a SIGHUP that
/// arrives during startup would kill it and make the tab look
/// cooperative.
const SIGHUP_IMMUNE: &str = "trap '' HUP; printf R; exec sleep 100";

fn socket() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/roost-pty-shutdown.sock")
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

/// Wait for the child's readiness byte, so the shutdown that follows
/// cannot race the shell's own startup.
async fn wait_ready(rx: &mut broadcast::Receiver<PtyOutputEvent>, tab_id: i64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(PtyOutputEvent::Bytes { data, .. }) if data.contains(&b'R') => return,
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Empty) => sleep(Duration::from_millis(5)).await,
        }
    }
    panic!("tab {tab_id} never signalled readiness");
}

/// The report's three vectors must partition the ids that were live when
/// shutdown started: every id exactly once, none invented, none dropped.
/// A duplicate shows up as a length mismatch against the target list.
fn assert_partitions(report: &ShutdownReport, targets: &[i64]) {
    let mut seen: Vec<i64> = report
        .reaped
        .iter()
        .chain(&report.killed)
        .chain(&report.abandoned)
        .copied()
        .collect();
    seen.sort_unstable();
    let mut expected = targets.to_vec();
    expected.sort_unstable();
    assert_eq!(
        seen, expected,
        "every live tab must appear exactly once in {report:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cooperative_children_are_all_reaped() {
    let sup = PtySupervisor::new();
    let targets: Vec<i64> = (100..104).collect();
    let _rx: Vec<_> = targets
        .iter()
        .map(|id| spawn_tab(&sup, *id, COOPERATIVE))
        .collect();

    let report = sup.shutdown_all(Duration::from_secs(10)).await;

    assert_eq!(report.reaped, targets, "SIGHUP alone should suffice");
    assert!(
        report.killed.is_empty(),
        "no SIGKILL was needed: {report:?}"
    );
    assert!(report.abandoned.is_empty(), "{report:?}");
    assert_partitions(&report, &targets);
    for id in &targets {
        assert!(!sup.has(*id), "tab {id} outlived shutdown");
    }
}

/// A child that ignores SIGHUP is still gone when shutdown returns, and
/// the report says how: `killed`, disjoint from `reaped`.
///
/// The deadline is deliberately shorter than the per-tab `KILL_GRACE`
/// watchdog (200ms) so the escalation under test is the one
/// `shutdown_all` performs, not the one `terminate_child` already spawns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sighup_immune_child_is_sigkilled_and_reported_killed() {
    let sup = PtySupervisor::new();
    let mut rx = spawn_tab(&sup, 200, SIGHUP_IMMUNE);
    wait_ready(&mut rx, 200).await;

    let report = sup.shutdown_all(Duration::from_millis(50)).await;

    assert_eq!(report.killed, vec![200], "{report:?}");
    assert!(
        report.reaped.is_empty(),
        "killed and reaped must be disjoint: {report:?}"
    );
    assert!(
        report.abandoned.is_empty(),
        "SIGKILL should have landed well inside the tail: {report:?}"
    );
    assert_partitions(&report, &[200]);
    assert!(!sup.has(200));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_supervisor_reports_nothing_immediately() {
    let sup = PtySupervisor::new();
    let started = Instant::now();

    let report = sup.shutdown_all(Duration::from_secs(30)).await;

    assert_eq!(report, ShutdownReport::default());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "nothing to wait for, yet shutdown took {:?}",
        started.elapsed()
    );
}

/// Deadline semantics: shutdown waits out the full soft deadline for a
/// child that will not exit, then escalates and returns inside the
/// SIGKILL tail. It must neither cut the cooperative window short nor
/// wait unbounded on a child that ignores the hangup.
///
/// Deadline under `KILL_GRACE` again, for the same reason as the test
/// above: past 200ms the per-tab watchdog would do the killing and the
/// tab would report as `reaped`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_returns_between_the_deadline_and_the_kill_tail() {
    let sup = PtySupervisor::new();
    let mut rx = spawn_tab(&sup, 300, SIGHUP_IMMUNE);
    wait_ready(&mut rx, 300).await;

    let deadline = Duration::from_millis(100);
    let started = Instant::now();
    let report = sup.shutdown_all(deadline).await;
    let elapsed = started.elapsed();

    assert_eq!(report.killed, vec![300], "{report:?}");
    assert!(
        elapsed >= deadline,
        "shutdown escalated before the deadline expired: {elapsed:?}"
    );
    // Deadline + the 500ms post-SIGKILL tail, with slack for a loaded
    // machine. The bound that matters is that it is bounded at all.
    assert!(
        elapsed < deadline + Duration::from_secs(5),
        "shutdown ran past its deadline plus the kill tail: {elapsed:?}"
    );
}

/// The other half of the escalation story: `killed` means *shutdown*
/// escalated. Given a deadline past the per-tab `KILL_GRACE` watchdog,
/// the same SIGHUP-immune child is force-killed by `terminate_child`'s
/// own watchdog and reported as `reaped` — the deadline never expired,
/// so shutdown never sent a signal of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generous_deadline_lets_the_per_tab_watchdog_do_the_killing() {
    let sup = PtySupervisor::new();
    let mut rx = spawn_tab(&sup, 350, SIGHUP_IMMUNE);
    wait_ready(&mut rx, 350).await;

    let report = sup.shutdown_all(Duration::from_secs(10)).await;

    assert_eq!(report.reaped, vec![350], "{report:?}");
    assert!(report.killed.is_empty(), "{report:?}");
    assert_partitions(&report, &[350]);
}

/// The lifecycle broadcast holds 64 events; this exits more tabs than
/// that at once, so any subscriber is free to lag. The report is built
/// from session-map removals, so lagging costs latency and nothing else
/// — every id must still be accounted for exactly once.
///
/// The test cannot black-box observe a `Lagged` — that is the point of
/// the design, not a gap in the test. What it pins is the consequence:
/// at this scale a report derived from the channel would lose tabs, and
/// one derived from the map cannot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_tab_is_accounted_for_past_the_lifecycle_channel_capacity() {
    let sup = PtySupervisor::new();
    let targets: Vec<i64> = (400..470).collect();
    let _rx: Vec<_> = targets
        .iter()
        .map(|id| spawn_tab(&sup, *id, COOPERATIVE))
        .collect();

    let report = sup.shutdown_all(Duration::from_secs(30)).await;

    assert_partitions(&report, &targets);
    assert_eq!(
        report.reaped.len(),
        targets.len(),
        "every tab should have been reaped cooperatively: {report:?}"
    );
    assert!(report.abandoned.is_empty(), "{report:?}");
    for id in &targets {
        assert!(!sup.has(*id), "tab {id} outlived shutdown");
    }
}

/// The no-more-spawns latch is permanent: once shutdown has walked the
/// session map, a tab that acquired a PTY afterwards would never be torn
/// down by anyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_is_rejected_once_shutdown_has_started() {
    let sup = PtySupervisor::new();
    let _rx = spawn_tab(&sup, 500, COOPERATIVE);

    let report = sup.shutdown_all(Duration::from_secs(10)).await;
    assert_eq!(report.reaped, vec![500]);

    let err = sup
        .spawn(
            501,
            "/tmp",
            &["/bin/sh".into(), "-c".into(), COOPERATIVE.into()],
            80,
            24,
            &socket(),
        )
        .expect_err("spawn after shutdown must be refused");
    let pty_err = err
        .downcast_ref::<PtyError>()
        .expect("expected PtyError in anyhow chain");
    assert!(
        matches!(pty_err, PtyError::ShuttingDown(501)),
        "unexpected error: {pty_err}"
    );
    assert!(!sup.has(501), "a refused spawn must leave no session");
}

/// `close()` frees its slot synchronously — it removes the session entry
/// and lets the waiter reap in the background. That is exactly the
/// signal `shutdown_all` reads as "this child was reaped", so a close
/// racing a shutdown would report a still-running child as reaped. While
/// the latch is set, close() must send its hangup and leave the entry
/// for the waiter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_during_shutdown_leaves_the_entry_for_the_waiter() {
    let sup = std::sync::Arc::new(PtySupervisor::new());
    let mut rx = spawn_tab(&sup, 900, SIGHUP_IMMUNE);
    wait_ready(&mut rx, 900).await;

    let shutdown = tokio::spawn({
        let sup = std::sync::Arc::clone(&sup);
        async move { sup.shutdown_all(Duration::from_secs(10)).await }
    });
    // The latch is set first thing in `shutdown_all`, and the child
    // ignores SIGHUP — so it cannot be reaped before the per-tab
    // watchdog fires at 200ms. The entry is still there to protect.
    sleep(Duration::from_millis(30)).await;
    sup.close(900);
    assert!(
        sup.has(900),
        "close() dropped a live child's entry mid-shutdown; shutdown would call it reaped"
    );

    let report = shutdown.await.expect("shutdown task");
    assert_partitions(&report, &[900]);
    assert!(
        !sup.has(900),
        "the waiter should have removed the entry: {report:?}"
    );
}

/// The latch alone is not enough, because a spawn can be *past* it: it
/// reserved its slot before shutdown started and is still building its
/// PTY when the sweep snapshots the session map. If it then installed
/// that session, nothing would ever tear the child down — the sweep has
/// already walked past, and the caller believes it owns a live tab.
///
/// So every racer must end in exactly one of two states: refused with
/// `ShuttingDown`, or present in exactly one bucket of the report. A
/// session that survives in the map without appearing in the report is
/// the leak.
///
/// A zero deadline is what opens the window: the wait for in-flight
/// spawns gives up before its first poll, so the sweep snapshots the map
/// with racers still inside `openpty`/`fork`. A deadline of even a few
/// milliseconds lets every spawn finish first and the race never
/// happens (verified: this test passes against the unfixed promotion
/// path when the drain is allowed to succeed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawns_racing_the_sweep_never_leak_a_session() {
    for round in 0..6i64 {
        let sup = std::sync::Arc::new(PtySupervisor::new());
        let base = 600 + round * 100;
        // A few tabs already live, so the sweep has real work to do and
        // shutdown does not finish before the racers get moving.
        let _rx: Vec<_> = (base..base + 3)
            .map(|id| spawn_tab(&sup, id, COOPERATIVE))
            .collect();

        let racers: Vec<_> = (base + 10..base + 18)
            .map(|id| {
                let sup = std::sync::Arc::clone(&sup);
                // `spawn` blocks on openpty + fork, so it belongs on the
                // blocking pool rather than a runtime worker.
                tokio::task::spawn_blocking(move || {
                    let result = sup.spawn(
                        id,
                        "/tmp",
                        &["/bin/sh".into(), "-c".into(), COOPERATIVE.into()],
                        80,
                        24,
                        &socket(),
                    );
                    (id, result.err().map(|err| err.to_string()))
                })
            })
            .collect();

        // Long enough for the racers to have reserved their slots and be
        // inside the PTY build, short enough that most have not promoted.
        sleep(Duration::from_millis(2)).await;
        let report = sup.shutdown_all(Duration::ZERO).await;

        for racer in racers {
            let (id, err) = racer.await.expect("racer task");
            match err {
                Some(err) => assert!(
                    err.contains("shutting down"),
                    "racer {id} failed for the wrong reason: {err}"
                ),
                None => {
                    let buckets = [&report.reaped, &report.killed, &report.abandoned]
                        .iter()
                        .filter(|bucket| bucket.contains(&id))
                        .count();
                    assert_eq!(
                        buckets, 1,
                        "racer {id} spawned successfully but shutdown never swept it: {report:?}"
                    );
                }
            }
        }
        // A tab still in the map is only acceptable if shutdown said so.
        for id in base..base + 18 {
            assert!(
                !sup.has(id) || report.abandoned.contains(&id),
                "round {round}: tab {id} outlived shutdown unreported: {report:?}"
            );
        }
    }
}
