//! The per-host op queue (plan 037 §3.9).
//!
//! `IpcClient` is strictly sequential, so every control-plane op a host
//! needs — the workspace mutations C6/C7 route here, `tab.attach` token
//! minting, `session.set_theme` — has to go through one place in one
//! order. UI intents enqueue on a bounded channel; the connection task
//! is the single worker that drains it.
//!
//! One connection, one order, no interleaving hazards. And because the
//! queue is bounded, a wedged session costs a bounded amount of memory
//! and a `Full` intent is refused at the enqueue rather than swallowed.

use std::borrow::Cow;

use roost_ipc::client::{ClientError, ServerCode};
use roost_ipc::messages::SessionPutFileResult;
use tokio::sync::{mpsc, oneshot};

use super::upload::{UploadSource, Uploads};

/// How many intents may be waiting on a host before enqueuing fails.
///
/// Generous enough that a burst of reorder ops never trips it, small
/// enough that a session that stopped answering cannot grow the client
/// without bound. Overflow surfaces as an error on the intent, which is
/// the honest outcome: the mutation did not happen.
const QUEUE_DEPTH: usize = 256;

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
    /// Suppress the "nobody listening" warning on failure. For the ops
    /// whose refusal is an expected answer rather than a fault — a
    /// session one release older refusing `session.set_focus` — where
    /// the worker says it once per incarnation instead.
    pub(crate) quiet: bool,
    pub(crate) reply: Option<HostOpReply>,
}

impl HostIntent {
    /// A fire-and-forget administrative op.
    pub(crate) fn new(op: impl Into<Cow<'static, str>>, params: serde_json::Value) -> Self {
        Self {
            op: op.into(),
            params,
            quiet: false,
            reply: None,
        }
    }

    /// A failure on this op is not news. See [`Self::quiet`].
    pub(crate) fn quiet(mut self) -> Self {
        self.quiet = true;
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
                    if self.quiet {
                        tracing::debug!(%op, %error, "host op failed with nobody listening");
                    } else {
                        tracing::warn!(%op, %error, "host op failed with nobody listening");
                    }
                }
            }
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
            },
            rx,
        )
    }

    /// The slot the connection task fills at its `Connected` edge.
    pub(crate) fn uploads(&self) -> Uploads {
        self.uploads.clone()
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
        let (tx, rx) = oneshot::channel();
        let intent = HostIntent::new(op, params).answering(tx);
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
        let waiting = ops.call("tab.attach", serde_json::json!({}));

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
