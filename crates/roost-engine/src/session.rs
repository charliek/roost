//! Per-tab PTY-output subscription.
//!
//! Daemon-removal refactor M3b: each `TabSession` subscribes to the
//! in-process [`crate::PtySupervisor`]'s broadcast for its
//! tab and forwards bytes / exit events to the UI adapter's main
//! thread via a tokio mpsc channel. The renderer drains the receiver
//! on the adapter's own main-loop task (`roost-iced` spawns a tokio
//! task per tab and forwards into its `Subscription`) so all
//! `vt_write` calls stay main-thread.
//!
//! Pre-M3b this module wrapped a gRPC bidi stream to `roost-core`'s
//! `StreamPty`. Everything stream-related is gone — the supervisor
//! lives in the same process, so the indirection collapses to a
//! single in-memory broadcast subscription.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::UnboundedSender;

use crate::osc::{OscAction, OscColorSnapshot, OscColorState, OscRouter};
use crate::{PtyOutputEvent, PtySupervisor};

/// Shared buffer used by the `tab.capture_pty_input` test op to
/// observe outbound PTY-input bytes. `None` in production; populated
/// by `App` when `ROOST_TEST_MODE=1`.
pub type InputCapture = Arc<Mutex<Vec<u8>>>;

pub type OutputSender = tokio::sync::mpsc::UnboundedSender<TabOutput>;
#[allow(dead_code)]
pub type OutputReceiver = tokio::sync::mpsc::UnboundedReceiver<TabOutput>;

#[derive(Debug)]
pub enum TabOutput {
    /// PTY emitted bytes; route into `Terminal::vt_write`. The UI owns
    /// the OSC scan for these — the default path, and the only one GTK
    /// ever sees.
    Bytes(Vec<u8>),
    /// PTY emitted bytes and this session's drain already scanned them
    /// (the `attach_scanned` opt-in). The bytes still route into
    /// `Terminal::vt_write` unchanged; `actions` carries what the scan
    /// produced MINUS the query replies, which the drain has already
    /// enqueued onto the PTY input channel. The UI must not run a
    /// second router over these bytes.
    Scanned {
        data: Vec<u8>,
        actions: Vec<OscAction>,
    },
    /// PTY exited (shell quit, supervisor closed it).
    Exit { status: i32, reason: String },
    /// Drain-level error (broadcast lag, etc.).
    Error(String),
}

/// A command queued onto a tab's serial PTY channel. Input and
/// resize share one channel so they apply in submission order.
enum PtyCommand {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// The drain-side OSC scan, shared by the forwarding task and the
/// handle. Both hold it briefly: the task per PTY chunk, the handle for
/// `scan_osc` (the test-mode byte injector) and `reseed_osc_colors` (a
/// theme change). One mutex keeps the scanner's streaming state and the
/// color state it answers from moving together.
struct OscDrain {
    router: OscRouter,
    colors: OscColorState,
}

/// Per-tab handle. Owns the sender of a per-tab serial command
/// channel; a single drain task applies each command to the
/// supervisor in submission order so keystrokes never reorder.
pub struct TabSession {
    // Handle identity. Captured into the drain task at construction
    // rather than read per-call, so it's no longer referenced after
    // attach — retained for diagnostics / external lookup.
    #[allow(dead_code)]
    pub tab_id: i64,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<PtyCommand>,
    /// Test-mode tap: when set, every payload enqueued onto the serial
    /// channel — from `send_input` OR from the drain's own OSC replies
    /// — is appended here first. `None` in production —
    /// allocated by `App` when `ROOST_TEST_MODE=1` so the
    /// `tab.capture_pty_input` IPC op can observe keystrokes,
    /// paste, and synthesised OSC replies that flow out to the
    /// PTY. The tap is upstream of the command queue, so it
    /// captures the bytes whether or not the supervisor write
    /// later succeeds — exactly what a test wants to assert on.
    input_capture: Option<InputCapture>,
    /// `Some` only under the `attach_scanned` opt-in (iced). `None` —
    /// the default, and what the (now-removed) GTK UI always used —
    /// leaves this session's forwarding task a pure byte pump,
    /// bit-identical to what it was before the opt-in existed.
    osc: Option<Arc<Mutex<OscDrain>>>,
}

/// Enqueue one payload onto a tab's serial PTY channel, mirroring it
/// into the test-mode capture first.
///
/// The capture lock is held ACROSS the send so the capture's order is
/// the channel's order even with the drain task and the UI thread
/// enqueuing concurrently — `tab.capture_pty_input`'s contract. The
/// send never blocks (unbounded), so the lock is held for a push.
///
/// A poisoned lock means a prior panic in this process; the enqueue
/// still happens, only the observation is lost.
fn enqueue_input(
    cmd_tx: &UnboundedSender<PtyCommand>,
    capture: Option<&InputCapture>,
    data: Vec<u8>,
) {
    if data.is_empty() {
        return;
    }
    // Bound to a local so the guard is still held at the send below —
    // that is what makes capture order channel order.
    let mut guard = capture.map(|capture| capture.lock());
    if let Some(Ok(buffer)) = &mut guard {
        buffer.extend_from_slice(&data);
    }
    let _ = cmd_tx.send(PtyCommand::Input(data));
}

/// Take the drain lock, recovering from poisoning: a poisoned mutex
/// means a prior panic somewhere else in the process, and a tab that
/// stops answering color queries for the rest of the session is a worse
/// outcome than continuing with the state we have.
fn lock_osc(osc: &Mutex<OscDrain>) -> std::sync::MutexGuard<'_, OscDrain> {
    osc.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Scan one chunk with a tab's drain-side router, then split what it
/// produced: query replies go straight onto the PTY input channel,
/// everything else is returned to travel to the UI with the bytes.
///
/// The scan AND its replies happen under one hold of the drain lock.
/// Releasing it in between would let a second scanner (the PTY drain
/// and a test-mode `scan_osc` are separate threads) interleave its
/// enqueue between this scan and this enqueue, putting replies on the
/// channel in the opposite order to the bytes that asked for them.
///
/// Lock order is `OscDrain` → `InputCapture`, and only here: every
/// other capture holder (`enqueue_input`, and each UI's
/// `tab.capture_pty_input` reader) takes the capture alone, so no site
/// can invert it.
fn scan_and_reply(
    osc: &Mutex<OscDrain>,
    cmd_tx: &UnboundedSender<PtyCommand>,
    capture: Option<&InputCapture>,
    bytes: &[u8],
) -> Vec<OscAction> {
    let mut drain = lock_osc(osc);
    let OscDrain { router, colors } = &mut *drain;
    let mut forwarded = Vec::new();
    for action in router.feed_stateful(bytes, colors) {
        match action {
            OscAction::PtyInput(reply) => enqueue_input(cmd_tx, capture, reply),
            other => forwarded.push(other),
        }
    }
    forwarded
}

impl TabSession {
    /// Attach to a tab the supervisor already spawned. `output_rx`
    /// should be a receiver subscribed before the supervisor's reader
    /// task started producing (`PtySupervisor::spawn`'s return, or
    /// the stashed twin via `take_initial_receiver`) — no early-byte
    /// loss.
    pub fn attach_with_receiver(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        output_rx: broadcast::Receiver<PtyOutputEvent>,
        output_tx: OutputSender,
        input_capture: Option<InputCapture>,
    ) -> Self {
        Self::attach_with_receiver_scanned(
            supervisor,
            tab_id,
            output_rx,
            output_tx,
            input_capture,
            None,
        )
    }

    /// [`TabSession::attach_with_receiver`] plus the OSC opt-in.
    ///
    /// With `osc_seed` set, this session's forwarding task owns the
    /// SOLE `OscRouter` for the tab: it scans each PTY chunk as it
    /// arrives, enqueues color-query replies onto the same serial input
    /// channel keystrokes use — before the bytes have even reached the
    /// UI — and forwards the chunk as [`TabOutput::Scanned`] with the
    /// remaining actions. That is the whole point of the opt-in: a
    /// program that queries and exits (Go termenv's 1-frame probe) gets
    /// its answer off the drain instead of one event-loop turn later,
    /// which is where the reply used to leak into the shell prompt.
    ///
    /// `osc_seed` is the theme the tab launched with; see
    /// [`OscColorState`] for why the drain tracks colors itself.
    pub fn attach_with_receiver_scanned(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        mut output_rx: broadcast::Receiver<PtyOutputEvent>,
        output_tx: OutputSender,
        input_capture: Option<InputCapture>,
        osc_seed: Option<OscColorSnapshot>,
    ) -> Self {
        // The serial channel is created first: the forwarding task
        // needs its sender to enqueue replies from the drain.
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<PtyCommand>();
        let osc = osc_seed.map(|seed| {
            Arc::new(Mutex::new(OscDrain {
                router: OscRouter::new(),
                colors: OscColorState::new(seed),
            }))
        });
        // Everything the scan needs travels as one option so the
        // default path takes nothing extra — not even a second sender
        // on the serial channel, whose lifetime ends the writer task.
        let scan = osc
            .clone()
            .map(|osc| (osc, cmd_tx.clone(), input_capture.clone()));
        tokio::spawn(async move {
            loop {
                match output_rx.recv().await {
                    Ok(PtyOutputEvent::Bytes(data)) => {
                        let output = match &scan {
                            Some((osc, cmd_tx, capture)) => {
                                let actions = scan_and_reply(osc, cmd_tx, capture.as_ref(), &data);
                                TabOutput::Scanned { data, actions }
                            }
                            None => TabOutput::Bytes(data),
                        };
                        if output_tx.send(output).is_err() {
                            break;
                        }
                    }
                    // Stopping here is safe on the normal path: the
                    // supervisor's reader task publishes `Exit` after
                    // the last bytes it read, so nothing is left
                    // behind (#255).
                    //
                    // The exception is `pty.rs`'s bounded deadline
                    // fallback. A reader that never reaches EOF — a
                    // background descendant holding the slave fd keeps
                    // the master readable forever — has `Exit`
                    // published out from under it after
                    // `EXIT_PUBLISH_GRACE`, and the bytes it reads
                    // afterwards are dropped by the `break` below.
                    // That is the deliberate trade: a tab that never
                    // reports its exit would never auto-close.
                    Ok(PtyOutputEvent::Exit(status)) => {
                        let _ = output_tx.send(TabOutput::Exit {
                            status,
                            reason: String::new(),
                        });
                        break;
                    }
                    // The other way a tab's output can be truncated,
                    // independent of the #255 ordering fix: this drain
                    // fell far enough behind that the broadcast
                    // dropped `n` messages. Out of scope there —
                    // fixing it means resizing or redesigning the
                    // channel (see `PTY_OUTPUT_BROADCAST_CAPACITY`).
                    // Surfaced rather than swallowed.
                    Err(RecvError::Lagged(n)) => {
                        let _ = output_tx.send(TabOutput::Error(format!(
                            "broadcast lagged: dropped {n} message(s)"
                        )));
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });

        // Single serial drain task: applies input/resize to the
        // supervisor in the exact order they were submitted. The
        // shared channel guarantees keystrokes (and resizes relative
        // to them) never reorder. Ends when the last `cmd_tx` drops
        // (TabSession dropped) — the forwarding task holds one too, so
        // a drain-side reply can never race the handle's teardown.
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    PtyCommand::Input(data) => {
                        if let Err(err) = supervisor.write(tab_id, data).await {
                            tracing::warn!(?err, tab_id, "pty write failed");
                        }
                    }
                    PtyCommand::Resize { cols, rows } => {
                        if let Err(err) = supervisor.resize(tab_id, cols, rows).await {
                            tracing::warn!(?err, tab_id, "pty resize failed");
                        }
                    }
                }
            }
        });
        Self {
            tab_id,
            cmd_tx,
            input_capture,
            osc,
        }
    }

    /// Attach by tab_id. The first attach consumes the receiver the
    /// supervisor subscribed before its reader task started, so
    /// output emitted before the UI attached (a fast launcher
    /// command's first bytes) is preserved; a reattach falls back to
    /// a fresh lazy subscription. Errors if the supervisor has no
    /// live PTY for that id. Production callers pass `None` for
    /// `input_capture`; `App` passes `Some` only when
    /// `ROOST_TEST_MODE=1`.
    pub fn attach(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        output_tx: OutputSender,
        input_capture: Option<InputCapture>,
    ) -> Result<Self> {
        Self::attach_scanned(supervisor, tab_id, output_tx, input_capture, None)
    }

    /// [`TabSession::attach`] plus the OSC opt-in — see
    /// [`TabSession::attach_with_receiver_scanned`].
    pub fn attach_scanned(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        output_tx: OutputSender,
        input_capture: Option<InputCapture>,
        osc_seed: Option<OscColorSnapshot>,
    ) -> Result<Self> {
        let rx = supervisor
            .take_initial_receiver(tab_id)
            .or_else(|| supervisor.subscribe_output(tab_id))
            .ok_or_else(|| anyhow::anyhow!("no live PTY for tab {tab_id}"))?;
        Ok(Self::attach_with_receiver_scanned(
            supervisor,
            tab_id,
            rx,
            output_tx,
            input_capture,
            osc_seed,
        ))
    }

    /// Scan a chunk of terminal output that did NOT come from the PTY
    /// — today only `tab.feed_pty_bytes`, the test-mode byte injector.
    ///
    /// It runs the SAME router and the SAME color state the drain does,
    /// so an injected chunk is indistinguishable from a real one: query
    /// replies are enqueued here, the caller writes the bytes into its
    /// terminal and applies the returned actions. Without the OSC
    /// opt-in there is nothing to scan with and the caller keeps its
    /// own router — no actions come back.
    ///
    /// **Callers must not race live PTY output.** Injected bytes and
    /// PTY bytes are two producers into one streaming scanner and one
    /// terminal, and the two orderings are independent: the injected
    /// chunk can be scanned between a PTY chunk's scan and its
    /// `vt_write`, or — worse — land inside a half-parsed sequence and
    /// corrupt both. Nothing here can order them, because the injector
    /// deliberately bypasses the drain (that is what makes it useful
    /// for tests). It is sound because `tab.feed_pty_bytes` is
    /// `ROOST_TEST_MODE=1`-gated and its tests drive quiet tabs, which
    /// the harness already requires for its own reasons
    /// (`wait_tab_quiet`, `tools/roosttest/README.md`). Production has
    /// exactly one scanner: the forwarding task.
    pub fn scan_osc(&self, bytes: &[u8]) -> Vec<OscAction> {
        let Some(osc) = &self.osc else {
            return Vec::new();
        };
        scan_and_reply(osc, &self.cmd_tx, self.input_capture.as_ref(), bytes)
    }

    /// Re-seed the drain-local color state after a theme change. A
    /// no-op without the OSC opt-in.
    pub fn reseed_osc_colors(&self, seed: OscColorSnapshot) {
        if let Some(osc) = &self.osc {
            lock_osc(osc).colors.reseed(seed);
        }
    }

    pub fn send_input(&self, data: Vec<u8>) {
        // One serial PTY writer, several enqueue sources.
        //
        // The single consumer is the `cmd_rx` task above: it is the
        // only thing that writes to the tab's master fd, so bytes reach
        // the PTY in exactly the order they were enqueued. What is NOT
        // single is the producer side — the UI adapter's main thread
        // (keystrokes, paste, resize, terminal replies) and, under the
        // OSC opt-in, this session's own drain task (color-query
        // replies, which is the point: they leave without waiting for
        // the UI's event loop). Both funnel through `enqueue_input`.
        //
        // The ordering contract that follows from that: enqueue order
        // IS observed order, and user input may interleave with
        // synthesised replies. `tab.capture_pty_input` observes the
        // same order — its buffer is written under a lock held across
        // the enqueue — so a test may assert on the presence and
        // relative order of what it caused, never on the absence of an
        // interleaving it did not.
        //
        // The capture is written BEFORE the supervisor write so an
        // assertion reflects what the session tried to write, even if
        // the write later fails — the test wants intent, not what the
        // kernel ultimately accepted. Empty payloads are dropped on
        // both paths.
        enqueue_input(&self.cmd_tx, self.input_capture.as_ref(), data);
    }

    pub fn send_resize(&self, cols: u16, rows: u16) {
        let _ = self.cmd_tx.send(PtyCommand::Resize { cols, rows });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// #80 A3: rapid `send_input` calls must reach the PTY in
    /// submission order. The PTY line discipline echoes each byte we
    /// write in the order the kernel received it, so the echoed
    /// stream is a faithful witness of write order. The old per-call
    /// `tokio::spawn` could reorder these under the multi-thread
    /// runtime; the single serial drain channel cannot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_input_preserves_submission_order() {
        let supervisor = Arc::new(PtySupervisor::new());
        let socket = std::path::PathBuf::from("/tmp/roost-tabsession-order.sock");
        let rx_pty = supervisor
            .spawn(1, "/tmp", &["/bin/cat".into()], 80, 24, &socket)
            .expect("spawn");
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        // Keep `session` alive: it owns the serial channel's sender,
        // and dropping it would end the drain task.
        let session = TabSession::attach_with_receiver(supervisor.clone(), 1, rx_pty, out_tx, None);

        for d in b'0'..=b'9' {
            session.send_input(vec![d]);
        }

        let mut seen = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && seen.len() < 10 {
            match out_rx.try_recv() {
                Ok(TabOutput::Bytes(b)) => {
                    for c in b {
                        if c.is_ascii_digit() {
                            seen.push(c as char);
                        }
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("output channel closed early: {e:?}"),
            }
        }
        supervisor.close(1);
        assert_eq!(seen, "0123456789", "send_input reordered keystrokes");
    }

    /// When attached with `Some(input_capture)`, every `send_input`
    /// payload is mirrored into the capture buffer before being
    /// enqueued — what `tab.capture_pty_input` later reads back.
    /// The buffer's contents are independent of whether the
    /// downstream PTY write succeeds (we never wait for it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_capture_records_send_input_in_order() {
        let supervisor = Arc::new(PtySupervisor::new());
        let socket = std::path::PathBuf::from("/tmp/roost-tabsession-capture.sock");
        let rx_pty = supervisor
            .spawn(7, "/tmp", &["/bin/cat".into()], 80, 24, &socket)
            .expect("spawn");
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let capture: InputCapture = Arc::new(Mutex::new(Vec::new()));
        let session = TabSession::attach_with_receiver(
            supervisor.clone(),
            7,
            rx_pty,
            out_tx,
            Some(capture.clone()),
        );

        session.send_input(b"hello".to_vec());
        session.send_input(b" world".to_vec());
        // Empty payload is dropped by send_input — must NOT appear
        // in the capture buffer either (matches the production
        // contract: empty writes are no-ops).
        session.send_input(Vec::new());

        let got = capture.lock().unwrap().clone();
        assert_eq!(got, b"hello world".to_vec());

        supervisor.close(7);
    }

    /// #267: output a fast command emits before the UI attaches must
    /// reach the terminal. The UI's attach can trail the spawn by a
    /// main-loop hop (or a whole TabOpened event round-trip for IPC
    /// opens); `TabSession::attach` consumes the receiver the
    /// supervisor subscribed before its reader task started, so those
    /// bytes are waiting in its buffer instead of lost to a late
    /// `subscribe_output`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attach_after_output_preserves_early_bytes() {
        let supervisor = Arc::new(PtySupervisor::new());
        let socket = std::path::PathBuf::from("/tmp/roost-tabsession-early.sock");
        // `exec cat` keeps the PTY alive so Exit can't race the drain.
        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf EARLY_MARKER; exec /bin/cat".to_string(),
        ];
        let _rx_dropped = supervisor
            .spawn(21, "/tmp", &argv, 80, 24, &socket)
            .expect("spawn");
        // Give the command time to run and its output time to reach
        // the broadcast channel — the window the old lazy subscribe
        // lost bytes in.
        tokio::time::sleep(Duration::from_millis(400)).await;

        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let _session = TabSession::attach(supervisor.clone(), 21, out_tx, None).expect("attach");

        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !seen.windows(12).any(|w| w == b"EARLY_MARKER") {
            match out_rx.try_recv() {
                Ok(TabOutput::Bytes(b)) => seen.extend_from_slice(&b),
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("output channel closed early: {e:?}"),
            }
        }
        supervisor.close(21);
        assert!(
            seen.windows(12).any(|w| w == b"EARLY_MARKER"),
            "pre-attach output was lost; got: {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    /// The stashed receiver is handed out exactly once; a second
    /// attach (reattach) falls back to a fresh lazy subscription and
    /// still succeeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_attach_falls_back_to_lazy_subscribe() {
        let supervisor = Arc::new(PtySupervisor::new());
        let socket = std::path::PathBuf::from("/tmp/roost-tabsession-reattach.sock");
        let _rx = supervisor
            .spawn(22, "/tmp", &["/bin/cat".into()], 80, 24, &socket)
            .expect("spawn");
        assert!(supervisor.take_initial_receiver(22).is_some());
        assert!(supervisor.take_initial_receiver(22).is_none());
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let _session = TabSession::attach(supervisor.clone(), 22, out_tx, None).expect("reattach");
        supervisor.close(22);
    }
}
