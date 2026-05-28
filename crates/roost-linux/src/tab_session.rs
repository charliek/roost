//! Per-tab PTY-output subscription.
//!
//! Daemon-removal refactor M3b: each `TabSession` subscribes to the
//! in-process [`crate::daemon::PtySupervisor`]'s broadcast for its
//! tab and forwards bytes / exit events to the GTK main thread via a
//! tokio mpsc channel. The renderer drains the receiver inside a
//! `glib::spawn_future_local` so all `vt_write` calls stay
//! main-thread.
//!
//! Pre-M3b this module wrapped a gRPC bidi stream to `roost-core`'s
//! `StreamPty`. Everything stream-related is gone — the supervisor
//! lives in the same process, so the indirection collapses to a
//! single in-memory broadcast subscription.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use roost_linux::daemon::{PtyOutputEvent, PtySupervisor};

/// Shared buffer used by the `tab.capture_pty_input` test op to
/// observe outbound PTY-input bytes. `None` in production; populated
/// by `App` when `ROOST_TEST_MODE=1`.
pub type InputCapture = Arc<Mutex<Vec<u8>>>;

pub type OutputSender = tokio::sync::mpsc::UnboundedSender<TabOutput>;
#[allow(dead_code)]
pub type OutputReceiver = tokio::sync::mpsc::UnboundedReceiver<TabOutput>;

#[derive(Debug)]
pub enum TabOutput {
    /// PTY emitted bytes; route into `Terminal::vt_write`.
    Bytes(Vec<u8>),
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
    /// Test-mode tap: when set, every `send_input` payload is also
    /// appended here before being enqueued. `None` in production —
    /// allocated by `App` when `ROOST_TEST_MODE=1` so the
    /// `tab.capture_pty_input` IPC op can observe keystrokes,
    /// paste, and synthesised OSC replies that flow out to the
    /// PTY. The tap is upstream of the command queue, so it
    /// captures the bytes whether or not the supervisor write
    /// later succeeds — exactly what a test wants to assert on.
    input_capture: Option<InputCapture>,
}

impl TabSession {
    /// Attach to a tab the supervisor already spawned. `output_rx`
    /// is the broadcast receiver that `LocalClient::open_tab`
    /// returned from `PtySupervisor::spawn` (subscribed BEFORE the
    /// reader task started — no early-byte loss).
    pub fn attach_with_receiver(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        mut output_rx: broadcast::Receiver<PtyOutputEvent>,
        output_tx: OutputSender,
        input_capture: Option<InputCapture>,
    ) -> Self {
        tokio::spawn(async move {
            loop {
                match output_rx.recv().await {
                    Ok(PtyOutputEvent::Bytes(data)) => {
                        if output_tx.send(TabOutput::Bytes(data)).is_err() {
                            break;
                        }
                    }
                    Ok(PtyOutputEvent::Exit(status)) => {
                        let _ = output_tx.send(TabOutput::Exit {
                            status,
                            reason: String::new(),
                        });
                        break;
                    }
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
        // (TabSession dropped).
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<PtyCommand>();
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
        }
    }

    /// Subscribe lazily by tab_id (used when reattaching to an
    /// existing session). Errors if the supervisor has no live PTY
    /// for that id. Production callers pass `None` for
    /// `input_capture`; `App` passes `Some` only when
    /// `ROOST_TEST_MODE=1`.
    pub fn attach(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        output_tx: OutputSender,
        input_capture: Option<InputCapture>,
    ) -> Result<Self> {
        let rx = supervisor
            .subscribe_output(tab_id)
            .ok_or_else(|| anyhow::anyhow!("no live PTY for tab {tab_id}"))?;
        Ok(Self::attach_with_receiver(
            supervisor,
            tab_id,
            rx,
            output_tx,
            input_capture,
        ))
    }

    pub fn send_input(&self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        // Test-mode tap: mirror into the capture buffer before
        // enqueuing. Capture order matches submission order because
        // `send_input` is only ever called from the GTK main thread
        // (the `terminal_view.set_on_input` closure runs there, the
        // OSC drain runs there via `glib::spawn_future_local`, paste
        // runs there). No concurrent producers → the capture
        // observes the same byte order the cmd_rx drain enqueues.
        //
        // Captured BEFORE the send so a `tab.capture_pty_input`
        // assertion reflects what the UI tried to write, even if a
        // later supervisor write fails — the test wants to see
        // intent, not what the kernel ultimately accepted.
        //
        // Lock-poisoning is silently swallowed — a poisoned mutex
        // means a prior panic in this test process; nothing useful
        // to do here.
        if let Some(cap) = &self.input_capture {
            if let Ok(mut buf) = cap.lock() {
                buf.extend_from_slice(&data);
            }
        }
        // Enqueue on the per-tab serial channel. `unbounded_send`
        // never blocks the GTK main thread and preserves submission
        // order; the prior per-call `tokio::spawn` could reorder
        // keystrokes under the multi-thread runtime.
        let _ = self.cmd_tx.send(PtyCommand::Input(data));
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
}
