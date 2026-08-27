//! Per-spawn sequence numbers on `PtyOutputEvent`.
//!
//! Two producers publish onto a tab's output channel: the reader loop
//! (`Bytes`, plus `Exit` on the normal EOF path) and the reap task's
//! deadline backstop (`Exit` when the reader never reaches EOF). Seq
//! assignment and the broadcast send happen together under one lock, so
//! the numbers a subscriber sees are in send order — that is the
//! property these tests pin, since a bare fetch-add would let a producer
//! reserve a number and then lose the send race to the other one.
//!
//! Every assertion is on the raw receiver `spawn` returns, which is
//! subscribed before the reader task starts, so the first event a test
//! sees is genuinely the spawn's first.

use std::time::Duration;

use roost_engine::{PtyOutputEvent, PtySupervisor};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

fn is_exit(event: &PtyOutputEvent) -> bool {
    matches!(event, PtyOutputEvent::Exit { .. })
}

fn text_of(events: &[PtyOutputEvent]) -> String {
    let mut bytes = Vec::new();
    for event in events {
        if let PtyOutputEvent::Bytes { data, .. } = event {
            bytes.extend_from_slice(data);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Drain a tab's raw output events until the channel closes, returning
/// them in receive order.
///
/// Closure is the condition, not a timer: every sender is dropped once
/// the reap task and the reader task have both finished, and the reap
/// task removes the session (the third holder) before either publishes
/// `Exit`. Waiting for it therefore captures trailing `Bytes` that
/// follow `Exit` on the deadline path, which a drain that stopped at
/// `Exit` would miss.
async fn collect_until_closed(
    output: &mut broadcast::Receiver<PtyOutputEvent>,
    budget: Duration,
) -> Vec<PtyOutputEvent> {
    let mut events = Vec::new();
    loop {
        match timeout(budget, output.recv()).await {
            Ok(Ok(event)) => events.push(event),
            Ok(Err(RecvError::Closed)) => return events,
            // A dropped message is a legitimate seq gap, so it would
            // turn the contiguity assertions below into noise.
            Ok(Err(RecvError::Lagged(dropped))) => {
                panic!("output receiver lagged; {dropped} message(s) dropped")
            }
            Err(_) => panic!(
                "pty output channel never closed; got {} event(s) so far",
                events.len()
            ),
        }
    }
}

/// The honest invariant: seqs start at 1 for the spawn and advance by
/// exactly one per event in the order they were received. Contiguity is
/// stronger than "strictly increasing" and holds because receive order
/// is send order and nothing lagged (the helper panics if it did) — a
/// gap or a repeat would mean the counter and the send came apart.
fn assert_seqs_contiguous_from_one(events: &[PtyOutputEvent], label: &str) {
    assert!(!events.is_empty(), "{label}: no events at all");
    for (index, event) in events.iter().enumerate() {
        assert_eq!(
            event.seq(),
            index as u64 + 1,
            "{label}: wrong seq at event {index}"
        );
    }
}

/// Normal EOF path: the reader publishes every `Bytes`, then `Exit`
/// last, with `Exit` taking the next ordinal rather than repeating the
/// last `Bytes` seq.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seqs_advance_and_exit_carries_the_highest_on_the_eof_path() {
    let sup = PtySupervisor::new();
    let socket = std::path::PathBuf::from("/tmp/roost-pty-seq-eof.sock");
    let mut output = sup
        .spawn(
            900,
            "/tmp",
            &[
                "/bin/sh".into(),
                "-c".into(),
                "printf 'ROOST_SEQ_ONE\\n'; printf 'ROOST_SEQ_TWO\\n'; exit 0".into(),
            ],
            80,
            24,
            &socket,
        )
        .expect("spawn");

    let events = collect_until_closed(&mut output, Duration::from_secs(10)).await;
    sup.close(900);

    let text = text_of(&events);
    assert!(
        text.contains("ROOST_SEQ_ONE") && text.contains("ROOST_SEQ_TWO"),
        "expected the shell's output, got:\n{text}"
    );
    assert_seqs_contiguous_from_one(&events, "eof path");

    let last = events.last().expect("at least one event");
    assert!(
        is_exit(last),
        "expected Exit last on the EOF path, got {last:?}"
    );
    assert_eq!(
        last.seq(),
        events.len() as u64,
        "Exit must carry its own ordinal, the highest of the spawn"
    );
    assert_eq!(
        events.iter().filter(|e| is_exit(e)).count(),
        1,
        "exactly one Exit per spawn"
    );
}

/// Competing producers: the reap task's deadline backstop publishing
/// `Exit` while the reader is still publishing `Bytes`.
///
/// A SIGHUP-ignoring background subshell inherits the slave fd and
/// outlives the shell, then writes a full second later — an eternity
/// past `EXIT_PUBLISH_GRACE`. On Linux the master stays readable, so the
/// reader never reaches EOF, the backstop publishes `Exit`, and the
/// subshell's bytes follow it. macOS revokes the tty when the session
/// leader exits (the descendant survives but its writes to the tty
/// fail), so the same script collapses to the ordinary EOF shape there.
///
/// Both shapes are legal — `session.rs` drops what follows `Exit`, which
/// is the deliberate trade `pty_exit_order_test.rs` pins — so the
/// assertions are on what must hold either way: one `Exit` per spawn,
/// and seqs contiguous from 1 in receive order across every producer.
/// The shape is asserted too, so a platform silently switching paths
/// shows up as a failure rather than as a quietly weaker test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seqs_stay_ordered_when_the_backstop_races_the_reader() {
    let sup = PtySupervisor::new();
    let socket = std::path::PathBuf::from("/tmp/roost-pty-seq-backstop.sock");
    // The subshell exits after its write, releasing the slave fd, so the
    // reader finishes and the channel closes on its own — nothing is
    // left holding the PTY open and no pid has to be killed.
    let mut output = sup
        .spawn(
            901,
            "/tmp",
            &[
                "/bin/sh".into(),
                "-c".into(),
                "{ trap '' HUP; sleep 1; printf 'ROOST_SEQ_TRAILING\\n'; } & \
                 printf 'ROOST_SEQ_FIRST\\n'; exit 0"
                    .into(),
            ],
            80,
            24,
            &socket,
        )
        .expect("spawn");

    let events = collect_until_closed(&mut output, Duration::from_secs(20)).await;
    sup.close(901);

    let text = text_of(&events);
    assert!(
        text.contains("ROOST_SEQ_FIRST"),
        "expected the shell's own output, got:\n{text}"
    );
    assert_seqs_contiguous_from_one(&events, "backstop path");
    assert_eq!(
        events.iter().filter(|e| is_exit(e)).count(),
        1,
        "the reader and the backstop must arbitrate to exactly one Exit"
    );

    let exit_index = events
        .iter()
        .position(is_exit)
        .expect("the tab must report its exit");
    let after_exit = events.len() - exit_index - 1;
    if cfg!(target_os = "linux") {
        assert!(
            after_exit > 0 && text.contains("ROOST_SEQ_TRAILING"),
            "expected the backstop shape — Exit published on the deadline with the \
             descendant's bytes following it — got {} event(s) after Exit:\n{text}",
            after_exit
        );
    } else {
        assert_eq!(
            after_exit, 0,
            "expected the EOF shape on a revoking tty, got {after_exit} event(s) after Exit"
        );
    }
}

/// A respawn is a new session with its own counter, not a continuation
/// of the dead one's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_respawned_tab_gets_a_fresh_counter() {
    let sup = PtySupervisor::new();
    let socket = std::path::PathBuf::from("/tmp/roost-pty-seq-respawn.sock");
    for run in 0..2 {
        let mut output = sup
            .spawn(
                902,
                "/tmp",
                &[
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf 'ROOST_SEQ_RESPAWN\\n'; exit 0".into(),
                ],
                80,
                24,
                &socket,
            )
            .unwrap_or_else(|err| panic!("run {run}: spawn failed: {err:?}"));

        let events = collect_until_closed(&mut output, Duration::from_secs(10)).await;
        // The reap task removed the session before publishing Exit, so
        // the slot is already free; close() is belt-and-braces.
        sup.close(902);

        // Contiguity from 1 is the whole assertion: run 1 restarting at
        // 1 is what "fresh counter" means, and it is only meaningful
        // because run 0 pushed the counter past 1.
        assert_seqs_contiguous_from_one(&events, &format!("run {run}"));
        assert!(
            events.len() > 1,
            "run {run} produced a single event, so a continued counter would be invisible"
        );
    }
}
