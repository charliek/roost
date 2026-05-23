//! Workspace event → GTK bridge.
//!
//! Daemon-removal refactor M3b: replaces the gRPC `WatchEvents` stream
//! with an in-process subscription to [`crate::daemon::Workspace`]'s
//! broadcast channel. Events flow to the GTK main loop via an
//! unbounded mpsc channel that `glib::spawn_future_local` drains.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use roost_linux::daemon::{Workspace, WorkspaceEvent};

pub type EventSender = mpsc::UnboundedSender<WorkspaceEvent>;
#[allow(dead_code)]
pub type EventReceiver = mpsc::UnboundedReceiver<WorkspaceEvent>;

/// Subscribe to `workspace`'s broadcast and forward each event into
/// `tx`. Returns Ok when the broadcast closes (workspace dropped) or
/// the receiver is dropped. Logs and continues on `Lagged`.
pub async fn subscribe(workspace: Arc<Workspace>, tx: EventSender) -> Result<()> {
    let mut rx = workspace.subscribe();
    loop {
        match rx.recv().await {
            Ok(event) => {
                if tx.send(event).is_err() {
                    return Ok(());
                }
            }
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(dropped = n, "workspace event subscriber lagged");
            }
            Err(RecvError::Closed) => return Ok(()),
        }
    }
}
