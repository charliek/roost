//! WatchEvents → GTK bridge.
//!
//! Subscribes to the daemon's server-streaming `WatchEvents` RPC on
//! a tokio task and pushes each event into an unbounded channel that
//! the GTK main loop drains via `glib::spawn_future_local`. Mirrors
//! `mac/Sources/Roost/RoostClient.swift::watchEvents` 1:1.

use anyhow::Context;
use tokio::sync::mpsc;
use tonic::Request;

use roost_proto::v1::{Event, WatchEventsRequest};

use crate::client::RoostClient;

pub type EventSender = mpsc::UnboundedSender<Event>;
#[allow(dead_code)] // re-surfaces if the App ever owns the receiver directly.
pub type EventReceiver = mpsc::UnboundedReceiver<Event>;

/// Open a `WatchEvents` server-stream and forward every event into
/// `tx`. Returns Ok once the stream ends naturally (daemon shutdown)
/// or an Err on transport / stream errors so the caller can log +
/// optionally retry.
pub async fn subscribe(client: &mut RoostClient, tx: EventSender) -> anyhow::Result<()> {
    let mut stream = client
        .inner()
        .watch_events(Request::new(WatchEventsRequest { tab_id_filter: 0 }))
        .await
        .context("WatchEvents RPC failed")?
        .into_inner();

    while let Some(msg) = stream.message().await.transpose() {
        match msg {
            Ok(event) => {
                if tx.send(event).is_err() {
                    // GTK side dropped the receiver — stop draining.
                    return Ok(());
                }
            }
            Err(status) => {
                anyhow::bail!("watch_events stream error: {status}");
            }
        }
    }
    Ok(())
}
