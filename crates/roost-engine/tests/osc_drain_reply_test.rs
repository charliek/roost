//! The OSC opt-in, exercised through the REAL forwarding task.
//!
//! Plan 026 D10: iced answered color queries only after the event loop
//! drained its multiplexed feed (1-12 ms), so a program that queries and
//! exits — Go termenv's probe, as `prox` runs it — was gone before the
//! answer arrived and the reply landed in the shell prompt as
//! `11;rgb:…` garbage. The fix moves the scan into
//! [`TabSession`]'s forwarding task, which enqueues the reply onto the
//! same serial input channel keystrokes use, before the bytes have
//! reached the UI at all.
//!
//! That property is exactly what the roosttest feed/capture path cannot
//! show: `tab.feed_pty_bytes` injects bytes on the UI thread. Here the
//! bytes enter through a synthetic `PtyOutputEvent` broadcast — the same
//! receiver the supervisor hands out — and NOTHING drains the UI-side
//! output channel, so a reply that needed the UI could not appear.

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
/// the supervisor's write of an enqueued reply fails and is logged,
/// which is irrelevant here — the capture buffer records the enqueue,
/// which is the observation point `tab.capture_pty_input` reads.
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

    /// Poll the capture until it holds `needle`, then return everything
    /// captured. Nothing in this test drains `output_rx`, so a reply
    /// that reached the capture did so without any UI involvement.
    async fn await_capture(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let captured = String::from_utf8_lossy(&self.capture.lock().unwrap()).into_owned();
            if captured.contains(needle) {
                return captured;
            }
            assert!(
                Instant::now() < deadline,
                "never captured {needle:?} (captured {captured:?})"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn captured(&self) -> Vec<u8> {
        self.capture.lock().unwrap().clone()
    }

    async fn next_output(&mut self) -> TabOutput {
        tokio::time::timeout(Duration::from_secs(5), self.output_rx.recv())
            .await
            .expect("output arrives")
            .expect("the forwarding task is alive")
    }
}

/// (a) The reply is enqueued drain-side. The UI-side output channel is
/// never drained in this test, so the reply cannot have come from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_color_query_is_answered_from_the_drain_without_the_ui() {
    let harness = harness(901, true);
    harness.emit(b"\x1b]11;?\x07");
    let captured = harness
        .await_capture("\x1b]11;rgb:1e1e/1e1e/1e1e\x07")
        .await;
    assert_eq!(
        captured, "\x1b]11;rgb:1e1e/1e1e/1e1e\x07",
        "the reply is the only thing enqueued"
    );
}

/// The chunk still reaches the UI — bytes verbatim, and WITHOUT the
/// reply action, which the drain already consumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scanned_output_forwards_the_bytes_and_the_non_reply_actions() {
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
                "the query reply is gone; the title action is not"
            );
        }
        other => panic!("expected Scanned, got {other:?}"),
    }
}

/// (b) A SET in an earlier chunk moves the color a later QUERY answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cross_chunk_set_is_reflected_in_the_next_query() {
    let harness = harness(903, true);
    harness.emit(b"\x1b]11;rgb:00/11/22\x07");
    harness.emit(b"\x1b]11;?\x07");
    let captured = harness.await_capture("\x1b]11;rgb:").await;
    assert!(captured.contains("0000/1111/2222"), "{captured:?}");
    assert!(!captured.contains("1e1e/1e1e/1e1e"), "{captured:?}");
}

/// (c) A SET and a QUERY in ONE chunk answer from the chunk-start
/// snapshot — the semantics `OscRouter::feed` has always documented,
/// preserved exactly by the move to the drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_chunk_set_still_answers_the_pre_chunk_color() {
    let harness = harness(904, true);
    harness.emit(b"\x1b]11;rgb:00/11/22\x07\x1b]11;?\x07");
    let captured = harness.await_capture("\x1b]11;rgb:").await;
    assert!(captured.contains("1e1e/1e1e/1e1e"), "{captured:?}");
    assert!(!captured.contains("0000/1111/2222"), "{captured:?}");
}

/// A theme change re-seeds the drain-local state; the next query
/// answers the new theme.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reseeding_moves_what_the_drain_answers_with() {
    let harness = harness(905, true);
    harness.session.reseed_osc_colors(OscColorSnapshot::new(
        (0x11, 0x22, 0x33),
        (0x44, 0x55, 0x66),
        (0x77, 0x88, 0x99),
        [(0, 0, 0); 256],
    ));
    harness.emit(b"\x1b]11;?\x07");
    let captured = harness.await_capture("\x1b]11;rgb:").await;
    assert!(captured.contains("4444/5555/6666"), "{captured:?}");
}

/// The production race behind the explicit-set flag: a theme lands
/// while the program owns the color. The terminal keeps the program's
/// value (libghostty's `override orelse default`), so the drain must
/// answer with it too — otherwise every query for the rest of the
/// session reports a color the tab is not showing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_theme_change_does_not_clobber_a_color_the_program_set() {
    let mut harness = harness(908, true);
    harness.emit(b"\x1b]11;rgb:ab/cd/ef\x07");
    // Await the forwarded chunk: the SET is in the drain's state by the
    // time the UI could see the bytes at all.
    harness.next_output().await;

    harness.session.reseed_osc_colors(OscColorSnapshot::new(
        (0x11, 0x11, 0x11),
        (0x22, 0x22, 0x22),
        (0x33, 0x33, 0x33),
        [(0, 0, 0); 256],
    ));

    harness.emit(b"\x1b]11;?\x07\x1b]10;?\x07");
    let captured = harness.await_capture("\x1b]10;rgb:").await;
    assert!(
        captured.contains("abab/cdcd/efef"),
        "the background the program set must survive the theme: {captured:?}"
    );
    assert!(
        captured.contains("1111/1111/1111"),
        "the foreground it did NOT set must follow the theme: {captured:?}"
    );
}

/// `tab.feed_pty_bytes`'s route: injected bytes run the SAME router and
/// state as the drain, so they share the scanner's streaming position
/// and the cross-chunk color contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_bytes_share_the_drain_router_and_state() {
    let harness = harness(906, true);
    let actions = harness
        .session
        .scan_osc(b"\x1b]11;rgb:00/11/22\x07\x1b]7;file:///tmp\x07");
    assert_eq!(
        actions,
        vec![OscAction::Workspace {
            command: 7,
            payload: "file:///tmp".into(),
        }]
    );
    // The SET landed in the state the PTY-side drain answers from.
    harness.emit(b"\x1b]11;?\x07");
    let captured = harness.await_capture("\x1b]11;rgb:").await;
    assert!(captured.contains("0000/1111/2222"), "{captured:?}");
}

/// (d) The default (the now-removed GTK UI's only mode): raw bytes,
/// byte-identical, and no drain-side reply — the UI keeps its own
/// router and answers queries itself.
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
    // Give a would-be drain-side reply every chance to show up.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let captured = harness.captured();
    assert!(
        captured.is_empty(),
        "the default path must not enqueue anything: {:?}",
        String::from_utf8_lossy(&captured)
    );
}
