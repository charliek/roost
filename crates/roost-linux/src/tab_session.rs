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

/// Per-tab handle. Holds an `Arc<PtySupervisor>` so input/resize
/// calls route to the right session.
pub struct TabSession {
    pub tab_id: i64,
    supervisor: Arc<PtySupervisor>,
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
        Self { tab_id, supervisor }
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
        let supervisor = self.supervisor.clone();
        let tab_id = self.tab_id;
        tokio::spawn(async move {
            if let Err(err) = supervisor.write(tab_id, data).await {
                tracing::warn!(?err, tab_id, "pty write failed");
            }
        });
    }

    #[allow(dead_code)]
    pub fn send_resize(&self, cols: u16, rows: u16) {
        let supervisor = self.supervisor.clone();
        let tab_id = self.tab_id;
        tokio::spawn(async move {
            if let Err(err) = supervisor.resize(tab_id, cols, rows).await {
                tracing::warn!(?err, tab_id, "pty resize failed");
            }
        });
    }
}
