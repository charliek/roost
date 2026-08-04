//! The single engine → UI feed.
//!
//! Workspace events, per-tab PTY output, IPC `UiRequest`s, git-metrics
//! probes and provider subprocesses all land on one unbounded channel —
//! the first three through an adapter task on the app runtime, the latter
//! two sent directly from the runtime task that produced the result — and
//! the app drains it in FIFO order. One channel means one ordering across
//! those sources and one place to wake on.
//!
//! The [`Notify`] every sender carries is that wake source. Nothing
//! subscribes to it yet — the 16 ms tick still drives the drain — but it
//! is load-bearing plumbing: plan 012 C3 turns it into the Iced
//! subscription that replaces the tick, and the send-then-notify ordering
//! it enforces here is what makes that subscription lossless.

use std::sync::Arc;

use roost_engine::ipc::UiRequest;
use roost_engine::session::TabOutput;
use roost_engine::{Workspace, WorkspaceEvent};
use tokio::sync::{mpsc, Notify};

use crate::app::{AgentMetricsResult, ProviderRunResult};

/// Items drained in one batch before the drain returns to the event loop.
/// A PTY or IPC flood must not starve rendering, so a capped drain
/// re-arms the wake (see [`EngineFeedReceiver::try_next`]) instead of
/// looping to exhaustion.
const ENGINE_FEED_BATCH_CAP: usize = 256;

pub(crate) enum EngineFeed {
    Workspace(WorkspaceEvent),
    /// One tab's PTY output, tagged with the tab it came from — the tag
    /// is what lets every tab share this channel.
    Tab(i64, TabOutput),
    UiRequest(UiRequest),
    AgentMetrics(AgentMetricsResult),
    Provider(Box<ProviderRunResult>),
}

/// The only way to put an item on the feed. Raw senders are never handed
/// out so the send-then-notify ordering cannot be bypassed.
#[derive(Clone)]
pub(crate) struct EngineFeedSender {
    tx: mpsc::UnboundedSender<EngineFeed>,
    wake: Arc<Notify>,
}

impl EngineFeedSender {
    /// Returns `false` once the drain side is gone (the app dropped);
    /// adapter tasks end their loop on that.
    pub(crate) fn send(&self, item: EngineFeed) -> bool {
        // Send THEN notify. Notifying first would wake the drainer to a
        // still-empty channel, and `Notify` holds at most one permit — the
        // item would then sit unread until some unrelated later send.
        if self.tx.send(item).is_err() {
            return false;
        }
        self.wake.notify_one();
        true
    }
}

pub(crate) struct EngineFeedReceiver {
    rx: mpsc::UnboundedReceiver<EngineFeed>,
    wake: Arc<Notify>,
}

impl EngineFeedReceiver {
    /// Take the next item of `batch`, or `None` when the feed is empty or
    /// the batch is full. Hitting the cap re-arms the wake before
    /// returning so the remainder is guaranteed another drain.
    ///
    /// Classification is done here, off the item in hand, so the batch a
    /// drain site reports is the one the feed observed — a drain loop
    /// cannot forget to mark a workspace event and silently lose
    /// [`EngineBatch::should_reconcile`]'s guarantee.
    pub(crate) fn try_next(&mut self, batch: &mut EngineBatch) -> Option<EngineFeed> {
        if batch.items >= ENGINE_FEED_BATCH_CAP {
            batch.capped = true;
            self.wake.notify_one();
            return None;
        }
        let item = self.rx.try_recv().ok()?;
        batch.items += 1;
        batch.workspace_events |= matches!(item, EngineFeed::Workspace(_));
        batch.non_tab_bytes |= !matches!(item, EngineFeed::Tab(_, TabOutput::Bytes(_)));
        Some(item)
    }

    /// The wake this feed notifies on. C3's subscription stream clones it;
    /// its identity must stay stable for the lifetime of the app. Only the
    /// tests read it until then — the 16 ms tick is still the drain driver,
    /// so the permits it accumulates go unread by design.
    #[allow(dead_code)]
    pub(crate) fn wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }
}

/// What one drain batch contained, for the post-drain economy rules.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EngineBatch {
    pub(crate) items: usize,
    pub(crate) workspace_events: bool,
    /// At least one item was something other than `Tab(_, Bytes)` — the
    /// single fact the reconcile rule turns on.
    pub(crate) non_tab_bytes: bool,
    pub(crate) capped: bool,
}

impl EngineBatch {
    pub(crate) fn is_empty(&self) -> bool {
        self.items == 0
    }

    /// A wake that drained nothing costs nothing: spurious wakes are
    /// guaranteed by the permit model and must stay free. Neither does a
    /// batch of pure PTY bytes — the common case under a flood: bytes move
    /// terminal state, which the touched-tab refresh publishes, and the
    /// workspace mutations they can trigger (OSC 7/9, title changes)
    /// round-trip back as workspace events carrying their own reconcile.
    pub(crate) fn should_reconcile(&self) -> bool {
        self.workspace_events || self.non_tab_bytes
    }
}

pub(crate) fn channel() -> (EngineFeedSender, EngineFeedReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let wake = Arc::new(Notify::new());
    (
        EngineFeedSender {
            tx,
            wake: Arc::clone(&wake),
        },
        EngineFeedReceiver { rx, wake },
    )
}

/// Adapter task: workspace broadcast → feed, through the same bridge GTK
/// uses. `events::subscribe` owns the boot `Resync` and the
/// `Lagged` → `Resync` conversion; this only re-tags its output.
pub(crate) async fn pump_workspace_events(workspace: Arc<Workspace>, feed: EngineFeedSender) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let bridge = tokio::spawn(roost_engine::events::subscribe(workspace, tx));
    while let Some(event) = rx.recv().await {
        if !feed.send(EngineFeed::Workspace(event)) {
            break;
        }
    }
    bridge.abort();
}

/// Adapter task: one tab's PTY output → feed, tagged with the tab id.
/// Spawned at attach and ended by the tab's own channel closing, which
/// `TabSession`'s pump does after the PTY exits or the supervisor drops
/// it. A forwarder that dies before delivering `Exit` leaks nothing: the
/// next reconcile prunes any tab the workspace no longer lists.
pub(crate) async fn pump_tab_output(
    tab_id: i64,
    mut rx: mpsc::UnboundedReceiver<TabOutput>,
    feed: EngineFeedSender,
) {
    while let Some(output) = rx.recv().await {
        if !feed.send(EngineFeed::Tab(tab_id, output)) {
            break;
        }
    }
}

/// Adapter task: IPC ingress → feed. The handler keeps its own
/// `UnboundedSender<UiRequest>`; only the receiving end moved.
pub(crate) async fn pump_ui_requests(
    mut rx: mpsc::UnboundedReceiver<UiRequest>,
    feed: EngineFeedSender,
) {
    while let Some(request) = rx.recv().await {
        if !feed.send(EngineFeed::UiRequest(request)) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut EngineFeedReceiver) -> (Vec<EngineFeed>, EngineBatch) {
        let mut batch = EngineBatch::default();
        let mut items = Vec::new();
        while let Some(item) = rx.try_next(&mut batch) {
            items.push(item);
        }
        (items, batch)
    }

    /// Whether a wake permit is pending, without blocking the test.
    async fn took_wake(wake: &Notify) -> bool {
        tokio::select! {
            biased;
            () = wake.notified() => true,
            () = std::future::ready(()) => false,
        }
    }

    #[tokio::test]
    async fn every_sent_item_lands_in_one_drain_batch() {
        let (tx, mut rx) = channel();
        for _ in 0..8 {
            assert!(tx.send(EngineFeed::UiRequest(UiRequest::Activate)));
        }
        let wake = rx.wake_handle();
        assert!(took_wake(&wake).await, "a send arms the wake");

        let (items, batch) = drain(&mut rx);
        assert_eq!(items.len(), 8);
        assert_eq!(batch.items, 8);
        assert!(!batch.capped);
        assert!(batch.should_reconcile());
        assert!(
            !took_wake(&wake).await,
            "an uncapped drain must not re-arm the wake"
        );
    }

    #[tokio::test]
    async fn a_capped_batch_rewakes_itself_for_the_remainder() {
        let (tx, mut rx) = channel();
        for _ in 0..ENGINE_FEED_BATCH_CAP + 5 {
            assert!(tx.send(EngineFeed::UiRequest(UiRequest::Activate)));
        }
        let wake = rx.wake_handle();
        assert!(took_wake(&wake).await);
        assert!(!took_wake(&wake).await, "Notify holds one permit");

        let (items, batch) = drain(&mut rx);
        assert_eq!(items.len(), ENGINE_FEED_BATCH_CAP);
        assert!(batch.capped);
        assert!(took_wake(&wake).await, "the cap re-arms the wake");

        let (rest, batch) = drain(&mut rx);
        assert_eq!(rest.len(), 5);
        assert!(!batch.capped);
    }

    #[tokio::test]
    async fn an_empty_drain_is_a_no_op() {
        let (_tx, mut rx) = channel();
        let wake = rx.wake_handle();
        let (items, batch) = drain(&mut rx);
        assert!(items.is_empty());
        assert!(batch.is_empty());
        assert!(!batch.capped);
        assert!(!batch.should_reconcile());
        assert!(!took_wake(&wake).await);
    }

    #[tokio::test]
    async fn a_resync_only_batch_still_reconciles() {
        let (tx, mut rx) = channel();
        assert!(tx.send(EngineFeed::Workspace(WorkspaceEvent::Resync(vec![]))));

        let mut batch = EngineBatch::default();
        let item = rx.try_next(&mut batch).expect("the resync is queued");
        assert!(matches!(
            item,
            EngineFeed::Workspace(WorkspaceEvent::Resync(_))
        ));
        assert!(rx.try_next(&mut batch).is_none());
        assert!(batch.workspace_events, "the drain classifies the item");
        assert!(batch.should_reconcile());
    }

    #[tokio::test]
    async fn a_batch_of_nothing_but_tab_bytes_skips_the_reconcile() {
        let (tx, mut rx) = channel();
        for _ in 0..3 {
            assert!(tx.send(EngineFeed::Tab(7, TabOutput::Bytes(b"out".to_vec()))));
        }

        let (items, batch) = drain(&mut rx);
        assert_eq!(items.len(), 3);
        assert!(!batch.workspace_events);
        assert!(!batch.non_tab_bytes);
        assert!(
            !batch.should_reconcile(),
            "PTY bytes move terminal state, not workspace state"
        );
    }

    #[tokio::test]
    async fn one_non_bytes_item_makes_the_whole_batch_reconcile() {
        for tail in [
            EngineFeed::UiRequest(UiRequest::Activate),
            EngineFeed::Tab(
                7,
                TabOutput::Exit {
                    status: 0,
                    reason: "shell exited".into(),
                },
            ),
            EngineFeed::Tab(7, TabOutput::Error("broadcast lagged".into())),
            EngineFeed::Workspace(WorkspaceEvent::TabClosed { tab_id: 7 }),
        ] {
            let (tx, mut rx) = channel();
            assert!(tx.send(EngineFeed::Tab(7, TabOutput::Bytes(b"out".to_vec()))));
            assert!(tx.send(tail));

            let (items, batch) = drain(&mut rx);
            assert_eq!(items.len(), 2);
            assert!(batch.non_tab_bytes);
            assert!(batch.should_reconcile());
        }
    }

    #[tokio::test]
    async fn a_tab_forwarder_tags_its_output_and_ends_with_its_channel() {
        let (feed_tx, mut rx) = channel();
        let (tab_tx, tab_rx) = mpsc::unbounded_channel();
        let forwarder = tokio::spawn(pump_tab_output(7, tab_rx, feed_tx));

        assert!(tab_tx.send(TabOutput::Bytes(b"out".to_vec())).is_ok());
        assert!(tab_tx
            .send(TabOutput::Exit {
                status: 0,
                reason: "shell exited".into(),
            })
            .is_ok());
        drop(tab_tx);
        forwarder.await.expect("the forwarder ends, not panics");

        let (items, batch) = drain(&mut rx);
        assert!(matches!(items[0], EngineFeed::Tab(7, TabOutput::Bytes(_))));
        assert!(matches!(
            items[1],
            EngineFeed::Tab(7, TabOutput::Exit { .. })
        ));
        assert!(batch.should_reconcile(), "the exit closes a tab");
    }

    /// `TerminalTab`'s `Drop` aborts its forwarder; this is the half of
    /// that cascade the tab cannot express itself. The aborted task takes
    /// the tab's receiver with it, which is how the engine-side bridge
    /// learns — on its very next send — that it has nobody left to feed.
    #[tokio::test]
    async fn aborting_a_forwarder_closes_the_channel_it_drained() {
        let (feed_tx, mut rx) = channel();
        let (tab_tx, tab_rx) = mpsc::unbounded_channel();
        let forwarder = tokio::spawn(pump_tab_output(7, tab_rx, feed_tx));
        assert!(tab_tx.send(TabOutput::Bytes(b"live".to_vec())).is_ok());
        tokio::task::yield_now().await;

        forwarder.abort();
        assert!(
            forwarder.await.unwrap_err().is_cancelled(),
            "awaiting the aborted handle guarantees the task is dropped"
        );

        assert!(
            tab_tx.send(TabOutput::Bytes(b"after".to_vec())).is_err(),
            "the engine bridge cannot keep feeding a tab that is gone"
        );
        let (items, _) = drain(&mut rx);
        assert_eq!(items.len(), 1, "only what was forwarded before the abort");
    }

    /// The lag path for real: overflow the workspace broadcast while the
    /// bridge cannot run, then observe the `Resync` it converts that into.
    /// This is the property the deleted resubscribe-and-drop arm used to
    /// approximate with a per-tick snapshot.
    #[tokio::test]
    async fn a_lagged_broadcast_reaches_the_feed_as_a_resync() {
        let workspace = Arc::new(Workspace::new());
        let (tx, mut rx) = channel();
        tokio::spawn(pump_workspace_events(Arc::clone(&workspace), tx));
        // Wait for the boot `Resync` — it is sent AFTER the bridge
        // subscribes, so observing it proves the broadcast receiver
        // exists. Deterministic on the current-thread test runtime: the
        // publishes below are synchronous, so the parked bridge cannot
        // run between them.
        let mut boot = None;
        for _ in 0..1024 {
            tokio::task::yield_now().await;
            let mut batch = EngineBatch::default();
            if let Some(item) = rx.try_next(&mut batch) {
                boot = Some(item);
                break;
            }
        }
        assert!(
            matches!(boot, Some(EngineFeed::Workspace(WorkspaceEvent::Resync(_)))),
            "bridge must deliver the boot Resync before the test publishes"
        );

        // Broadcast capacity is 256; publish past it without yielding so
        // the parked bridge is genuinely lagged.
        for index in 0..300 {
            workspace
                .create_project(&format!("p{index}"), "/tmp")
                .expect("create project");
        }
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }

        let (items, _) = drain(&mut rx);
        let mut resyncs = items.iter().filter_map(|item| match item {
            EngineFeed::Workspace(WorkspaceEvent::Resync(projects)) => Some(projects),
            _ => None,
        });
        // The boot resync was consumed above; the first resync after the
        // overflow is the in-band lag conversion.
        let lag = resyncs.next().expect("the lag resync is delivered in-band");
        assert_eq!(lag.len(), 300, "the lag resync carries the full snapshot");
    }
}
