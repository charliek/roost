//! Server-push event delivery: workspace batches → wire batches.
//!
//! The half of `events.subscribe` that has nothing to do with the IPC
//! envelope. [`envelope`] is the serializer — one total match from
//! [`WorkspaceEvent`] to the wire's event catalog — and [`spawn`] is the
//! adapter task that turns a [`VersionedWorkspaceEvent`] subscription
//! into the [`PushSource`] `roost-ipc` writes from.
//!
//! Two rules shape everything here:
//!
//! * **No holes.** Every commit publishes exactly one batch, empty
//!   commits included, so a client's gap check ("did I skip a
//!   revision?") is the whole loss-detection protocol.
//! * **Close rather than lie.** Anything that would put a hole in the
//!   stream — a lagged broadcast, a full queue, an internal
//!   [`WorkspaceEvent::Resync`] that has no wire spelling — ends the
//!   connection instead. A close carries no message on this wire; what
//!   the client does with it is dial back and resume from the fence it
//!   reached, and the next `events.subscribe` is where the session says
//!   how far back it can catch the client up — `replay-expired`, naming
//!   the oldest revision it still holds, is the answer that means
//!   "snapshot with `tab.list` instead".
//!
//! A replay drains into the same bounded queue while the live receiver
//! is not being read, so a long enough drain can itself lag the live
//! half and close the stream mid-replay. That is the rule working, not
//! an edge: each resume advances the client's fence by whatever it did
//! receive, so the retries converge.

use std::sync::Arc;
use std::time::Duration;

use roost_ipc::messages::{
    bytes_base64, ops, ActiveChangedEvent, AgentReportChangedEvent, EventBatch, EventEnvelope,
    HookActiveChangedEvent, NotificationFiredEvent, ProjectCreatedEvent, ProjectDeletedEvent,
    ProjectRenamedEvent, ProjectsReorderedEvent, TabClosedEvent, TabCwdChangedEvent, TabEffect,
    TabEffectEvent, TabNotificationEvent, TabOpenedEvent, TabStateChangedEvent,
    TabTitleChangedEvent, TabsReorderedEvent,
};
use roost_ipc::PushSource;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, warn};

use crate::workspace::TabEffectKind;
use crate::{ResumeCut, VersionedWorkspaceEvent, WorkspaceEvent};

/// What one subscription is allowed to see, asked at the instant a batch
/// is enqueued.
///
/// The seam exists so the *decision* can be made under whatever lock the
/// embedder reclassifies streams with. A takeover flips a driver stream
/// to an observer and injects `session.driver_changed` into the same
/// queue; unless the classification and the enqueue are one step, an
/// effect batch could be written after the envelope that says the reader
/// is no longer entitled to effects.
pub trait StreamGate: Send + Sync + 'static {
    /// Project `batch` for this stream and hand it to `permit`.
    fn deliver(
        &self,
        permit: mpsc::Permit<'_, serde_json::Value>,
        batch: &VersionedWorkspaceEvent,
    ) -> Delivery;
}

/// What a gate did with the permit it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The batch is on the queue.
    Delivered,
    /// The permit carried something else — a notice the embedder could
    /// not enqueue itself because the queue was full at the time (plan
    /// 049 §3.8's `session.driver_changed`). The batch was **not**
    /// sent: the relay must reserve again and re-offer it, so the notice
    /// always precedes it and the batch is classified after whatever the
    /// notice announced.
    NoticeSentRetryBatch,
    /// The batch has no wire form and the stream must end — the same
    /// "close rather than lie" answer the relay gives a
    /// [`WorkspaceEvent::Resync`].
    End,
}

/// The gate for a stream with no classification to make: everything the
/// workspace publishes goes out.
///
/// The relay's own cases drive it, here and across the crate boundary in
/// `tests/`, which is why it is public: a fence or a teardown is easier
/// to read against a gate that decides nothing.
pub struct FullFeed;

impl StreamGate for FullFeed {
    fn deliver(
        &self,
        permit: mpsc::Permit<'_, serde_json::Value>,
        batch: &VersionedWorkspaceEvent,
    ) -> Delivery {
        match batch_value(batch, true) {
            Some(value) => {
                permit.send(value);
                Delivery::Delivered
            }
            None => Delivery::End,
        }
    }
}

/// How many batches may be queued for one subscriber before it is
/// treated as not keeping up.
///
/// Generous: a batch is a few hundred bytes and a client that is 256
/// commits behind is not a client that hit a hiccup, it is one that has
/// stopped reading. Tied to the workspace broadcast channel's own
/// capacity so neither bound goes first by accident.
pub const DEFAULT_PUSH_CAPACITY: usize = crate::workspace::EVENT_CHANNEL_CAPACITY;

/// Base budget for [`PushLimits::stall`].
const DEFAULT_PUSH_STALL: Duration = Duration::from_secs(30);

/// The two bounds on one subscriber's delivery.
///
/// Injectable so a test can force the overflow branch deterministically
/// instead of racing a 256-deep queue against a 30-second budget;
/// production never constructs anything but [`PushLimits::default`].
#[derive(Debug, Clone, Copy)]
pub struct PushLimits {
    /// Queue depth. Clamped to at least 1 — a zero-capacity channel
    /// would panic, and "no queue at all" is not a policy anyone means.
    pub capacity: usize,
    /// How long delivery may be stuck before the subscriber is dropped.
    /// One budget, two places it can be spent: a queue that stays full
    /// (this crate) and a socket write that will not complete
    /// (`roost-ipc`'s push loop, via
    /// [`PushSource::with_write_deadline`]). They are the same stall
    /// seen from either side of the channel, so they share a number
    /// rather than disagreeing about when a peer has stopped reading.
    pub stall: Duration,
}

impl Default for PushLimits {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_PUSH_CAPACITY,
            stall: DEFAULT_PUSH_STALL.mul_f64(roost_ipc::session_launch::timeout_scale()),
        }
    }
}

/// The wire form of one workspace event, or `None` for one that must
/// never reach a client.
///
/// Each arm goes through the event's declared wire type in
/// [`roost_ipc::messages`], so the id encoding and the field names are
/// the ones a client decodes with rather than a second spelling of them.
///
/// Deliberately a total match with no catch-all arm: a new
/// [`WorkspaceEvent`] variant is a compile error here, which is the
/// point — the alternative is a variant that silently never ships.
///
/// [`WorkspaceEvent::Resync`] is the one `None`. It is minted
/// *client-side* by the in-process UI bridge (`events::subscribe`) as a
/// full-state recovery snapshot; on the wire the equivalent signal is
/// the connection closing, so a `Resync` reaching this function means
/// the caller must drop the connection rather than serialize it.
pub fn envelope(event: &WorkspaceEvent) -> Option<EventEnvelope> {
    use serde_json::to_value;

    let (name, data) = match event {
        WorkspaceEvent::TabOpened(tab) => (
            ops::EVENT_TAB_OPENED,
            to_value(TabOpenedEvent { tab: tab.clone() }),
        ),
        WorkspaceEvent::TabClosed { tab_id } => (
            ops::EVENT_TAB_CLOSED,
            to_value(TabClosedEvent { tab_id: *tab_id }),
        ),
        WorkspaceEvent::TabStateChanged { tab_id, state } => (
            ops::EVENT_TAB_STATE_CHANGED,
            to_value(TabStateChangedEvent {
                tab_id: *tab_id,
                state: *state,
            }),
        ),
        WorkspaceEvent::TabTitleChanged { tab_id, title } => (
            ops::EVENT_TAB_TITLE_CHANGED,
            to_value(TabTitleChangedEvent {
                tab_id: *tab_id,
                title: title.clone(),
            }),
        ),
        WorkspaceEvent::TabCwdChanged { tab_id, cwd } => (
            ops::EVENT_TAB_CWD_CHANGED,
            to_value(TabCwdChangedEvent {
                tab_id: *tab_id,
                cwd: cwd.clone(),
            }),
        ),
        WorkspaceEvent::TabNotification {
            tab_id,
            has_pending,
        } => (
            ops::EVENT_TAB_NOTIFICATION,
            to_value(TabNotificationEvent {
                tab_id: *tab_id,
                has_pending: *has_pending,
            }),
        ),
        WorkspaceEvent::ProjectCreated(project) => (
            ops::EVENT_PROJECT_CREATED,
            to_value(ProjectCreatedEvent {
                project: project.clone(),
            }),
        ),
        WorkspaceEvent::ProjectRenamed { project_id, name } => (
            ops::EVENT_PROJECT_RENAMED,
            to_value(ProjectRenamedEvent {
                project_id: *project_id,
                name: name.clone(),
            }),
        ),
        WorkspaceEvent::ProjectDeleted { project_id } => (
            ops::EVENT_PROJECT_DELETED,
            to_value(ProjectDeletedEvent {
                project_id: *project_id,
            }),
        ),
        WorkspaceEvent::ActiveChanged { project_id, tab_id } => (
            ops::EVENT_ACTIVE_CHANGED,
            to_value(ActiveChangedEvent {
                project_id: *project_id,
                tab_id: *tab_id,
            }),
        ),
        WorkspaceEvent::HookActiveChanged { tab_id, active } => (
            ops::EVENT_HOOK_ACTIVE_CHANGED,
            to_value(HookActiveChangedEvent {
                tab_id: *tab_id,
                active: *active,
            }),
        ),
        // The one event whose wire shape is not its in-process shape:
        // the wire carries the two projections (`state`, `hook_active`)
        // pre-derived so no subscriber has to re-run them.
        WorkspaceEvent::AgentChanged { tab_id, agent } => (
            ops::EVENT_AGENT_REPORT_CHANGED,
            to_value(AgentReportChangedEvent {
                tab_id: *tab_id,
                shell_state: agent.shell,
                agent_lifecycle: agent.lifecycle,
                ownership: agent.ownership.clone(),
                state: roost_ipc::agent::effective(agent),
                hook_active: roost_ipc::agent::is_live(agent),
            }),
        ),
        WorkspaceEvent::NotificationFired {
            tab_id,
            title,
            body,
        } => (
            ops::EVENT_NOTIFICATION_FIRED,
            to_value(NotificationFiredEvent {
                tab_id: *tab_id,
                title: title.clone(),
                body: body.clone(),
            }),
        ),
        WorkspaceEvent::TabsReordered {
            project_id,
            tab_ids,
        } => (
            ops::EVENT_TABS_REORDERED,
            to_value(TabsReorderedEvent {
                project_id: *project_id,
                tab_ids: tab_ids.clone(),
            }),
        ),
        // Plan 037 §3.6. The in-process payload is plaintext; base64 is
        // the wire's encoding for bytes, so it is applied here, at the
        // projection boundary, like every other wire-shape difference.
        // The clipboard payload goes on the wire and nowhere else — no
        // log line here or in the tab task ever carries it.
        WorkspaceEvent::TabEffect { tab_id, effect } => (
            ops::EVENT_TAB_EFFECT,
            to_value(match effect {
                TabEffectKind::Bell => TabEffectEvent {
                    tab_id: *tab_id,
                    effect: TabEffect::Bell,
                    data: None,
                    target: None,
                },
                TabEffectKind::ClipboardWrite { text, target } => TabEffectEvent {
                    tab_id: *tab_id,
                    effect: TabEffect::ClipboardWrite,
                    data: Some(bytes_base64::encode(text.as_bytes())),
                    target: Some(*target),
                },
            }),
        ),
        WorkspaceEvent::ProjectsReordered { project_ids } => (
            ops::EVENT_PROJECTS_REORDERED,
            to_value(ProjectsReorderedEvent {
                project_ids: project_ids.clone(),
            }),
        ),
        WorkspaceEvent::Resync(_) => return None,
    };
    match data {
        Ok(data) => Some(EventEnvelope {
            event: name.to_string(),
            data,
        }),
        // Not reachable with today's types (no floats, no non-string map
        // keys), but the alternative to reporting it is a hole in a
        // stream whose whole contract is that it has none.
        Err(error) => {
            warn!(%error, name, "workspace event could not be serialized for push");
            None
        }
    }
}

/// One batch's wire form, or `None` if the connection must close.
///
/// `driver` is the stream's classification (plan 049 §3.7). An observer
/// sees every workspace fact — `notification.fired` included, because
/// routing notifications is what a watcher subscribes for — but never a
/// [`WorkspaceEvent::TabEffect`]: bells and OSC 52 clipboard writes are
/// the *driving* client's side-channel (DL-18), and fanning a clipboard
/// payload out to every watcher is not a filter anyone can add later.
///
/// A revision whose every event was filtered still ships, as an empty
/// batch. The client's loss check is "did I skip a revision", so a
/// silently-dropped commit would read as loss; an empty batch advances
/// the fence exactly as a real one does.
pub fn batch_value(batch: &VersionedWorkspaceEvent, driver: bool) -> Option<serde_json::Value> {
    let mut events = Vec::with_capacity(batch.events.len());
    for event in &batch.events {
        if !driver && matches!(event, WorkspaceEvent::TabEffect { .. }) {
            continue;
        }
        events.push(envelope(event)?);
    }
    // Infallible in practice: the envelopes are already `Value`s.
    serde_json::to_value(EventBatch {
        revision: batch.revision,
        events,
    })
    .ok()
}

/// Everything one [`spawn`]ed subscription hands back to its caller.
pub struct Subscription {
    /// The revision the subscription starts from — the ack the client
    /// is owed, meaning "you already have everything up to this". On a
    /// resume that is the `from_revision` the client presented, not the
    /// cut's fence: the batches between the two are replayed, so the
    /// first batch on the wire is `revision + 1` either way.
    pub revision: u64,
    /// What the server writes frames from.
    pub source: PushSource,
    /// Ends the relay. Dropping its sender is what closes the
    /// connection, so this doubles as "cut this stream".
    pub abort: AbortHandle,
    /// Writes a non-batch envelope into this stream's queue, ahead of
    /// nothing and behind everything already queued — the serialization
    /// a takeover's `session.driver_changed` rides.
    ///
    /// **Weak on purpose.** A strong clone parked in a registry would
    /// keep the channel open after the relay ended, turning what should
    /// be an EOF into a connection that hangs with no producer. Upgrade
    /// failing *is* "this stream is already going away".
    pub inject: mpsc::WeakSender<serde_json::Value>,
}

/// Start relaying `cut`'s commits through `gate`.
///
/// **Never subscribes a receiver of its own.** The one the workspace
/// captured under its commit lock ([`crate::Workspace::subscribe_from`]) is the
/// only one this subscription ever has, so there is exactly one receiver
/// creation per subscription and no commit can fall between the ring
/// copy and the receiver's existence — see [`ResumeCut`].
///
/// The task ends (dropping its sender, which closes the connection) on
/// any condition that would otherwise hide a loss: a lagged broadcast, a
/// queue that stays full past [`PushLimits::stall`], a `Resync`, or a
/// dropped receiver.
pub fn spawn(cut: ResumeCut, limits: PushLimits, gate: Arc<dyn StreamGate>) -> Subscription {
    // What the client already has. The ring is gapless, so a non-empty
    // replay starts at exactly one past it; an empty replay means the
    // client is already at the cut.
    let revision = cut
        .replay
        .first()
        .map_or(cut.fence, |batch| batch.revision.saturating_sub(1));
    let (tx, source_rx) = mpsc::channel(limits.capacity.max(1));
    let inject = tx.downgrade();
    let task = tokio::spawn(relay(cut, tx, limits, gate));
    Subscription {
        revision,
        // The queue bound and the socket-write bound are the same
        // policy seen from two sides, so they share one budget.
        source: PushSource::new(source_rx).with_write_deadline(limits.stall),
        abort: task.abort_handle(),
        inject,
    }
}

/// Drain `cut`'s replay into `tx`, then relay its receiver's batches,
/// dropping everything at or below the cut's fence, and return the
/// moment the stream must end.
///
/// Split out from [`spawn`] so the fence boundary and the teardown
/// conditions can be driven with a hand-fed channel, without a
/// workspace, a socket, or a race to set up.
///
/// Capacity is reserved *before* `gate` is consulted, and the gate then
/// classifies and enqueues in one step: waiting for room is the only
/// part that can block, and it must not happen under the embedder's
/// registry lock. Replayed batches take that same path — they are
/// already effect-free, so the gate's filter is a no-op on them, but a
/// takeover notice parked on this stream must still come out ahead of
/// them.
async fn relay(
    cut: ResumeCut,
    tx: mpsc::Sender<serde_json::Value>,
    limits: PushLimits,
    gate: Arc<dyn StreamGate>,
) {
    let ResumeCut {
        mut rx,
        replay,
        fence,
    } = cut;
    for batch in replay {
        // No `tx.closed()` arm is needed here: nothing is awaited but
        // the reservation, which fails outright once the receiver is
        // gone.
        if let Step::Stop = push_batch(&batch, &tx, limits, &gate).await {
            return;
        }
    }
    loop {
        let received = tokio::select! {
            // The connection went away. Without this arm the task parks
            // in `recv` until the *next* commit — which on an idle
            // session may never come — holding a broadcast receiver and
            // an un-prunable registry entry for every client that has
            // already hung up. Dropping an mpsc receiver does not wake a
            // sender parked elsewhere, so the wake has to be asked for.
            () = tx.closed() => return,
            received = rx.recv() => received,
        };
        match received {
            // Already covered by the revision the client was acked
            // with: it committed between the subscribe and the read.
            Ok(batch) if batch.revision <= fence => continue,
            Ok(batch) => {
                if let Step::Stop = push_batch(&batch, &tx, limits, &gate).await {
                    return;
                }
            }
            Err(RecvError::Lagged(missed)) => {
                warn!(
                    missed,
                    "events subscriber lagged the workspace broadcast; closing the connection"
                );
                return;
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// Whether the relay may go on after one batch.
enum Step {
    Continue,
    Stop,
}

/// Put one batch on the queue, waiting up to [`PushLimits::stall`] for
/// room.
///
/// One batch can cost two permits. A takeover that found this queue full
/// left its `session.driver_changed` with the gate; the gate spends the
/// first permit on that notice and the batch is re-offered, which is
/// what keeps the notice ahead of it. Each attempt gets the full stall
/// budget, and a peer that never drains still dies on the first one.
async fn push_batch(
    batch: &VersionedWorkspaceEvent,
    tx: &mpsc::Sender<serde_json::Value>,
    limits: PushLimits,
    gate: &Arc<dyn StreamGate>,
) -> Step {
    loop {
        let permit = match tokio::time::timeout(limits.stall, tx.reserve()).await {
            Ok(Ok(permit)) => permit,
            // Receiver gone: the connection is already down.
            Ok(Err(_)) => return Step::Stop,
            Err(_) => {
                warn!(
                    revision = batch.revision,
                    "events subscriber is not draining; closing the connection"
                );
                return Step::Stop;
            }
        };
        match gate.deliver(permit, batch) {
            Delivery::Delivered => return Step::Continue,
            Delivery::NoticeSentRetryBatch => continue,
            Delivery::End => {
                debug!(
                    revision = batch.revision,
                    "unpushable workspace event; closing the events connection"
                );
                return Step::Stop;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::messages::EventBatch;

    const TEST_LIMITS: PushLimits = PushLimits {
        capacity: 8,
        stall: Duration::from_millis(200),
    };

    fn batch(revision: u64) -> VersionedWorkspaceEvent {
        VersionedWorkspaceEvent {
            revision,
            events: vec![WorkspaceEvent::TabClosed { tab_id: 5 }],
        }
    }

    /// A cut with nothing to replay: a hand-fed receiver and a fence.
    fn live_cut(
        rx: tokio::sync::broadcast::Receiver<VersionedWorkspaceEvent>,
        fence: u64,
    ) -> ResumeCut {
        ResumeCut {
            rx,
            replay: Vec::new(),
            fence,
        }
    }

    async fn next(rx: &mut mpsc::Receiver<serde_json::Value>) -> Option<EventBatch> {
        let value = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the relay must answer")?;
        Some(serde_json::from_value(value).expect("typed batch"))
    }

    /// The fence is inclusive, and only a hand-fed channel can prove it:
    /// over a real workspace the equality case fires only when a commit
    /// lands inside the subscribe→revision-read window, so a regression
    /// to `<` would pass the rest of the suite.
    #[tokio::test]
    async fn the_fence_drops_its_own_revision_and_keeps_the_next() {
        let (events, rx) = tokio::sync::broadcast::channel(16);
        let (tx, mut source) = mpsc::channel(8);
        let task = tokio::spawn(relay(live_cut(rx, 7), tx, TEST_LIMITS, Arc::new(FullFeed)));

        // Below, at, and above the fence, in one go: only the last two
        // may be delivered, and they must arrive in order.
        events.send(batch(6)).unwrap();
        events.send(batch(7)).unwrap();
        events.send(batch(8)).unwrap();
        events.send(batch(9)).unwrap();

        assert_eq!(next(&mut source).await.expect("a batch").revision, 8);
        assert_eq!(next(&mut source).await.expect("a batch").revision, 9);

        drop(events);
        assert!(next(&mut source).await.is_none());
        task.await.expect("the relay ends with its broadcast");
    }

    /// A client that hangs up ends its relay immediately, with no commit
    /// to wake it. Otherwise every disconnect on an idle session parks a
    /// task forever, and its registry entry never reports `is_finished`.
    #[tokio::test]
    async fn a_dropped_receiver_ends_the_relay_without_a_commit() {
        let (events, rx) = tokio::sync::broadcast::channel(16);
        let (tx, source) = mpsc::channel(8);
        let task = tokio::spawn(relay(live_cut(rx, 0), tx, TEST_LIMITS, Arc::new(FullFeed)));

        drop(source);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the relay must end when its receiver goes away")
            .expect("relay task");

        // Nothing was ever committed, and the broadcast is still open —
        // the wake came from the disconnect alone.
        assert_eq!(events.receiver_count(), 0);
    }

    /// The observer projection: every workspace fact survives and the
    /// effect does not.
    #[test]
    fn an_observer_loses_the_effect_and_keeps_the_revision() {
        let commit = VersionedWorkspaceEvent {
            revision: 12,
            events: vec![
                WorkspaceEvent::TabEffect {
                    tab_id: 5,
                    effect: TabEffectKind::Bell,
                },
                WorkspaceEvent::NotificationFired {
                    tab_id: 5,
                    title: "t".into(),
                    body: "b".into(),
                },
            ],
        };

        let driver: EventBatch =
            serde_json::from_value(batch_value(&commit, true).expect("a wire form")).unwrap();
        assert_eq!(
            driver
                .events
                .iter()
                .map(|e| e.event.as_str())
                .collect::<Vec<_>>(),
            vec![
                roost_ipc::messages::ops::EVENT_TAB_EFFECT,
                roost_ipc::messages::ops::EVENT_NOTIFICATION_FIRED,
            ]
        );

        let observer: EventBatch =
            serde_json::from_value(batch_value(&commit, false).expect("a wire form")).unwrap();
        assert_eq!(observer.revision, 12);
        assert_eq!(
            observer
                .events
                .iter()
                .map(|e| e.event.as_str())
                .collect::<Vec<_>>(),
            vec![roost_ipc::messages::ops::EVENT_NOTIFICATION_FIRED],
            "notification.fired crosses to observers; only tab.effect is driver-only"
        );
    }

    /// A commit whose only event was filtered is still a batch — see
    /// [`batch_value`] for why the alternative reads as loss.
    #[test]
    fn a_wholly_filtered_commit_ships_as_an_empty_batch() {
        let commit = VersionedWorkspaceEvent {
            revision: 3,
            events: vec![WorkspaceEvent::TabEffect {
                tab_id: 1,
                effect: TabEffectKind::ClipboardWrite {
                    text: "secret".into(),
                    target: roost_ipc::messages::ClipboardEffectTarget::System,
                },
            }],
        };
        let observer: EventBatch =
            serde_json::from_value(batch_value(&commit, false).expect("a wire form")).unwrap();
        assert_eq!(observer.revision, 3);
        assert!(observer.events.is_empty());
    }

    // ================================================================
    // Resuming: the replay half of a cut (plan 052 §3.6)
    // ================================================================

    fn effect_batch(revision: u64) -> VersionedWorkspaceEvent {
        VersionedWorkspaceEvent {
            revision,
            events: vec![WorkspaceEvent::TabEffect {
                tab_id: 1,
                effect: TabEffectKind::Bell,
            }],
        }
    }

    async fn next_value(rx: &mut mpsc::Receiver<serde_json::Value>) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the relay must answer")
            .expect("a frame")
    }

    /// The replay comes first and the live half picks up exactly where
    /// it stopped — including dropping the fence's own revision, which
    /// the replay already carried.
    #[tokio::test]
    async fn a_cut_drains_its_replay_before_its_live_batches() {
        let (events, rx) = tokio::sync::broadcast::channel(16);
        let (tx, mut source) = mpsc::channel(8);
        let cut = ResumeCut {
            rx,
            replay: vec![batch(8), batch(9)],
            fence: 9,
        };
        let task = tokio::spawn(relay(cut, tx, TEST_LIMITS, Arc::new(FullFeed)));

        // The broadcast still holds the fence's own commit; the replay
        // already delivered it, so it must not go out twice.
        events.send(batch(9)).unwrap();
        events.send(batch(10)).unwrap();

        assert_eq!(next(&mut source).await.expect("a batch").revision, 8);
        assert_eq!(next(&mut source).await.expect("a batch").revision, 9);
        assert_eq!(next(&mut source).await.expect("a batch").revision, 10);

        drop(events);
        assert!(next(&mut source).await.is_none());
        task.await.expect("the relay ends with its broadcast");
    }

    /// A full-window replay is far longer than the queue it drains into,
    /// so most of it is delivered under backpressure — in order, one
    /// reservation at a time, with no batch overtaking another.
    #[tokio::test]
    async fn a_replay_longer_than_the_queue_drains_in_order() {
        let (events, rx) = tokio::sync::broadcast::channel(16);
        let (tx, mut source) = mpsc::channel(2);
        let cut = ResumeCut {
            rx,
            replay: (1..=12).map(batch).collect(),
            fence: 12,
        };
        let task = tokio::spawn(relay(
            cut,
            tx,
            PushLimits {
                capacity: 2,
                stall: Duration::from_secs(5),
            },
            Arc::new(FullFeed),
        ));

        let mut seen = Vec::new();
        for _ in 1..=12 {
            seen.push(next(&mut source).await.expect("a batch").revision);
        }
        assert_eq!(seen, (1..=12).collect::<Vec<_>>());

        drop(events);
        assert!(next(&mut source).await.is_none());
        task.await.expect("the relay ends with its broadcast");
    }

    /// A takeover that lands mid-replay: the gate spends the next permit
    /// on its `session.driver_changed`, and the batch it interrupted is
    /// re-offered and classified *after* it — so the effect that would
    /// have followed the announcement is filtered instead.
    struct NoticeAfterFirst {
        delivered: std::sync::atomic::AtomicUsize,
        deposed: std::sync::atomic::AtomicBool,
    }

    impl StreamGate for NoticeAfterFirst {
        fn deliver(
            &self,
            permit: mpsc::Permit<'_, serde_json::Value>,
            batch: &VersionedWorkspaceEvent,
        ) -> Delivery {
            use std::sync::atomic::Ordering::SeqCst;
            if self.delivered.load(SeqCst) == 1 && !self.deposed.swap(true, SeqCst) {
                permit.send(serde_json::json!({ "event": "session.driver_changed" }));
                return Delivery::NoticeSentRetryBatch;
            }
            let driver = !self.deposed.load(SeqCst);
            match batch_value(batch, driver) {
                Some(value) => {
                    permit.send(value);
                    self.delivered.fetch_add(1, SeqCst);
                    Delivery::Delivered
                }
                None => Delivery::End,
            }
        }
    }

    #[tokio::test]
    async fn a_notice_parked_mid_replay_precedes_the_batch_it_reclassifies() {
        let (events, rx) = tokio::sync::broadcast::channel(16);
        let (tx, mut source) = mpsc::channel(8);
        let cut = ResumeCut {
            rx,
            replay: vec![effect_batch(1), effect_batch(2)],
            fence: 2,
        };
        let task = tokio::spawn(relay(
            cut,
            tx,
            TEST_LIMITS,
            Arc::new(NoticeAfterFirst {
                delivered: std::sync::atomic::AtomicUsize::new(0),
                deposed: std::sync::atomic::AtomicBool::new(false),
            }),
        ));

        let first: EventBatch = serde_json::from_value(next_value(&mut source).await).unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            vec![ops::EVENT_TAB_EFFECT]
        );

        let notice = next_value(&mut source).await;
        assert_eq!(
            notice["event"], "session.driver_changed",
            "the notice comes out ahead of the batch it interrupted"
        );

        let second: EventBatch = serde_json::from_value(next_value(&mut source).await).unwrap();
        assert_eq!(second.revision, 2);
        assert!(
            second.events.is_empty(),
            "the re-offered batch is classified after the notice, so its effect is filtered"
        );

        drop(events);
        assert!(next(&mut source).await.is_none());
        task.await.expect("the relay ends with its broadcast");
    }

    /// The dynamic the module doc calls out: a slow drain lets the live
    /// receiver lag, and the stream closes mid-stride. It is not a dead
    /// end — the client resumes from what it did receive, and the next
    /// cut carries on.
    #[tokio::test]
    async fn a_live_lag_during_a_replay_closes_and_the_next_cut_continues() {
        let (events, rx) = tokio::sync::broadcast::channel(2);
        let (tx, mut source) = mpsc::channel(8);
        let cut = ResumeCut {
            rx,
            replay: vec![batch(1), batch(2), batch(3)],
            fence: 3,
        };
        // More live commits than the broadcast holds, none of them read
        // while the replay is draining.
        for revision in 4..=9 {
            events.send(batch(revision)).unwrap();
        }
        let task = tokio::spawn(relay(cut, tx, TEST_LIMITS, Arc::new(FullFeed)));

        for revision in 1..=3 {
            assert_eq!(next(&mut source).await.expect("a batch").revision, revision);
        }
        assert!(
            next(&mut source).await.is_none(),
            "the lagged live half closes the stream rather than skipping revisions"
        );
        task.await.expect("the relay ends on the lag");

        // The client comes back with the fence it actually reached.
        let mut subscription = spawn(
            ResumeCut {
                rx: events.subscribe(),
                replay: vec![batch(4), batch(5)],
                fence: 5,
            },
            TEST_LIMITS,
            Arc::new(FullFeed),
        );
        assert_eq!(subscription.revision, 3, "the ack is what the client has");
        events.send(batch(6)).unwrap();
        for revision in 4..=6 {
            let value = tokio::time::timeout(Duration::from_secs(5), subscription.source.next())
                .await
                .expect("the source must answer")
                .expect("a frame");
            let batch: EventBatch = serde_json::from_value(value).unwrap();
            assert_eq!(batch.revision, revision);
        }
    }
}
