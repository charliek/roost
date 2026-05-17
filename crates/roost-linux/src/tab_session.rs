//! Per-tab StreamPty session.
//!
//! One [`TabSession`] owns the bidi gRPC stream with the daemon, the
//! tokio task that drains PTY output, and the channel that feeds
//! keystroke + resize messages into the daemon. Output bytes are
//! delivered to a caller-provided `glib` sender on the GTK main
//! thread so the [`crate::terminal_view::TerminalView`] can `vt_write`
//! them without any cross-thread libghostty access.
//!
//! Mirrors `mac/Sources/Roost/TabSession.swift` and the Mac UI's
//! `RoostClient.runShellSession` 1:1 in shape; the channel-based
//! bridge to the GTK main loop is the gtk4-rs analog of Swift's
//! `@MainActor` dispatch.

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;

use roost_proto::v1::pty_client_message::Kind as PtyClientKind;
use roost_proto::v1::pty_server_message::Kind as PtyServerKind;
use roost_proto::v1::{PtyAttach, PtyClientMessage, PtyInput, PtyResize};

use crate::client::RoostClient;

/// Output bytes pushed from the tokio side. The GTK side polls via
/// `glib::spawn_future_local` on the matching `UnboundedReceiver`.
/// We use tokio's unbounded channel instead of `glib::MainContext::channel`
/// because that API was retired in glib 0.21; tokio's
/// `UnboundedReceiver` implements `Stream`, which `spawn_future_local`
/// can await directly via `StreamExt::next`.
pub type OutputSender = tokio::sync::mpsc::UnboundedSender<TabOutput>;
#[allow(dead_code)] // re-surfaces when commit 8 owns the per-tab session map.
pub type OutputReceiver = tokio::sync::mpsc::UnboundedReceiver<TabOutput>;

#[derive(Debug)]
pub enum TabOutput {
    /// PTY emitted bytes; route into `Terminal::vt_write`.
    Bytes(Vec<u8>),
    /// PTY exited (shell quit, daemon closed it). The renderer can
    /// keep the buffer visible; UI-level cascade lives in commit 8.
    Exit { status: i32, reason: String },
    /// Stream-level error from the daemon. Logged + the view stays
    /// on whatever bytes have already landed.
    Error(String),
}

/// Per-tab StreamPty handle. Drop closes the input channel, which
/// the daemon sees as a clean disconnect.
pub struct TabSession {
    /// Daemon-assigned tab id. Used by commit 8's WatchEvents
    /// handler to match incoming events to this session.
    #[allow(dead_code)]
    pub tab_id: i64,
    /// Outbound side of the bidi stream — keystrokes + resizes.
    /// Bounded so a runaway typer doesn't grow memory unbounded.
    input_tx: mpsc::Sender<PtyClientMessage>,
}

impl TabSession {
    /// Spawn a session for `tab_id`. The bidi stream is opened
    /// immediately with a `PtyAttach` at the requested dims; output
    /// bytes flow into `output_tx` on the GTK main thread.
    pub async fn spawn(
        client: &mut RoostClient,
        tab_id: i64,
        cols: u16,
        rows: u16,
        output_tx: OutputSender,
    ) -> Result<Self> {
        let (input_tx, input_rx) = mpsc::channel::<PtyClientMessage>(64);
        let outbound = ReceiverStream::new(input_rx);

        // Bind: first message MUST be PtyAttach per the proto.
        input_tx
            .send(PtyClientMessage {
                kind: Some(PtyClientKind::Attach(PtyAttach {
                    tab_id,
                    cols: cols as u32,
                    rows: rows as u32,
                })),
            })
            .await
            .context("send PtyAttach")?;

        let response = client
            .inner()
            .stream_pty(Request::new(outbound))
            .await
            .context("StreamPty RPC failed")?;
        let mut inbound = response.into_inner();

        // Drain task: tokio-side. Forwards every server message into
        // the GTK channel; the channel's main-loop handler is what
        // actually touches the Terminal.
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(msg)) => match msg.kind {
                        Some(PtyServerKind::Output(out)) => {
                            if output_tx.send(TabOutput::Bytes(out.data)).is_err() {
                                // GTK side dropped the receiver
                                // (window closed). Stop draining.
                                break;
                            }
                        }
                        Some(PtyServerKind::Exit(exit)) => {
                            let _ = output_tx.send(TabOutput::Exit {
                                status: exit.status,
                                reason: exit.reason,
                            });
                            break;
                        }
                        _ => {}
                    },
                    Ok(None) => break,
                    Err(status) => {
                        let _ = output_tx.send(TabOutput::Error(status.to_string()));
                        break;
                    }
                }
            }
        });

        Ok(Self { tab_id, input_tx })
    }

    /// Send keystroke bytes to the PTY. Non-blocking: if the channel
    /// is at capacity we drop the bytes with a warning rather than
    /// stalling the GTK main loop. The 64-slot buffer is sized for
    /// the typing-cadence path; sustained backpressure would only
    /// happen in pathological cases.
    pub fn send_input(&self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        if let Err(err) = self.input_tx.try_send(PtyClientMessage {
            kind: Some(PtyClientKind::Input(PtyInput { data })),
        }) {
            tracing::warn!(?err, tab_id = self.tab_id, "PTY input channel full");
        }
    }

    /// Send a resize message. Same try_send semantics as
    /// [`Self::send_input`] — a resize storm during live-drag drops
    /// stale events instead of queueing. Used by commit 7's
    /// `viewDidEndLiveResize` analog.
    #[allow(dead_code)]
    pub fn send_resize(&self, cols: u16, rows: u16) {
        if let Err(err) = self.input_tx.try_send(PtyClientMessage {
            kind: Some(PtyClientKind::Resize(PtyResize {
                cols: cols as u32,
                rows: rows as u32,
            })),
        }) {
            tracing::warn!(?err, tab_id = self.tab_id, "PTY resize channel full");
        }
    }
}
