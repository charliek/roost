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
//!   connection instead. The plain close *is* the resync signal: the
//!   client reconnects, re-subscribes, and re-pulls `tab.list`.

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
use crate::{VersionedWorkspaceEvent, Workspace, WorkspaceEvent};

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
    ///
    /// `false` means the batch has no wire form and the stream must end
    /// — the same "close rather than lie" answer the relay gives a
    /// [`WorkspaceEvent::Resync`].
    fn deliver(
        &self,
        permit: mpsc::Permit<'_, serde_json::Value>,
        batch: &VersionedWorkspaceEvent,
    ) -> bool;
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
    ) -> bool {
        match batch_value(batch, true) {
            Some(value) => {
                permit.send(value);
                true
            }
            None => false,
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
    /// is owed.
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

/// Subscribe `workspace` and start relaying its commits through `gate`.
///
/// The ordering is the contract: the broadcast is subscribed **before**
/// the revision is read, so no commit can slip through the gap. Commits
/// that land in that window arrive on the channel *and* are already
/// reflected in the revision, so the relay drops every batch at or below
/// it — leaving the client's first batch exactly `revision + 1`.
///
/// The task ends (dropping its sender, which closes the connection) on
/// any condition that would otherwise hide a loss: a lagged broadcast, a
/// queue that stays full past [`PushLimits::stall`], a `Resync`, or a
/// dropped receiver.
pub fn spawn(
    workspace: &Arc<Workspace>,
    limits: PushLimits,
    gate: Arc<dyn StreamGate>,
) -> Subscription {
    let rx = workspace.subscribe_versioned();
    let revision = workspace.revision();
    let (tx, source_rx) = mpsc::channel(limits.capacity.max(1));
    let inject = tx.downgrade();
    let task = tokio::spawn(relay(rx, tx, revision, limits, gate));
    Subscription {
        revision,
        // The queue bound and the socket-write bound are the same
        // policy seen from two sides, so they share one budget.
        source: PushSource::new(source_rx).with_write_deadline(limits.stall),
        abort: task.abort_handle(),
        inject,
    }
}

/// Relay `rx`'s batches into `tx`, dropping everything at or below
/// `fence`, and return the moment the stream must end.
///
/// Split out from [`spawn`] so the fence boundary and the teardown
/// conditions can be driven with a hand-fed channel, without a
/// workspace, a socket, or a race to set up.
///
/// Capacity is reserved *before* `gate` is consulted, and the gate then
/// classifies and enqueues in one step: waiting for room is the only
/// part that can block, and it must not happen under the embedder's
/// registry lock.
async fn relay(
    mut rx: tokio::sync::broadcast::Receiver<VersionedWorkspaceEvent>,
    tx: mpsc::Sender<serde_json::Value>,
    fence: u64,
    limits: PushLimits,
    gate: Arc<dyn StreamGate>,
) {
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
                let permit = match tokio::time::timeout(limits.stall, tx.reserve()).await {
                    Ok(Ok(permit)) => permit,
                    // Receiver gone: the connection is already down.
                    Ok(Err(_)) => return,
                    Err(_) => {
                        warn!(
                            revision = batch.revision,
                            "events subscriber is not draining; closing the connection"
                        );
                        return;
                    }
                };
                if !gate.deliver(permit, &batch) {
                    debug!(
                        revision = batch.revision,
                        "unpushable workspace event; closing the events connection"
                    );
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
        let task = tokio::spawn(relay(rx, tx, 7, TEST_LIMITS, Arc::new(FullFeed)));

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
        let task = tokio::spawn(relay(rx, tx, 0, TEST_LIMITS, Arc::new(FullFeed)));

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
}
