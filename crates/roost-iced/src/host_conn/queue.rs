//! The per-host op queue (plan 037 §3.9).
//!
//! `IpcClient` is strictly sequential, so every control-plane op a host
//! needs — the workspace mutations C6/C7 route here, `session.set_theme`
//! — has to go through one place in one order. UI intents enqueue on a
//! bounded channel; the connection task is the single worker that drains
//! it.
//!
//! One connection, one order, no interleaving hazards. And because the
//! queue is bounded, a wedged session costs a bounded amount of memory
//! and a `Full` intent is refused at the enqueue rather than swallowed.
//!
//! Two things deliberately do **not** ride the queue, and for the same
//! reason: the worker awaits each op inline, so anything slow on it
//! blocks every later control op. Uploads go to a lane of their own
//! ([`HostOps::put_file`]), and an attach dials a data connection of its
//! own. What the attach needs from the queue is not service but
//! *position*, which is what [`HostOps::attach_permit`] takes.

use std::borrow::Cow;
use std::sync::Arc;

use roost_ipc::client::{ClientError, ServerCode};
use roost_ipc::messages::SessionPutFileResult;
use roost_ui_model::keys::HostId;
use tokio::sync::{mpsc, oneshot, watch};

use super::upload::{UploadSource, Uploads};

/// How many intents may be waiting on a host before enqueuing fails.
///
/// Generous enough that a burst of reorder ops never trips it, small
/// enough that a session that stopped answering cannot grow the client
/// without bound. Overflow surfaces as an error on the intent, which is
/// the honest outcome: the mutation did not happen.
const QUEUE_DEPTH: usize = 256;

/// The label a barrier carries in the worker's logs. Not a wire op —
/// see [`HostIntent::barrier`].
pub(crate) const BARRIER_OP: &str = "attach-permit";

/// Why an intent did not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostOpError {
    /// The queue was flushed because the connection left `Connected`
    /// before this intent reached the wire. Plan 037 §3.9's "a queue
    /// stuck behind a dead connection is flushed with errors".
    Disconnected,
    /// The session refused it. Typed rather than string-compared so a
    /// caller matches instead of grepping.
    Rejected { code: ServerCode, message: String },
    /// The wire died mid-op. The connection is going down with it.
    Transport(String),
    /// The queue was full, or the connection task is already gone.
    Unavailable,
    /// **This client** refused it: an upload's file could not be read,
    /// or the session's answer was not one the client can use (plan 047
    /// §3.1's reply re-check). Nothing is wrong with the connection, and
    /// nothing was wrong with the request — the message is the whole
    /// story.
    Local(String),
}

impl std::fmt::Display for HostOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostOpError::Disconnected => f.write_str("the host disconnected before this ran"),
            HostOpError::Rejected { code, message } => {
                write!(f, "{}: {message}", code.as_str())
            }
            HostOpError::Transport(error) => write!(f, "connection lost: {error}"),
            HostOpError::Unavailable => f.write_str("the host is not accepting operations"),
            HostOpError::Local(message) => f.write_str(message),
        }
    }
}

/// Where a reply goes. `None` is fire-and-forget: the worker logs a
/// failure and moves on.
pub(crate) type HostOpReply = oneshot::Sender<Result<serde_json::Value, HostOpError>>;

/// One queued control-plane op.
#[derive(Debug)]
pub(crate) struct HostIntent {
    pub(crate) op: Cow<'static, str>,
    pub(crate) params: serde_json::Value,
    /// The incarnation this intent was issued *for*, when the caller
    /// cared.
    ///
    /// The queue outlives a connection: the task's own retry ladder
    /// keeps draining the same receiver across attempts, so an intent
    /// enqueued after a drop but before the main thread has drained the
    /// `Disconnected` off the feed would otherwise be served by the
    /// **replacement** connection. Against a session that restarted and
    /// re-minted its ids, a forwarded `project.delete {"project_id":
    /// "1"}` would then delete a project the caller never named.
    ///
    /// `None` is the unfenced form every existing call site keeps —
    /// administrative ops (a theme push, an `agent-hooks` raise) that are
    /// about the connection rather than about a row on it, and are
    /// correct on whichever connection serves them.
    pub(crate) fence: Option<HostId>,
    pub(crate) reply: Option<HostOpReply>,
    /// The ordering barrier: the worker answers this item itself, where
    /// it stands, instead of putting it on the wire. Reaching it *is*
    /// the answer — see [`HostOps::attach_permit`], which is the only
    /// thing that makes one.
    pub(crate) barrier: bool,
}

impl HostIntent {
    /// A fire-and-forget administrative op.
    pub(crate) fn new(op: impl Into<Cow<'static, str>>, params: serde_json::Value) -> Self {
        Self {
            op: op.into(),
            params,
            fence: None,
            reply: None,
            barrier: false,
        }
    }

    /// A queue position and nothing else. See [`Self::barrier`]; the
    /// `op` is a label for the worker's logs, not a wire op.
    fn barrier() -> Self {
        Self {
            barrier: true,
            ..Self::new(BARRIER_OP, serde_json::Value::Null)
        }
    }

    /// Bind this intent to the connection it was issued for. See
    /// [`Self::fence`].
    pub(crate) fn fenced_at(mut self, incarnation: HostId) -> Self {
        self.fence = Some(incarnation);
        self
    }

    /// Route the outcome to `reply`.
    pub(crate) fn answering(mut self, reply: HostOpReply) -> Self {
        self.reply = Some(reply);
        self
    }

    /// Hand a result to whoever is waiting, if anyone is.
    pub(crate) fn answer(self, outcome: Result<serde_json::Value, HostOpError>) {
        let op = self.op;
        match self.reply {
            Some(reply) => {
                // A dropped receiver means the caller lost interest —
                // an ordinary outcome, not a fault.
                let _ = reply.send(outcome);
            }
            None => {
                if let Err(error) = outcome {
                    tracing::warn!(%op, %error, "host op failed with nobody listening");
                }
            }
        }
    }
}

/// Which incarnation this host's connection task is serving right now,
/// or `None` between connections.
///
/// [`HostIntent::fence`] covers an intent only while it is *waiting* in
/// the queue. A permit is different: it is answered where it stands and
/// everything it releases — dial, handshake, snapshot — happens later,
/// on a socket of its own, with the worker already on to the next op. So
/// the fence has to outlive the answer, and this is where it lives. See
/// [`AttachPermit::guarding`].
#[derive(Debug, Clone)]
pub(crate) struct Serving(Arc<watch::Sender<Option<HostId>>>);

impl Default for Serving {
    fn default() -> Self {
        Serving(Arc::new(watch::channel(None).0))
    }
}

impl Serving {
    /// Name `incarnation` as the one being served, for as long as the
    /// returned hold lives — the connection task opens one at its
    /// `Connected` edge and drops it on every way out, exactly as it
    /// does the upload lane ([`Uploads::open`]).
    pub(crate) fn open(&self, incarnation: HostId) -> Hold {
        self.0.send_replace(Some(incarnation));
        Hold {
            serving: self.clone(),
            incarnation,
        }
    }
}

/// One incarnation's hold on [`Serving`].
pub(crate) struct Hold {
    serving: Serving,
    incarnation: HostId,
}

impl Drop for Hold {
    fn drop(&mut self) {
        // Only if it still names *mine*: a hold can outlive the install
        // of its successor — the connection loop's next attempt starts
        // whether or not the last one has finished unwinding — so an
        // unconditional clear would unfence the incarnation after this
        // one (the hazard [`Uploads::close`] guards the same way).
        self.serving.0.send_if_modified(|held| {
            let mine = *held == Some(self.incarnation);
            if mine {
                *held = None;
            }
            mine
        });
    }
}

/// A granted queue permit: the attach's place in line, and its licence
/// to dial.
///
/// The dial goes *through* the permit rather than beside it, because the
/// grant is a statement about where the queue stood and not a promise
/// about the future. The worker answers a barrier and moves on, so the
/// connection the permit was granted for can end before the socket is
/// even opened — and if that session's process is still listening (only
/// the control or event leg died, say), the handshake's `session_id`
/// matches and the stale attach is accepted, resizing the tab for a
/// connection this client has already lost. Dialing through the permit
/// is what makes that impossible to forget.
#[derive(Debug)]
#[must_use = "a permit is only good for the dial it guards"]
pub(crate) struct AttachPermit {
    incarnation: HostId,
    serving: watch::Receiver<Option<HostId>>,
}

impl AttachPermit {
    /// Run `dial` only while the connection this permit was granted for
    /// is still the one being served, and abandon it the moment that
    /// stops being true.
    pub(crate) async fn guarding<T>(
        mut self,
        dial: impl std::future::Future<Output = T>,
    ) -> Result<T, HostOpError> {
        let mine = self.incarnation;
        tokio::select! {
            // Biased so the fence is read before the dial is polled: a
            // permit whose connection has already gone opens nothing at
            // all. `wait_for` checks the current value before it waits,
            // and a sender that went away resolves it too — nothing is
            // being served if there is nobody left to say so.
            biased;
            _ = self.serving.wait_for(|held| *held != Some(mine)) => {
                Err(HostOpError::Disconnected)
            }
            dialed = dial => Ok(dialed),
        }
    }
}

/// The UI-side handle. Cloneable, cheap, and never blocks: enqueuing
/// happens on the main thread, so a full queue is refused rather than
/// awaited.
///
/// [`HostOps::queued_for_test`] is how a set-level case says "nothing
/// was enqueued": the worker owns the receiving half, so there is no
/// other way to look.
#[derive(Debug, Clone)]
pub(crate) struct HostOps {
    tx: mpsc::Sender<HostIntent>,
    /// The upload lane, which shares nothing with the queue above but
    /// the handle that reaches it. See [`upload`].
    uploads: Uploads,
    /// What a granted permit is fenced against. See [`Serving`].
    serving: Serving,
}

impl HostOps {
    /// How many intents are queued and unread. See the struct doc.
    #[cfg(test)]
    pub(crate) fn queued_for_test(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    pub(crate) fn channel() -> (HostOps, mpsc::Receiver<HostIntent>) {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        (
            HostOps {
                tx,
                uploads: Uploads::default(),
                serving: Serving::default(),
            },
            rx,
        )
    }

    /// The slot the connection task fills at its `Connected` edge.
    pub(crate) fn uploads(&self) -> Uploads {
        self.uploads.clone()
    }

    /// The fence the connection task holds open across a `Connected`
    /// incarnation. See [`Serving`].
    pub(crate) fn serving(&self) -> Serving {
        self.serving.clone()
    }

    /// Send one file to the host, and wait for the path it landed at.
    ///
    /// Deliberately **not** an intent: the queue above is drained by a
    /// loop that awaits each op inline, so an upload on it could not
    /// even be admitted while a control op was in flight, and its
    /// timeout would drop the host. It goes to the per-incarnation
    /// dispatcher instead, on a connection of its own.
    ///
    /// Like [`Self::call`], the future always resolves: a dispatcher
    /// that went away mid-upload reads as [`HostOpError::Disconnected`],
    /// which is what happened.
    pub(crate) fn put_file(
        &self,
        name: String,
        source: UploadSource,
    ) -> impl std::future::Future<Output = Result<SessionPutFileResult, HostOpError>> + Send + 'static
    {
        let queued = self.uploads.enqueue(name, source);
        async move { queued?.await.unwrap_or(Err(HostOpError::Disconnected)) }
    }

    /// Enqueue. The intent's own reply channel carries the outcome; the
    /// `Err` here is only the enqueue failing.
    pub(crate) fn send(&self, intent: HostIntent) -> Result<(), HostOpError> {
        match self.tx.try_send(intent) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(intent)) => {
                tracing::warn!(op = %intent.op, "host op queue is full");
                intent.answer(Err(HostOpError::Unavailable));
                Err(HostOpError::Unavailable)
            }
            Err(mpsc::error::TrySendError::Closed(intent)) => {
                intent.answer(Err(HostOpError::Unavailable));
                Err(HostOpError::Unavailable)
            }
        }
    }

    /// Enqueue and await the result.
    ///
    /// The future always resolves to a `HostOpError` rather than to a
    /// channel cancellation: an intent whose reply channel is dropped
    /// unanswered — the worker went away mid-op — reads as
    /// [`HostOpError::Disconnected`], which is what actually happened.
    /// A caller never sees a bare `RecvError` and never waits forever.
    pub(crate) fn call(
        &self,
        op: impl Into<Cow<'static, str>>,
        params: serde_json::Value,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, HostOpError>> + Send + 'static
    {
        self.dispatch(HostIntent::new(op, params))
    }

    /// Take a place in this host's queue and wait for the worker to
    /// reach it. The attach's ordering device (plan 065 §3.10).
    ///
    /// A data connection is a socket of its own, so nothing about
    /// dialing it is ordered against the control ops the UI has already
    /// asked for — `session.set_theme` most of all, whose colors the
    /// session needs *before* it composes a snapshot. Until #473 the
    /// attach's ticket mint was itself a queued op and the order came
    /// for free; the inline handshake has no control leg, so the order
    /// has to be taken deliberately.
    ///
    /// The permit restores it without putting the attach on the queue:
    /// the worker answers this item the moment it reaches it — which is
    /// the moment every item enqueued before it has been answered — and
    /// moves straight on to the next one. It never awaits the dial, the
    /// handshake or the snapshot. An attach on the queue would block
    /// every later control op for the whole attach timeout, which is the
    /// same reason uploads are not on it either ([`Self::put_file`]).
    ///
    /// Fenced at `incarnation` twice over, because a permit is a
    /// statement about one connection: [`HostIntent::fence`] while it
    /// waits in the queue, and [`AttachPermit::guarding`] for everything
    /// it releases afterwards. An attach must not dial the session that
    /// replaced the connection it was asked of, whether the replacement
    /// happened before the grant or after it.
    pub(crate) fn attach_permit(
        &self,
        incarnation: HostId,
    ) -> impl std::future::Future<Output = Result<AttachPermit, HostOpError>> + Send + 'static {
        let granted = self.dispatch(HostIntent::barrier().fenced_at(incarnation));
        // Subscribed before the wait, so a connection that ends between
        // the grant and the first read is still seen.
        let serving = self.serving.0.subscribe();
        async move {
            granted.await?;
            Ok(AttachPermit {
                incarnation,
                serving,
            })
        }
    }

    /// [`Self::call`] bound to the incarnation it was issued for — see
    /// [`HostIntent::fence`]. The form every op that names a *row* on
    /// the session should use.
    pub(crate) fn call_at(
        &self,
        incarnation: HostId,
        op: impl Into<Cow<'static, str>>,
        params: serde_json::Value,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, HostOpError>> + Send + 'static
    {
        self.dispatch(HostIntent::new(op, params).fenced_at(incarnation))
    }

    fn dispatch(
        &self,
        intent: HostIntent,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, HostOpError>> + Send + 'static
    {
        let (tx, rx) = oneshot::channel();
        let intent = intent.answering(tx);
        // `send` already answers the intent on failure, so the receiver
        // resolves either way and no caller waits forever.
        let _ = self.send(intent);
        async move { rx.await.unwrap_or(Err(HostOpError::Disconnected)) }
    }
}

/// Answer everything still queued with `Disconnected`, leaving the
/// senders usable.
///
/// Called the moment the state machine leaves `Connected` *and the task
/// intends to try again*: an intent behind a dead connection has no way
/// to succeed later, and leaving it queued would let it run against the
/// *next* incarnation — a mutation the user asked of a session that is
/// gone, applied to its replacement. The handle stays open because the
/// same handle serves the reconnect.
pub(crate) fn flush(rx: &mut mpsc::Receiver<HostIntent>, error: &HostOpError) {
    while let Ok(intent) = rx.try_recv() {
        intent.answer(Err(error.clone()));
    }
}

/// Close the queue, then answer everything left on it.
///
/// The worker's *final* flush, and the order is the whole point: a plain
/// drain races the senders, because an intent enqueued between the last
/// `try_recv` and the task returning is one nobody will ever answer.
/// `close` first means no further send can land, so the drain that
/// follows is exhaustive; a sender that races it gets
/// [`mpsc::error::TrySendError::Closed`], which [`HostOps::send`] already
/// answers as [`HostOpError::Unavailable`].
pub(crate) fn close_and_flush(rx: &mut mpsc::Receiver<HostIntent>, error: &HostOpError) {
    rx.close();
    flush(rx, error);
}

/// What an op failure means for the *connection*, as opposed to for the
/// caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpFault {
    /// `shutting-down`: the session has latched a stop.
    ShuttingDown,
    /// The wire died.
    Transport(String),
    /// An ordinary refusal. The connection is fine; the caller hears
    /// about it.
    Surfaced,
}

/// Map a client error onto its connection-level meaning and the error
/// the caller is handed.
pub(crate) fn classify(error: &ClientError) -> (OpFault, HostOpError) {
    match error.server_code() {
        // Every refusal reaches the caller the same way; the code only
        // decides what it means for the connection.
        Some(code) => {
            let fault = match code {
                ServerCode::ShuttingDown => OpFault::ShuttingDown,
                _ => OpFault::Surfaced,
            };
            (
                fault,
                HostOpError::Rejected {
                    code,
                    message: server_message(error),
                },
            )
        }
        // Not a refusal: the transport or the schema. Either way this
        // connection is finished — a client that cannot decode what the
        // session says has nothing to gain by asking again on it.
        None => {
            let rendered = error.to_string();
            (
                OpFault::Transport(rendered.clone()),
                HostOpError::Transport(rendered),
            )
        }
    }
}

fn server_message(error: &ClientError) -> String {
    match error {
        ClientError::Server { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(op: &'static str) -> HostIntent {
        HostIntent::new(op, serde_json::json!({}))
    }

    #[tokio::test]
    async fn intents_drain_in_the_order_they_were_enqueued() {
        let (ops, mut rx) = HostOps::channel();
        for op in ["a", "b", "c"] {
            ops.send(intent(op)).unwrap();
        }
        let drained: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|intent| intent.op.into_owned())
            .collect();
        assert_eq!(drained, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn a_flush_answers_every_queued_intent_with_disconnected() {
        let (ops, mut rx) = HostOps::channel();
        let waiting: Vec<_> = (0..3)
            .map(|_| ops.call("tab.open", serde_json::json!({})))
            .collect();

        flush(&mut rx, &HostOpError::Disconnected);

        for waiter in waiting {
            assert_eq!(
                waiter.await,
                Err(HostOpError::Disconnected),
                "a queued intent must not survive the connection"
            );
        }
        assert!(rx.try_recv().is_err(), "the queue is empty afterwards");
    }

    /// The worker's final flush closes before it drains, so an intent
    /// enqueued in the window a plain drain would race is refused at the
    /// enqueue instead of being stranded on a channel nobody reads.
    #[tokio::test]
    async fn the_final_flush_closes_first_so_no_straggler_is_stranded() {
        let (ops, mut rx) = HostOps::channel();
        let queued: Vec<_> = (0..3)
            .map(|_| ops.call("tab.open", serde_json::json!({})))
            .collect();

        close_and_flush(&mut rx, &HostOpError::Disconnected);

        for waiter in queued {
            assert_eq!(waiter.await, Err(HostOpError::Disconnected));
        }
        // The window the race lived in: a send *after* the drain.
        assert_eq!(
            ops.call("tab.open", serde_json::json!({})).await,
            Err(HostOpError::Unavailable),
            "a closed queue refuses at the enqueue rather than swallowing"
        );
        assert!(rx.try_recv().is_err());
    }

    /// The worker aborted mid-op without answering. The caller must hear
    /// `Disconnected`, not a raw channel cancellation.
    #[tokio::test]
    async fn a_reply_channel_dropped_unanswered_reads_as_disconnected() {
        let (ops, mut rx) = HostOps::channel();
        let waiting = ops.call("tab.open", serde_json::json!({}));

        // Exactly what an aborted task leaves behind: the intent taken
        // off the queue and then dropped with its reply unsent.
        let intent = rx.try_recv().expect("the intent was enqueued");
        drop(intent);

        assert_eq!(waiting.await, Err(HostOpError::Disconnected));
    }

    /// The handle survives a flush: reconnecting reuses it, so a
    /// post-flush enqueue must still land.
    #[tokio::test]
    async fn the_handle_still_works_after_a_flush() {
        let (ops, mut rx) = HostOps::channel();
        ops.send(intent("first")).unwrap();
        flush(&mut rx, &HostOpError::Disconnected);

        ops.send(intent("second")).unwrap();
        assert_eq!(rx.try_recv().unwrap().op, "second");
    }

    #[tokio::test]
    async fn a_full_queue_refuses_rather_than_blocking() {
        let (ops, _rx) = HostOps::channel();
        for _ in 0..QUEUE_DEPTH {
            ops.send(intent("fill")).unwrap();
        }
        let overflow = ops.call("tab.open", serde_json::json!({}));
        assert_eq!(overflow.await, Err(HostOpError::Unavailable));
    }

    /// The permit takes its place in line like anything else — which is
    /// the whole of what it does.
    #[tokio::test]
    async fn a_permit_queues_behind_the_ops_enqueued_before_it() {
        let (ops, mut rx) = HostOps::channel();
        ops.send(intent("session.set_theme")).unwrap();
        let _permit = ops.attach_permit(HostId::new(1));
        ops.send(intent("tab.close")).unwrap();

        let queued: Vec<(String, bool)> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|intent| (intent.op.into_owned(), intent.barrier))
            .collect();
        assert_eq!(
            queued,
            vec![
                ("session.set_theme".to_string(), false),
                (BARRIER_OP.to_string(), true),
                ("tab.close".to_string(), false),
            ]
        );
    }

    /// Grant a permit the way the worker does: take the barrier off the
    /// queue and answer it where it stands.
    async fn granted(
        ops: &HostOps,
        rx: &mut mpsc::Receiver<HostIntent>,
        incarnation: HostId,
    ) -> AttachPermit {
        let waiting = ops.attach_permit(incarnation);
        let intent = rx.recv().await.expect("the barrier was enqueued");
        assert!(intent.barrier, "and it is a barrier");
        intent.answer(Ok(serde_json::Value::Null));
        waiting.await.expect("the grant")
    }

    /// **A permit does not outlive the connection that granted it.**
    ///
    /// The grant says where the queue stood, not what is still true when
    /// the dial finally happens — and the handshake's `session_id` does
    /// not cover this, because the session process it names may well
    /// still be listening.
    #[tokio::test]
    async fn a_granted_permit_only_dials_for_the_connection_it_was_granted_for() {
        let (ops, mut rx) = HostOps::channel();
        let first = HostId::new(1);

        let held = ops.serving().open(first);
        let permit = granted(&ops, &mut rx, first).await;
        assert_eq!(
            permit.guarding(std::future::ready("dialed")).await,
            Ok("dialed"),
            "a permit whose connection is still the one being served dials"
        );

        // The connection ended between the grant and the dial.
        let permit = granted(&ops, &mut rx, first).await;
        drop(held);
        assert_eq!(
            permit.guarding(std::future::ready("dialed")).await,
            Err(HostOpError::Disconnected),
            "an ended connection refuses before anything is opened"
        );

        // And a replacement is not a continuation of it.
        let held = ops.serving().open(first);
        let permit = granted(&ops, &mut rx, first).await;
        let _next = ops.serving().open(HostId::new(2));
        drop(held);
        assert_eq!(
            permit.guarding(std::future::ready("dialed")).await,
            Err(HostOpError::Disconnected),
            "the session that replaced it is not the one this attach was asked of"
        );
    }

    /// The same fence mid-flight: a dial is abandoned the moment its
    /// connection stops being the one served, rather than landing an
    /// attach — and a focused attach's resize — on a session this client
    /// has already lost.
    #[tokio::test]
    async fn a_dial_in_flight_is_abandoned_when_its_connection_ends() {
        let (ops, mut rx) = HostOps::channel();
        let first = HostId::new(1);
        let held = ops.serving().open(first);
        let permit = granted(&ops, &mut rx, first).await;

        let dialing = tokio::spawn(permit.guarding(std::future::pending::<()>()));
        drop(held);

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), dialing)
                .await
                .expect("the dial is abandoned rather than parked")
                .expect("and its task does not panic"),
            Err(HostOpError::Disconnected)
        );
    }

    /// A hold can outlive the install of its successor — the connection
    /// loop's next attempt starts whether or not the last one has
    /// finished unwinding — so dropping the old one must not unfence the
    /// new one.
    #[tokio::test]
    async fn a_late_hold_drop_does_not_unfence_the_incarnation_after_it() {
        let (ops, mut rx) = HostOps::channel();
        let first = ops.serving().open(HostId::new(1));
        let _second = ops.serving().open(HostId::new(2));
        drop(first);

        let permit = granted(&ops, &mut rx, HostId::new(2)).await;
        assert_eq!(
            permit.guarding(std::future::ready("dialed")).await,
            Ok("dialed")
        );
    }

    /// The two ways a permit is refused, and they are the only two: a
    /// barrier never reaches the wire, so it cannot be rejected by a
    /// session or die with a transport.
    #[tokio::test]
    async fn a_permit_is_refused_by_a_drop_and_by_a_full_queue() {
        let (ops, mut rx) = HostOps::channel();
        let flushed = ops.attach_permit(HostId::new(1));
        flush(&mut rx, &HostOpError::Disconnected);
        assert_eq!(flushed.await.unwrap_err(), HostOpError::Disconnected);

        for _ in 0..QUEUE_DEPTH {
            ops.send(intent("fill")).unwrap();
        }
        assert_eq!(
            ops.attach_permit(HostId::new(1)).await.unwrap_err(),
            HostOpError::Unavailable
        );
    }

    /// A caller that awaits a reply must never wait forever, including
    /// when the connection task has already gone.
    #[tokio::test]
    async fn a_dead_worker_answers_immediately() {
        let (ops, rx) = HostOps::channel();
        drop(rx);
        let outcome = ops.call("tab.open", serde_json::json!({})).await;
        assert_eq!(outcome, Err(HostOpError::Unavailable));
    }

    #[test]
    fn shutting_down_routes_to_stopped() {
        let (fault, _) = classify(&ClientError::Server {
            code: "shutting-down".into(),
            message: "latched".into(),
        });
        assert_eq!(fault, OpFault::ShuttingDown);
    }

    #[test]
    fn an_ordinary_refusal_surfaces_without_faulting_the_connection() {
        let (fault, error) = classify(&ClientError::Server {
            code: "invalid-param".into(),
            message: "cols must be positive".into(),
        });
        assert_eq!(fault, OpFault::Surfaced);
        assert_eq!(
            error,
            HostOpError::Rejected {
                code: ServerCode::InvalidParam,
                message: "cols must be positive".into()
            }
        );
    }

    #[test]
    fn a_transport_failure_faults_the_connection() {
        let (fault, error) = classify(&ClientError::Disconnected);
        assert!(matches!(fault, OpFault::Transport(_)));
        assert!(matches!(error, HostOpError::Transport(_)));
    }

    /// An unknown code keeps its spelling all the way to the caller —
    /// a newer session's refusal must still be readable in a toast.
    #[test]
    fn an_unrecognized_code_survives_the_mapping() {
        let (fault, error) = classify(&ClientError::Server {
            code: "some-future-code".into(),
            message: "…".into(),
        });
        assert_eq!(fault, OpFault::Surfaced);
        let HostOpError::Rejected { code, .. } = error else {
            panic!("expected a refusal");
        };
        assert_eq!(code.as_str(), "some-future-code");
    }
}
