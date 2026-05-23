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

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use roost_linux::daemon::{PtyOutputEvent, PtySupervisor};

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
        Self { tab_id, cmd_tx }
    }

    /// Subscribe lazily by tab_id (used when reattaching to an
    /// existing session). Errors if the supervisor has no live PTY
    /// for that id.
    pub fn attach(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        output_tx: OutputSender,
    ) -> Result<Self> {
        let rx = supervisor
            .subscribe_output(tab_id)
            .ok_or_else(|| anyhow::anyhow!("no live PTY for tab {tab_id}"))?;
        Ok(Self::attach_with_receiver(
            supervisor, tab_id, rx, output_tx,
        ))
    }

    pub fn send_input(&self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        // Enqueue on the per-tab serial channel. `unbounded_send`
        // never blocks the GTK main thread and preserves submission
        // order; the prior per-call `tokio::spawn` could reorder
        // keystrokes under the multi-thread runtime.
        let _ = self.cmd_tx.send(PtyCommand::Input(data));
    }

    #[allow(dead_code)]
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
        let session = TabSession::attach_with_receiver(supervisor.clone(), 1, rx_pty, out_tx);

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
}
