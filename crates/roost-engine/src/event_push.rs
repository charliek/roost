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
fn batch_value(batch: &VersionedWorkspaceEvent) -> Option<serde_json::Value> {
    let mut events = Vec::with_capacity(batch.events.len());
    for event in &batch.events {
        events.push(envelope(event)?);
    }
    // Infallible in practice: the envelopes are already `Value`s.
    serde_json::to_value(EventBatch {
        revision: batch.revision,
        events,
    })
    .ok()
}

/// Subscribe `workspace` and start relaying its commits.
///
/// Returns the revision the subscription starts from — the ack the
/// client is owed — plus the [`PushSource`] the server writes from and
/// an [`AbortHandle`] for the relay task.
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
pub fn spawn(workspace: &Arc<Workspace>, limits: PushLimits) -> (u64, PushSource, AbortHandle) {
    let rx = workspace.subscribe_versioned();
    let revision = workspace.revision();
    let (tx, source_rx) = mpsc::channel(limits.capacity.max(1));
    let task = tokio::spawn(relay(rx, tx, revision, limits));
    (
        revision,
        // The queue bound and the socket-write bound are the same
        // policy seen from two sides, so they share one budget.
        PushSource::new(source_rx).with_write_deadline(limits.stall),
        task.abort_handle(),
    )
}

/// Relay `rx`'s batches into `tx`, dropping everything at or below
/// `fence`, and return the moment the stream must end.
///
/// Split out from [`spawn`] so the fence boundary and the teardown
/// conditions can be driven with a hand-fed channel, without a
/// workspace, a socket, or a race to set up.
async fn relay(
    mut rx: tokio::sync::broadcast::Receiver<VersionedWorkspaceEvent>,
    tx: mpsc::Sender<serde_json::Value>,
    fence: u64,
    limits: PushLimits,
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
                let Some(value) = batch_value(&batch) else {
                    debug!(
                        revision = batch.revision,
                        "unpushable workspace event; closing the events connection"
                    );
                    return;
                };
                match tokio::time::timeout(limits.stall, tx.send(value)).await {
                    Ok(Ok(())) => {}
                    // Receiver gone: the connection is already down.
                    Ok(Err(_)) => return,
                    Err(_) => {
                        warn!(
                            revision = batch.revision,
                            "events subscriber is not draining; closing the connection"
                        );
                        return;
                    }
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
        let task = tokio::spawn(relay(rx, tx, 7, TEST_LIMITS));

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
        let task = tokio::spawn(relay(rx, tx, 0, TEST_LIMITS));

        drop(source);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the relay must end when its receiver goes away")
            .expect("relay task");

        // Nothing was ever committed, and the broadcast is still open —
        // the wake came from the disconnect alone.
        assert_eq!(events.receiver_count(), 0);
    }
}
