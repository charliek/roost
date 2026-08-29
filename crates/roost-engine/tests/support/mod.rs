//! Shared scaffolding for the PTY output-stream integration tests.
//!
//! `pty_seq_test.rs` pins the default pipeline and `tab_task_test.rs` its
//! server-VT twin. Both assert on the same stream contract, so the drain
//! and the contiguity check live here — a divergence between the two
//! copies would quietly weaken whichever one drifted.

#![allow(dead_code)]

use std::time::Duration;

use roost_engine::PtyOutputEvent;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

/// Drain a tab's raw output events until the channel closes, returning
/// them in receive order.
///
/// Closure is the condition, not a timer: every sender is dropped once
/// the reap task and the reader task have both finished, and the reap
/// task removes the session (the third holder) before either publishes
/// `Exit`. Waiting for it therefore captures trailing `Bytes` that
/// follow `Exit` on the deadline path, which a drain that stopped at
/// `Exit` would miss.
pub async fn collect_until_closed(
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
/// is send order and nothing lagged (`collect_until_closed` panics if it
/// did) — a gap or a repeat would mean the counter and the send came
/// apart.
pub fn assert_seqs_contiguous_from_one(events: &[PtyOutputEvent], label: &str) {
    assert!(!events.is_empty(), "{label}: no events at all");
    for (index, event) in events.iter().enumerate() {
        assert_eq!(
            event.seq(),
            index as u64 + 1,
            "{label}: wrong seq at event {index}"
        );
    }
}

/// Every `Bytes` payload concatenated, in receive order.
pub fn bytes_of(events: &[PtyOutputEvent]) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events {
        if let PtyOutputEvent::Bytes { data, .. } = event {
            out.extend_from_slice(data);
        }
    }
    out
}

/// [`bytes_of`] as text, for assertions on what the child printed.
pub fn text_of(events: &[PtyOutputEvent]) -> String {
    String::from_utf8_lossy(&bytes_of(events)).into_owned()
}
