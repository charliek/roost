//! The OSC opt-in, exercised through the REAL forwarding task.
//!
//! The drain used to answer OSC color queries itself. It no longer
//! does: libghostty answers OSC 4 / 10 / 11 / 12 (and Kitty OSC 21)
//! through the `write_pty` effect as of the pinned ghostty
//! `f2d5758f6`, and Roost installs that callback for its device-query
//! replies — so a drain-side answer put a SECOND reply on the wire for
//! every query. That is what these tests now guard: the drain scans,
//! tracks colors and forwards actions, and enqueues NOTHING onto the
//! PTY input channel.
//!
//! The observation point is the one `tab.capture_pty_input` reads, and
//! bytes enter through a synthetic `PtyOutputEvent` broadcast — the
//! same receiver the supervisor hands out. There is no terminal here,
//! so anything in the capture came from the drain.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use roost_engine::osc::{OscAction, OscColorSnapshot, OscRgb};
use roost_engine::session::{InputCapture, TabOutput, TabSession};
use roost_engine::{PtyOutputEvent, PtySupervisor};
use tokio::sync::broadcast;

const FOREGROUND: OscRgb = (0xff, 0xff, 0xff);
const BACKGROUND: OscRgb = (0x1e, 0x1e, 0x1e);
const CURSOR: OscRgb = (0x98, 0x98, 0x9d);

fn theme_seed() -> OscColorSnapshot {
    let mut palette = [(0, 0, 0); 256];
    palette[5] = (0xde, 0xad, 0xbe);
    OscColorSnapshot::new(FOREGROUND, BACKGROUND, CURSOR, palette)
}

struct Harness {
    session: TabSession,
    pty_tx: broadcast::Sender<PtyOutputEvent>,
    output_rx: tokio::sync::mpsc::UnboundedReceiver<TabOutput>,
    capture: InputCapture,
}

/// Attach a session to a synthetic PTY broadcast. No PTY is spawned:
/// nothing here writes to one, and the capture buffer records every
/// enqueue, which is the observation point `tab.capture_pty_input`
/// reads.
fn harness(tab_id: i64, scanned: bool) -> Harness {
    let supervisor = Arc::new(PtySupervisor::new());
    let (pty_tx, pty_rx) = broadcast::channel(64);
    let (output_tx, output_rx) = tokio::sync::mpsc::unbounded_channel();
    let capture: InputCapture = Arc::new(Mutex::new(Vec::new()));
    let session = TabSession::attach_with_receiver_scanned(
        supervisor,
        tab_id,
        pty_rx,
        output_tx,
        Some(capture.clone()),
        scanned.then(theme_seed),
    );
    Harness {
        session,
        pty_tx,
        output_rx,
        capture,
    }
}

impl Harness {
    fn emit(&self, bytes: &[u8]) {
        self.pty_tx
            .send(PtyOutputEvent::Bytes(bytes.to_vec()))
            .expect("the forwarding task is subscribed");
    }

    fn captured(&self) -> Vec<u8> {
        self.capture.lock().unwrap().clone()
    }

    /// Give a would-be drain-side enqueue every chance to appear, then
    /// assert the channel stayed empty. The wait is generous because a
    /// false green here is the failure mode that matters: the reply
    /// that regressed CI arrived milliseconds after the chunk.
    async fn assert_never_enqueues_anything(&self) {
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            let captured = self.captured();
            assert!(
                captured.is_empty(),
                "the drain must not answer color queries — libghostty does: {:?}",
                String::from_utf8_lossy(&captured)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn next_output(&mut self) -> TabOutput {
        tokio::time::timeout(Duration::from_secs(5), self.output_rx.recv())
            .await
            .expect("output arrives")
            .expect("the forwarding task is alive")
    }
}

/// (a) The regression guard for the duplicate reply: a color query
/// reaching the drain produces no PTY input at all. The query bytes
/// still travel to the UI, which writes them to the terminal — that is
/// where the one answer comes from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_color_query_is_not_answered_by_the_drain() {
    let mut harness = harness(901, true);
    harness.emit(b"\x1b]11;?\x07\x1b]10;?\x07\x1b]12;?\x07\x1b]4;5;?\x07");
    match harness.next_output().await {
        TabOutput::Scanned { data, actions } => {
            assert_eq!(
                data,
                b"\x1b]11;?\x07\x1b]10;?\x07\x1b]12;?\x07\x1b]4;5;?\x07"
            );
            assert_eq!(actions, Vec::<OscAction>::new());
        }
        other => panic!("expected Scanned, got {other:?}"),
    }
    harness.assert_never_enqueues_anything().await;
}

/// The chunk reaches the UI with the bytes verbatim and the actions the
/// scan produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scanned_output_forwards_the_bytes_and_the_actions() {
    let mut harness = harness(902, true);
    harness.emit(b"\x1b]11;?\x07\x1b]0;title\x07hello");
    match harness.next_output().await {
        TabOutput::Scanned { data, actions } => {
            assert_eq!(data, b"\x1b]11;?\x07\x1b]0;title\x07hello");
            assert_eq!(
                actions,
                vec![OscAction::Workspace {
                    command: 0,
                    payload: "title".into(),
                }],
                "the query produces nothing; the title action survives"
            );
        }
        other => panic!("expected Scanned, got {other:?}"),
    }
}

/// A color SET is likewise silent on the input channel — libghostty
/// applies it from the same bytes, and the drain only moves its own
/// color state (whose semantics are pinned in
/// `roost_engine::osc`'s unit tests).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_followed_by_a_query_stays_silent_across_chunks() {
    let mut harness = harness(903, true);
    harness.emit(b"\x1b]11;rgb:00/11/22\x07");
    harness.next_output().await;
    harness.emit(b"\x1b]11;?\x07");
    harness.next_output().await;
    harness.assert_never_enqueues_anything().await;
}

/// A theme change re-seeds the drain-local state while the forwarding
/// task is live. It shares one lock with the scan, so this pins that
/// the two interleave without deadlocking or losing a chunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reseeding_does_not_disturb_the_forwarding_task() {
    let mut harness = harness(905, true);
    harness.session.reseed_osc_colors(OscColorSnapshot::new(
        (0x11, 0x22, 0x33),
        (0x44, 0x55, 0x66),
        (0x77, 0x88, 0x99),
        [(0, 0, 0); 256],
    ));
    harness.emit(b"\x1b]0;after-reseed\x07");
    match harness.next_output().await {
        TabOutput::Scanned { actions, .. } => assert_eq!(
            actions,
            vec![OscAction::Workspace {
                command: 0,
                payload: "after-reseed".into(),
            }]
        ),
        other => panic!("expected Scanned, got {other:?}"),
    }
}

/// `tab.feed_pty_bytes`'s route: injected bytes run the SAME router and
/// state as the drain, so they share the scanner's streaming position —
/// and, like the drain, enqueue nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_bytes_share_the_drain_router_and_state() {
    let harness = harness(906, true);
    let actions = harness
        .session
        .scan_osc(b"\x1b]11;rgb:00/11/22\x07\x1b]7;file:///tmp\x07\x1b]11;?\x07");
    assert_eq!(
        actions,
        vec![OscAction::Workspace {
            command: 7,
            payload: "file:///tmp".into(),
        }]
    );
    harness.assert_never_enqueues_anything().await;
}

/// A sequence split across two injected chunks still parses as one:
/// the injector and the drain share the scanner's streaming position.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_bytes_resume_a_split_sequence_on_the_drain() {
    let mut harness = harness(909, true);
    assert!(harness.session.scan_osc(b"\x1b]7;file:///Us").is_empty());
    harness.emit(b"ers/me\x07");
    match harness.next_output().await {
        TabOutput::Scanned { actions, .. } => assert_eq!(
            actions,
            vec![OscAction::Workspace {
                command: 7,
                payload: "file:///Users/me".into(),
            }]
        ),
        other => panic!("expected Scanned, got {other:?}"),
    }
}

/// (d) The default (the now-removed GTK UI's only mode): raw bytes,
/// byte-identical, no scan and nothing enqueued.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_path_forwards_raw_bytes_and_answers_nothing() {
    let mut harness = harness(907, false);
    let chunk = b"\x1b]11;?\x07\x1b]11;rgb:00/11/22\x07plain output";
    harness.emit(chunk);
    match harness.next_output().await {
        TabOutput::Bytes(data) => assert_eq!(data, chunk),
        other => panic!("the default path must forward raw Bytes, got {other:?}"),
    }
    assert!(
        harness.session.scan_osc(b"\x1b]11;?\x07").is_empty(),
        "no router without the opt-in"
    );
    harness.assert_never_enqueues_anything().await;
}
