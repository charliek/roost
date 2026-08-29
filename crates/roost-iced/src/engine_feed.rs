//! The single engine → UI feed.
//!
//! Workspace events, per-tab PTY output, IPC `UiRequest`s, git-metrics
//! probes and provider subprocesses all land on one unbounded channel —
//! the first three through an adapter task on the app runtime, the latter
//! two sent directly from the runtime task that produced the result — and
//! the app drains it in FIFO order. One channel means one ordering across
//! those sources and one place to wake on.
//!
//! The [`Notify`] every sender carries is that wake source, and
//! [`wake_subscription`] is what turns it into `Message::EngineReady` —
//! the only thing that drains the feed now that the 16 ms tick is gone.
//! The send-then-notify ordering enforced here is what makes that
//! subscription lossless, and losslessness is no longer optional: a
//! missed wake is a stalled surface, not a 16 ms delay.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use iced::futures::stream;
use iced::Subscription;
use roost_engine::ipc::UiRequest;
use roost_engine::session::TabOutput;
use roost_engine::{Workspace, WorkspaceEvent};
use roost_ui_model::keys::{HostId, TabKey};
use tokio::sync::{mpsc, Notify};

use crate::app::{AgentMetricsResult, ProviderRunResult};
use crate::Message;

/// Items drained in one batch before the drain returns to the event loop.
/// A PTY or IPC flood must not starve rendering, so a capped drain
/// re-arms the wake (see [`EngineFeedReceiver::try_next`]) instead of
/// looping to exhaustion.
const ENGINE_FEED_BATCH_CAP: usize = 256;

pub(crate) enum EngineFeed {
    Workspace(WorkspaceEvent),
    /// One tab's PTY output, tagged with the tab it came from — the tag
    /// is what lets every tab share this channel. Host-qualified, so a
    /// backend's items can never land on another backend's tab that
    /// happens to carry the same numeric id.
    Tab(TabKey, TabOutput),
    /// One attached host tab's data-plane traffic: handshake outcomes,
    /// SNAP/PTY/EXIT/ERROR frames off the data connection, and the
    /// attach machinery's own timers. Background tasks only move the
    /// frames; the decoder and the client terminal they drive live in
    /// the per-tab attach state on the main thread (plan 037 §3.3).
    HostTab(TabKey, crate::app::host_tab::HostTabFrame),
    UiRequest(UiRequest),
    AgentMetrics(AgentMetricsResult),
    Provider(Box<ProviderRunResult>),
    /// The user picked an item off the native macOS menu bar. Not an
    /// engine event, but it wants exactly what one wants: the App-side
    /// receiver, the wake, and a place in the same FIFO — so a menu click
    /// is ordered against the workspace events around it instead of
    /// racing them on a channel of its own.
    #[cfg(target_os = "macos")]
    Menu(crate::macos::menu::MenuEvent),
    /// The user clicked the OS notification banner for this tab. It travels
    /// the feed like every other engine → UI item so the jump it triggers is
    /// ordered against the events that may have closed the tab meanwhile.
    NotificationActivated {
        tab: TabKey,
    },
    /// One connected host's workspace mirror moving forward, tagged with
    /// the connection *instance* that produced it.
    ///
    /// [`EngineFeed::Workspace`] stays untagged and local-only: the
    /// in-process workspace is the one every existing consumer means by
    /// "the workspace", and giving it a tag would have been a rename of
    /// every call site for no gain. A host's mirror is a different
    /// thing arriving on the same ordered channel — which is what keeps
    /// a remote `tab.opened` ordered against the local events around it.
    ///
    /// Carries no copy of the mirror: the connection task writes the
    /// shared one in place, so this is a wake plus the envelopes that
    /// have to be exact per commit.
    HostWorkspace(HostId, crate::host_conn::HostWorkspaceEvent),
    /// One host's connection lifecycle, for the sidebar headers,
    /// takeover banner and upgrade dialog.
    HostState(HostId, crate::host_conn::HostConnState),
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
        let workspace = matches!(item, EngineFeed::Workspace(_));
        let tab_bytes = matches!(
            item,
            EngineFeed::Tab(_, TabOutput::Bytes(_) | TabOutput::Scanned { .. })
                // `StepDecoder` joins them: it only steps history pages
                // or finishes the swap, so it moves terminal state and
                // never workspace state — a batch carrying one must not
                // pay a full reconcile.
                | EngineFeed::HostTab(
                    _,
                    crate::app::host_tab::HostTabFrame::Pty { .. }
                        | crate::app::host_tab::HostTabFrame::Snap { .. }
                        | crate::app::host_tab::HostTabFrame::StepDecoder { .. }
                )
        );
        batch.workspace_events |= workspace;
        batch.non_tab_bytes |= !tab_bytes;
        batch.workspace_dirty |= workspace;
        batch.dirty |= !tab_bytes;
        Some(item)
    }

    /// The wake this feed notifies on. [`wake_subscription`] clones it
    /// into the stream that drives the drain.
    pub(crate) fn wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }
}

/// What one drain batch contained, for the post-drain economy rules.
/// `items`/`workspace_events`/`non_tab_bytes`/`capped` describe the whole
/// batch (they are what the trace reports); the two dirty flags describe
/// only what has landed *since the last reconcile*, because a drain may
/// reconcile in its middle as well as at its end.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EngineBatch {
    pub(crate) items: usize,
    pub(crate) workspace_events: bool,
    /// At least one item was something other than `Tab(_, Bytes)` — the
    /// single fact the reconcile rule turns on.
    pub(crate) non_tab_bytes: bool,
    pub(crate) capped: bool,
    dirty: bool,
    workspace_dirty: bool,
}

impl EngineBatch {
    pub(crate) fn is_empty(&self) -> bool {
        self.items == 0
    }

    /// Whether a workspace event has landed since the last reconcile.
    /// The drain checks this before servicing an IPC request so a read op
    /// answers from a cache that already contains the mutation that
    /// preceded it in this batch.
    pub(crate) fn workspace_dirty(&self) -> bool {
        self.workspace_dirty
    }

    /// The drain reconciled: everything drained so far is folded into the
    /// UI's cache.
    pub(crate) fn mark_reconciled(&mut self) {
        self.dirty = false;
        self.workspace_dirty = false;
    }

    /// Something the drain *did* — as opposed to something it drained —
    /// left the cache behind again, so the tail still owes a reconcile.
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// A wake that drained nothing costs nothing: spurious wakes are
    /// guaranteed by the permit model and must stay free. Neither does a
    /// batch of pure PTY bytes — the common case under a flood: bytes move
    /// terminal state, which the touched-tab refresh publishes, and the
    /// workspace mutations they can trigger (OSC 7/9, title changes)
    /// round-trip back as workspace events carrying their own reconcile.
    ///
    /// This is the tail question — "is the cache still behind?" — so a
    /// mid-drain reconcile answers it too.
    pub(crate) fn should_reconcile(&self) -> bool {
        self.dirty
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

/// The recipe identity of the wake subscription. Iced keys a running
/// subscription on `TypeId` + the hash of its recipe data
/// (`iced_futures-0.14.0/src/subscription.rs:486-489`, tracker
/// `subscription/tracker.rs:74-84`) and drops any stream whose id is
/// missing from the next set (`tracker.rs:126`) — so an identity that
/// moved would restart the stream, and a canceled `Notify::notified()`
/// waiter can lose its permit on the way out. A constant is therefore the
/// whole hash: `Arc<Notify>` is not `Hash`, and its pointer is neither
/// stable nor meaningful as an identity.
const ENGINE_WAKE_ID: &str = "roost-iced::engine_feed::wake";

/// Recipe data for [`wake_subscription`]: the wake to await, plus the
/// constant that is its entire identity.
struct EngineWake {
    wake: Arc<Notify>,
}

impl Hash for EngineWake {
    fn hash<H: Hasher>(&self, state: &mut H) {
        ENGINE_WAKE_ID.hash(state);
    }
}

/// The subscription that replaces polling: every feed send notifies the
/// wake, and this stream turns each notification into one
/// `Message::EngineReady`.
///
/// The stream is rebuilt from the `Arc<Notify>` clone alone — no
/// take-once receiver, no state consumed at construction — so the recipe
/// stays a pure value that Iced may hash as often as it likes. Wakes
/// coalesce (a `Notify` holds at most one permit), which is exactly the
/// batching the drain wants: one wake, one drain of everything queued.
pub(crate) fn wake_subscription(wake: Arc<Notify>) -> Subscription<Message> {
    Subscription::run_with(EngineWake { wake }, |data: &EngineWake| {
        stream::unfold(Arc::clone(&data.wake), |wake| async move {
            wake.notified().await;
            Some((Message::EngineReady, wake))
        })
    })
}

/// Adapter task: workspace broadcast → feed, through `roost_engine::events`'s
/// UI-adapter bridge. `events::subscribe` owns the boot `Resync` and the
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

/// Adapter task: one tab's PTY output → feed, tagged with the tab it came
/// from. The engine's id-space is host-unaware, so the backend that
/// attached this tab is what qualified its bare id into `key`; the pump
/// simply carries that key onto the feed.
///
/// Spawned at attach and ended by the tab's own channel closing, which
/// `TabSession`'s pump does after the PTY exits or the supervisor drops
/// it. A forwarder that dies before delivering `Exit` leaks nothing: the
/// next reconcile prunes any tab the workspace no longer lists.
pub(crate) async fn pump_tab_output(
    key: TabKey,
    mut rx: mpsc::UnboundedReceiver<TabOutput>,
    feed: EngineFeedSender,
) {
    while let Some(output) = rx.recv().await {
        if !feed.send(EngineFeed::Tab(key, output)) {
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
    use std::hash::Hasher as _;

    use iced::advanced::subscription;
    use iced::futures::{Stream, StreamExt};

    use super::*;

    fn drain(rx: &mut EngineFeedReceiver) -> (Vec<EngineFeed>, EngineBatch) {
        let mut batch = EngineBatch::default();
        let mut items = Vec::new();
        while let Some(item) = rx.try_next(&mut batch) {
            items.push(item);
        }
        (items, batch)
    }

    /// Materialize the subscription's stream the way Iced's runtime does:
    /// take the recipe apart and run it against the (here empty) shell
    /// event stream every recipe is handed.
    fn wake_stream(wake: Arc<Notify>) -> impl Stream<Item = Message> + Unpin {
        let mut recipes = subscription::into_recipes(wake_subscription(wake));
        assert_eq!(recipes.len(), 1, "the wake is exactly one recipe");
        recipes
            .pop()
            .expect("one recipe")
            .stream(stream::empty().boxed())
    }

    /// The id Iced's tracker would key this subscription on.
    fn wake_recipe_id(wake: Arc<Notify>) -> u64 {
        let recipes = subscription::into_recipes(wake_subscription(wake));
        let mut hasher = subscription::Hasher::default();
        recipes[0].hash(&mut hasher);
        hasher.finish()
    }

    /// The next message the stream already has, without blocking the test.
    async fn next_now(stream: &mut (impl Stream<Item = Message> + Unpin)) -> Option<Message> {
        tokio::select! {
            biased;
            item = stream.next() => item,
            () = std::future::ready(()) => None,
        }
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
            assert!(tx.send(EngineFeed::Tab(
                TabKey::local(7),
                TabOutput::Bytes(b"out".to_vec())
            )));
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

    /// The bookkeeping behind the mid-drain reconcile. `App` has no test
    /// constructor (bootstrap needs a profile, the instance lock and the
    /// Iced runtime), so the drain loop's decision is exercised here at
    /// the seam it turns on: the flags, in the order `service_engine`
    /// reads and clears them. An IPC read that follows a mutation in the
    /// same batch must not answer from the pre-mutation cache.
    #[tokio::test]
    async fn a_request_after_a_workspace_event_reconciles_mid_drain() {
        let (tx, mut rx) = channel();
        assert!(tx.send(EngineFeed::Workspace(WorkspaceEvent::TabClosed {
            tab_id: 7
        })));
        assert!(tx.send(EngineFeed::UiRequest(UiRequest::Activate)));
        assert!(tx.send(EngineFeed::Tab(
            TabKey::local(7),
            TabOutput::Bytes(b"out".to_vec())
        )));

        let mut batch = EngineBatch::default();
        let mut mid_drain_reconciles = 0;
        while let Some(item) = rx.try_next(&mut batch) {
            if matches!(item, EngineFeed::UiRequest(_)) {
                assert!(
                    batch.workspace_dirty(),
                    "the event that preceded this request is not folded into the cache yet"
                );
                mid_drain_reconciles += 1;
                batch.mark_reconciled();
                assert!(
                    !batch.should_reconcile(),
                    "one reconcile answers the tail question too"
                );
                // Servicing the request can mutate the workspace itself.
                batch.mark_dirty();
            }
        }

        assert_eq!(mid_drain_reconciles, 1, "one extra reconcile, not one each");
        assert!(
            batch.should_reconcile(),
            "the request's own mutations still owe the tail a reconcile"
        );
        assert!(
            !batch.workspace_dirty(),
            "no workspace event landed after the mid-drain reconcile"
        );
        assert!(
            batch.workspace_events && batch.non_tab_bytes,
            "the whole-batch stats keep describing the whole batch"
        );
    }

    /// The other half of the lifecycle: a reconcile clears the slate, and
    /// only what lands afterwards can dirty it again — PTY bytes never do.
    #[tokio::test]
    async fn only_what_lands_after_a_reconcile_dirties_the_batch_again() {
        let (tx, mut rx) = channel();
        let mut batch = EngineBatch::default();

        assert!(tx.send(EngineFeed::Workspace(WorkspaceEvent::TabClosed {
            tab_id: 7
        })));
        assert!(rx.try_next(&mut batch).is_some());
        assert!(batch.workspace_dirty() && batch.should_reconcile());
        batch.mark_reconciled();

        assert!(tx.send(EngineFeed::Tab(
            TabKey::local(7),
            TabOutput::Bytes(b"out".to_vec())
        )));
        assert!(rx.try_next(&mut batch).is_some());
        assert!(
            !batch.should_reconcile() && !batch.workspace_dirty(),
            "PTY bytes move terminal state, never the workspace cache"
        );

        assert!(tx.send(EngineFeed::Workspace(WorkspaceEvent::TabClosed {
            tab_id: 8
        })));
        assert!(rx.try_next(&mut batch).is_some());
        assert!(
            batch.workspace_dirty() && batch.should_reconcile(),
            "a later event re-arms both the mid-drain and the tail reconcile"
        );
    }

    #[tokio::test]
    async fn one_non_bytes_item_makes_the_whole_batch_reconcile() {
        for tail in [
            EngineFeed::UiRequest(UiRequest::Activate),
            EngineFeed::Tab(
                TabKey::local(7),
                TabOutput::Exit {
                    status: 0,
                    reason: "shell exited".into(),
                },
            ),
            EngineFeed::Tab(
                TabKey::local(7),
                TabOutput::Error("broadcast lagged".into()),
            ),
            EngineFeed::Workspace(WorkspaceEvent::TabClosed { tab_id: 7 }),
            EngineFeed::NotificationActivated {
                tab: TabKey::local(7),
            },
        ] {
            let (tx, mut rx) = channel();
            assert!(tx.send(EngineFeed::Tab(
                TabKey::local(7),
                TabOutput::Bytes(b"out".to_vec())
            )));
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
        let forwarder = tokio::spawn(pump_tab_output(TabKey::local(7), tab_rx, feed_tx));

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
        assert!(matches!(
            items[0],
            EngineFeed::Tab(key, TabOutput::Bytes(_)) if key == TabKey::local(7)
        ));
        assert!(matches!(
            items[1],
            EngineFeed::Tab(key, TabOutput::Exit { .. }) if key == TabKey::local(7)
        ));
        assert!(batch.should_reconcile(), "the exit closes a tab");
    }

    /// The backend hands the pump the key it qualified the engine's bare
    /// id at; the pump carries exactly that onto the feed, and for the
    /// in-process backend it is the local instance.
    #[tokio::test]
    async fn the_pump_qualifies_engine_ids_at_the_local_instance() {
        let (feed_tx, mut rx) = channel();
        let (tab_tx, tab_rx) = mpsc::unbounded_channel();
        let forwarder = tokio::spawn(pump_tab_output(TabKey::local(7), tab_rx, feed_tx));

        assert!(tab_tx.send(TabOutput::Bytes(b"out".to_vec())).is_ok());
        drop(tab_tx);
        forwarder.await.expect("the forwarder ends, not panics");

        let (items, _) = drain(&mut rx);
        let EngineFeed::Tab(key, TabOutput::Bytes(bytes)) = &items[0] else {
            panic!("the pump tags its output");
        };
        assert_eq!(*key, TabKey::local(7));
        assert!(key.is_local());
        assert_eq!(bytes, b"out");
    }

    /// `TabHandle`'s `Drop` aborts its forwarder; this is the half of
    /// that cascade the tab cannot express itself. The aborted task takes
    /// the tab's receiver with it, which is how the engine-side bridge
    /// learns — on its very next send — that it has nobody left to feed.
    #[tokio::test]
    async fn aborting_a_forwarder_closes_the_channel_it_drained() {
        let (feed_tx, mut rx) = channel();
        let (tab_tx, tab_rx) = mpsc::unbounded_channel();
        let forwarder = tokio::spawn(pump_tab_output(TabKey::local(7), tab_rx, feed_tx));
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

    /// A wake is a hint that the feed has work, never a per-item signal:
    /// `Notify` holds at most one permit, so a burst of sends costs the UI
    /// one drain — which is the batching the whole design leans on.
    #[tokio::test]
    async fn a_burst_of_sends_coalesces_into_one_wake() {
        let (tx, rx) = channel();
        let wake = rx.wake_handle();
        for _ in 0..16 {
            assert!(tx.send(EngineFeed::UiRequest(UiRequest::Activate)));
        }
        assert!(took_wake(&wake).await, "the burst arms the wake");
        assert!(
            !took_wake(&wake).await,
            "sixteen sends leave one permit, not sixteen"
        );
    }

    /// The same property through the subscription: a burst that lands
    /// before the stream is ever polled yields exactly one `EngineReady`,
    /// and the drain that follows sees all sixteen items.
    #[tokio::test]
    async fn a_burst_yields_one_engine_ready_covering_the_whole_batch() {
        let (tx, mut rx) = channel();
        let mut stream = wake_stream(rx.wake_handle());
        for _ in 0..16 {
            assert!(tx.send(EngineFeed::UiRequest(UiRequest::Activate)));
        }

        assert!(matches!(
            next_now(&mut stream).await,
            Some(Message::EngineReady)
        ));
        assert!(
            next_now(&mut stream).await.is_none(),
            "one wake, one message"
        );
        let (items, _) = drain(&mut rx);
        assert_eq!(items.len(), 16, "the single wake drains everything queued");
    }

    /// The boot race: an adapter task can send before Iced has built the
    /// subscription's stream. The permit is stored, so the stream's very
    /// first wait completes immediately instead of stranding the batch
    /// until some unrelated later send.
    #[tokio::test]
    async fn a_send_before_the_stream_exists_still_wakes_it() {
        let (tx, rx) = channel();
        assert!(tx.send(EngineFeed::UiRequest(UiRequest::Activate)));

        let mut stream = wake_stream(rx.wake_handle());
        assert!(
            matches!(next_now(&mut stream).await, Some(Message::EngineReady)),
            "the stored permit is delivered to the first waiter"
        );
    }

    /// The other side of the race: a send that lands after the drain has
    /// already decided the feed was empty. Send-then-notify means the item
    /// is queued before the permit exists, so the permit cannot be
    /// consumed by a drain that ran too early to see it.
    #[tokio::test]
    async fn a_send_during_a_drain_rearms_the_wake() {
        let (tx, mut rx) = channel();
        let wake = rx.wake_handle();
        assert!(tx.send(EngineFeed::UiRequest(UiRequest::Activate)));
        assert!(took_wake(&wake).await, "the first send armed the wake");

        let (first, _) = drain(&mut rx);
        assert_eq!(first.len(), 1);
        // The drain has returned to the event loop; the adapter sends now.
        assert!(tx.send(EngineFeed::UiRequest(UiRequest::Activate)));

        assert!(took_wake(&wake).await, "the late send arms the wake again");
        let (second, _) = drain(&mut rx);
        assert_eq!(second.len(), 1, "and its item is still there to drain");
    }

    /// Iced keys a running subscription on the hash of its recipe data and
    /// drops any stream missing from the next set, so a recipe whose hash
    /// moved would be restarted — and a canceled `notified()` waiter can
    /// lose its permit on the way out. The hash must therefore be the
    /// constant and nothing else: not the `Arc`, not the app instance.
    #[tokio::test]
    async fn the_wake_recipe_id_is_constant() {
        let first = wake_recipe_id(Arc::new(Notify::new()));
        let second = wake_recipe_id(Arc::new(Notify::new()));
        assert_eq!(
            first, second,
            "the wake keeps one identity across constructions"
        );

        let shared = Arc::new(Notify::new());
        assert_eq!(
            wake_recipe_id(Arc::clone(&shared)),
            first,
            "and the Arc it carries is not part of that identity"
        );
    }
}
