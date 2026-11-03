//! One host's connection task for a client that is not iced: prologue,
//! mirror, event pump, control-plane ops and reconnect, all over a
//! [`crate::dial::Dialer`].
//!
//! **Drop order.** [`HostClient`]'s `Drop` only *signals* its task (a
//! flag and a wake), so it never needs the runtime and is safe to run
//! after the runtime is gone — but then the task was already cancelled
//! with the runtime, and nothing wound down in order. An owner that wants
//! the wind-down to happen (sockets closed by the task, `Idle` published)
//! drops its `HostClient`s — and the [`crate::attach::AttachStream`]s
//! made from them, whose `DETACH` is written by their own task — before
//! the runtime they were spawned on.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::FutureExt;
use roost_ipc::client::{ClientError, EventFrame, EventStream, ServerCode};
use roost_ipc::messages::{
    ops, ClientHelloParams, EventBatch, NotificationFiredEvent, SessionIdentify,
    SessionIdentifyParams, TabListResult,
};
use roost_ipc::session_launch::IPC_TIMEOUT;
use roost_ipc::IpcClient;
use tokio::sync::{mpsc, oneshot, watch, Notify};

use crate::dial::{DialError, Dialer};
use crate::mirror::{HostMirror, SharedMirror};
use crate::state::{
    check_compatibility, Backoff, BuildMismatch, Compatibility, MismatchKind, RestartAction,
};
use crate::task::{jitter, scale, Shutdown};

/// Who this client is to every session it reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostClientConfig {
    /// Stable per install: 1–64 bytes of `[A-Za-z0-9._-]`.
    pub client_id: String,
    /// The device name sessions show as a tab's size owner.
    pub label: String,
    /// This client's `roost_vt::libghostty_build()`, offered on attach so
    /// a matching session serves `ghostty-snapshot`.
    pub libghostty_build: String,
}

/// The session a connection reached, as `session.identify` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFacts {
    pub session_id: String,
    pub libghostty_build: String,
    pub payload_kinds: Vec<String>,
    pub app_version: String,
}

impl From<&SessionIdentify> for SessionFacts {
    fn from(identity: &SessionIdentify) -> Self {
        Self {
            session_id: identity.session_id.clone(),
            libghostty_build: identity.libghostty_build.clone(),
            payload_kinds: identity
                .payload_kinds
                .iter()
                .map(|kind| kind.0.clone())
                .collect(),
            app_version: identity.app_version.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    Connecting,
    Connected,
    /// Dropped; the next attempt is scheduled.
    Retrying {
        in_ms: u64,
        reason: String,
    },
    /// The host key is not pinned yet. Waits for the user.
    NeedsTrust {
        fingerprint: String,
    },
    /// Needs the user: a protocol mismatch, a changed host key, a host
    /// that refuses the login. Waits for `reconnect_now`.
    Failed {
        reason: String,
    },
    /// Disconnected on request.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    State(HostState),
    /// The mirror changed; read it through the client.
    Workspace,
    /// `notification.fired` — live only, never replayed.
    Notification {
        tab_id: i64,
        title: String,
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostCallError {
    #[error("not connected")]
    NotConnected,
    #[error("{code}: {message}")]
    Server { code: String, message: String },
    #[error("{0}")]
    Transport(String),
}

impl HostCallError {
    /// The typed refusal, when the session refused.
    pub fn server_code(&self) -> Option<ServerCode> {
        match self {
            HostCallError::Server { code, .. } => Some(ServerCode::from_wire(code)),
            _ => None,
        }
    }
}

/// First retry delay after a drop.
const BACKOFF_BASE: Duration = Duration::from_millis(250);
/// A phone that comes back into coverage should find its host within
/// seconds, and a dial is cheap enough to repeat that often.
const BACKOFF_CAP: Duration = Duration::from_secs(10);
/// How many event batches the pump may hold while the task is busy on a
/// control op. Past it the pump stops reading, and a session that sees a
/// subscriber stop reading closes the stream — the resync signal.
const EVENT_PUMP_DEPTH: usize = 1024;

/// One control-plane leg's budget, and a dial's.
pub(crate) fn leg() -> Duration {
    IPC_TIMEOUT.mul_f64(scale())
}

/// A handle on one host's connection task. Dropping it shuts the task
/// down.
pub struct HostClient {
    shared: Arc<Shared>,
}

/// What the handle and the task share.
struct Shared {
    config: HostClientConfig,
    /// One handle for the client's whole life: a fresh snapshot resets
    /// its contents and a resume keeps them, so a consumer can hold it.
    mirror: Arc<SharedMirror>,
    /// `Some` exactly while connected.
    facts: watch::Sender<Option<SessionFacts>>,
    calls: mpsc::UnboundedSender<Call>,
    /// The connection a call made now would run on; `0` while none is up.
    serving: AtomicU64,
    reconnect: Notify,
    shutdown: Shutdown,
}

/// One queued control-plane op.
struct Call {
    op: String,
    params: serde_json::Value,
    /// The connection the caller saw. A call is never carried onto a
    /// connection it was not made against: a tab id means something only
    /// to the session process that minted it.
    incarnation: u64,
    reply: oneshot::Sender<Result<serde_json::Value, HostCallError>>,
}

impl HostClient {
    /// Start the connection task on `rt`. Nothing here needs an ambient
    /// runtime.
    pub fn spawn(
        rt: &tokio::runtime::Handle,
        dialer: Arc<dyn Dialer>,
        cfg: HostClientConfig,
    ) -> (HostClient, mpsc::UnboundedReceiver<HostEvent>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (calls_tx, calls_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            config: cfg,
            mirror: Arc::new(SharedMirror::default()),
            facts: watch::Sender::new(None),
            calls: calls_tx,
            serving: AtomicU64::new(0),
            reconnect: Notify::new(),
            shutdown: Shutdown::default(),
        });
        rt.spawn(run(Arc::clone(&shared), dialer, calls_rx, events_tx));
        (HostClient { shared }, events_rx)
    }

    /// The host's projects and tabs. The same handle across reconnects.
    pub fn mirror(&self) -> Arc<SharedMirror> {
        Arc::clone(&self.shared.mirror)
    }

    /// Run one control-plane op on this host. Ops run one at a time, in
    /// the order they were called, on the connection that said
    /// `client.hello` — so the ops that act for "this client"
    /// (`tab.take_size`, `tab.release_size`) work.
    ///
    /// Refused with [`HostCallError::NotConnected`] while no connection
    /// is up, and when the connection it was made against goes away
    /// before it ran.
    pub fn call(
        &self,
        op: &str,
        params: serde_json::Value,
    ) -> BoxFuture<'static, Result<serde_json::Value, HostCallError>> {
        let incarnation = self.shared.serving.load(Ordering::Acquire);
        let (reply, answer) = oneshot::channel();
        let queued = incarnation != 0
            && self
                .shared
                .calls
                .send(Call {
                    op: op.to_string(),
                    params,
                    incarnation,
                    reply,
                })
                .is_ok();
        async move {
            if !queued {
                return Err(HostCallError::NotConnected);
            }
            answer.await.unwrap_or(Err(HostCallError::NotConnected))
        }
        .boxed()
    }

    /// The session the current connection reached; `None` while not
    /// connected.
    pub fn session(&self) -> Option<SessionFacts> {
        self.shared.facts.borrow().clone()
    }

    /// Skip the wait: retry a scheduled or a terminal attempt now (the
    /// app resumed, the network changed, the user trusted a key). A live
    /// connection is probed instead, and replaced at once if it is dead.
    pub fn reconnect_now(&self) {
        self.shared.reconnect.notify_one();
    }

    /// Wind the task down. Idempotent.
    pub fn shutdown(&self) {
        self.shared.shutdown.request();
    }

    pub(crate) fn config(&self) -> &HostClientConfig {
        &self.shared.config
    }

    pub(crate) fn watch_session(&self) -> watch::Receiver<Option<SessionFacts>> {
        self.shared.facts.subscribe()
    }
}

impl Drop for HostClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Why an attempt did not produce a connection.
#[derive(Debug)]
enum Fail {
    /// Worth retrying on the backoff ladder.
    Retry(String),
    /// Waits for the user, then `reconnect_now`.
    Terminal(HostState),
}

impl From<ClientError> for Fail {
    fn from(error: ClientError) -> Self {
        Fail::Retry(error.to_string())
    }
}

/// How a served connection ended.
enum End {
    Shutdown,
    /// `now`: a probe found it dead, so the next attempt skips the wait.
    Dropped {
        reason: String,
        now: bool,
    },
}

/// A connection past its prologue.
struct Live {
    control: IpcClient,
    events: EventRx,
    _pump: EventPump,
    facts: SessionFacts,
}

type EventRx = mpsc::Receiver<Result<EventFrame, ClientError>>;

/// Aborts the pump task when the subscription it reads is replaced.
struct EventPump(tokio::task::AbortHandle);

impl Drop for EventPump {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn run(
    shared: Arc<Shared>,
    dialer: Arc<dyn Dialer>,
    mut calls: mpsc::UnboundedReceiver<Call>,
    events: mpsc::UnboundedSender<HostEvent>,
) {
    let publish = |event: HostEvent| {
        let _ = events.send(event);
    };
    let mut backoff = Backoff::new(BACKOFF_BASE, BACKOFF_CAP);
    // The session the mirror's revision is a fence for: what a resume
    // may name.
    let mut fenced: Option<String> = None;
    let mut incarnation = 0u64;
    loop {
        publish(HostEvent::State(HostState::Connecting));
        let attempt = refusing(
            &mut calls,
            &shared.shutdown,
            connect(&shared, &*dialer, fenced.as_deref()),
        )
        .await;
        let wait = match attempt {
            None => break,
            Some(Ok(mut live)) => {
                backoff.reset();
                incarnation += 1;
                fenced = Some(live.facts.session_id.clone());
                shared.facts.send_replace(Some(live.facts.clone()));
                shared.serving.store(incarnation, Ordering::Release);
                publish(HostEvent::State(HostState::Connected));
                publish(HostEvent::Workspace);
                let end = serve(
                    &shared,
                    &*dialer,
                    &mut live,
                    &mut calls,
                    &publish,
                    incarnation,
                )
                .await;
                shared.serving.store(0, Ordering::Release);
                shared.facts.send_replace(None);
                shared.mirror.read().suspend_password_input();
                while let Ok(call) = calls.try_recv() {
                    let _ = call.reply.send(Err(HostCallError::NotConnected));
                }
                match end {
                    End::Shutdown => break,
                    End::Dropped { reason, now: true } => {
                        tracing::info!(host = %dialer.describe(), %reason, "connection lost; reconnecting now");
                        continue;
                    }
                    End::Dropped { reason, now: false } => Fail::Retry(reason),
                }
            }
            Some(Err(fail)) => fail,
        };
        let waited = match wait {
            Fail::Retry(reason) => {
                let delay = backoff.next_delay(jitter());
                tracing::info!(host = %dialer.describe(), %reason, ?delay, "host connection retrying");
                publish(HostEvent::State(HostState::Retrying {
                    in_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    reason,
                }));
                let skip = shared.reconnect.notified();
                refusing(&mut calls, &shared.shutdown, async {
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {}
                        () = skip => {}
                    }
                })
                .await
            }
            Fail::Terminal(state) => {
                tracing::warn!(host = %dialer.describe(), ?state, "host connection needs the user");
                publish(HostEvent::State(state));
                let retried =
                    refusing(&mut calls, &shared.shutdown, shared.reconnect.notified()).await;
                backoff.reset();
                retried
            }
        };
        if waited.is_none() {
            break;
        }
    }
    publish(HostEvent::State(HostState::Idle));
}

/// Drive `work` while answering every call that arrives with
/// [`HostCallError::NotConnected`]: none of them has a connection to run
/// on. `None` when a shutdown was asked for first.
async fn refusing<T>(
    calls: &mut mpsc::UnboundedReceiver<Call>,
    shutdown: &Shutdown,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            () = shutdown.requested() => return None,
            out = &mut work => return Some(out),
            Some(call) = calls.recv() => {
                let _ = call.reply.send(Err(HostCallError::NotConnected));
            }
        }
    }
}

/// One attempt: the prologue in the order `ipc.md` fixes — hello, the
/// identify gate, then the subscription fenced against a snapshot (or a
/// resume of the one the mirror already holds).
async fn connect(shared: &Shared, dialer: &dyn Dialer, fenced: Option<&str>) -> Result<Live, Fail> {
    let mut control = open_control(&shared.config, dialer).await?;
    let raw = call_within(&mut control, ops::SESSION_IDENTIFY, identify_params()).await?;
    let identity: SessionIdentify = serde_json::from_value(raw)
        .map_err(|error| Fail::Retry(format!("session.identify did not decode: {error}")))?;
    gate(&identity, &shared.config, dialer)?;
    let facts = SessionFacts::from(&identity);
    let resume = fenced.filter(|fenced| *fenced == facts.session_id);
    let (events, pump) = subscribe(
        dialer,
        &mut control,
        &shared.mirror,
        resume,
        &facts.session_id,
    )
    .await?;
    Ok(Live {
        control,
        events,
        _pump: pump,
        facts,
    })
}

/// Dial the control connection and say hello on it.
///
/// A refusal of the hello leaves the connection in step and the identify
/// that follows decides. A hello that got no answer costs the connection
/// (a late reply would be read as the identify's), and the replacement
/// says hello too; only a second loss goes on without one.
async fn open_control(config: &HostClientConfig, dialer: &dyn Dialer) -> Result<IpcClient, Fail> {
    let mut control = dial(dialer).await?;
    for _ in 0..2 {
        if say_hello(&mut control, config).await {
            break;
        }
        control = dial(dialer).await?;
    }
    Ok(control)
}

async fn dial(dialer: &dyn Dialer) -> Result<IpcClient, Fail> {
    let duplex = tokio::time::timeout(leg(), dialer.dial())
        .await
        .map_err(|_| Fail::Retry(format!("dialing {} timed out", dialer.describe())))?
        .map_err(dial_failure)?;
    Ok(IpcClient::over_split(duplex.read, duplex.write))
}

/// What a dial error means for the connection's state.
fn dial_failure(error: DialError) -> Fail {
    match error {
        DialError::UntrustedHostKey { fingerprint, .. } => {
            Fail::Terminal(HostState::NeedsTrust { fingerprint })
        }
        error if !error.retryable() => Fail::Terminal(HostState::Failed {
            reason: error.to_string(),
        }),
        error => Fail::Retry(error.to_string()),
    }
}

async fn say_hello(control: &mut IpcClient, config: &HostClientConfig) -> bool {
    let hello = ClientHelloParams {
        client_id: config.client_id.clone(),
        label: config.label.clone(),
    };
    match tokio::time::timeout(leg(), control.call_raw(ops::CLIENT_HELLO, &hello)).await {
        Ok(Ok(_)) => true,
        Ok(Err(ClientError::Server { code, .. })) => {
            tracing::debug!(%code, "client.hello refused; identify decides");
            true
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "client.hello failed; dialing again");
            false
        }
        Err(_) => {
            tracing::debug!("client.hello timed out; dialing again");
            false
        }
    }
}

/// The compatibility gate. A session this client cannot talk to is
/// terminal: retrying would only ask the same question again.
fn gate(
    identity: &SessionIdentify,
    config: &HostClientConfig,
    dialer: &dyn Dialer,
) -> Result<(), Fail> {
    match check_compatibility(identity, &config.libghostty_build, RestartAction::None) {
        Ok(Compatibility::Exact) => Ok(()),
        Ok(Compatibility::BuildSkew) => {
            tracing::warn!(
                host = %dialer.describe(),
                session_build = %identity.libghostty_build,
                client_build = %config.libghostty_build,
                "libghostty build skew: attaching in the vt fallback"
            );
            Ok(())
        }
        Err(mismatch) => Err(Fail::Terminal(HostState::Failed {
            reason: mismatch_reason(&mismatch),
        })),
    }
}

/// The line a user reads when the gate refuses — naming both sides, so
/// they can tell which one to update.
pub fn mismatch_reason(mismatch: &BuildMismatch) -> String {
    match mismatch.kind {
        MismatchKind::Protocol => format!(
            "session protocol mismatch: this app speaks {}, the session speaks {}; \
             update whichever is older",
            mismatch.client_protocol, mismatch.session_protocol
        ),
        MismatchKind::PayloadKind => format!(
            "the session offers no payload this app can read (it offers: {})",
            mismatch.session_payload_kinds.join(", ")
        ),
        MismatchKind::Build | MismatchKind::OldSocketPath => format!(
            "libghostty build mismatch: this app is {}, the session is {}, \
             and the session cannot serve the vt fallback",
            mismatch.client_build, mismatch.session_build
        ),
    }
}

/// Subscribe, resuming from the mirror's fence when `resume` names the
/// session it was taken from, and falling back to a fresh subscription
/// plus a `tab.list` fenced against its ack when the session cannot
/// replay that far.
async fn subscribe(
    dialer: &dyn Dialer,
    control: &mut IpcClient,
    mirror: &SharedMirror,
    resume: Option<&str>,
    session_id: &str,
) -> Result<(EventRx, EventPump), Fail> {
    if resume.is_some() {
        let from_revision = mirror.read().revision;
        let client = dial(dialer).await?;
        match tokio::time::timeout(leg(), client.resume_events(from_revision, session_id)).await {
            Err(_) => return Err(Fail::Retry("events.subscribe timed out".into())),
            Ok(Ok(stream)) => {
                same_incarnation(&stream, session_id)?;
                if stream.revision() != from_revision {
                    return Err(Fail::Retry(format!(
                        "resume ack {} does not match from_revision {from_revision}",
                        stream.revision()
                    )));
                }
                mirror.read().resume_password_input();
                return Ok(spawn_event_pump(stream));
            }
            Ok(Err(error)) => match error.server_code() {
                Some(
                    code @ (ServerCode::ReplayExpired
                    | ServerCode::RevisionAhead
                    | ServerCode::SessionMismatch),
                ) => {
                    tracing::info!(
                        code = code.as_str(),
                        from_revision,
                        "no replay from here; snapshotting"
                    );
                }
                _ => return Err(Fail::from(error)),
            },
        }
    }
    let client = dial(dialer).await?;
    let stream = tokio::time::timeout(leg(), client.subscribe_events())
        .await
        .map_err(|_| Fail::Retry("events.subscribe timed out".into()))??;
    same_incarnation(&stream, session_id)?;
    let ack = stream.revision();
    let pumped = spawn_event_pump(stream);
    let raw = call_within(control, ops::TAB_LIST, serde_json::json!({})).await?;
    let list: TabListResult = serde_json::from_value(raw)
        .map_err(|error| Fail::Retry(format!("tab.list did not decode: {error}")))?;
    let Some(revision) = list.revision else {
        return Err(Fail::Retry(
            "tab.list answered without a revision; a session socket fences its snapshot".into(),
        ));
    };
    mirror.reset(HostMirror::from_list(list, revision.max(ack)));
    Ok(pumped)
}

/// A subscribe is its own dial, so a restart between it and the identify
/// would fold one session's history onto another's rows.
fn same_incarnation(stream: &EventStream, identified: &str) -> Result<(), Fail> {
    if stream.session_id() != identified {
        return Err(Fail::Retry(format!(
            "session {} answered the subscribe; this attempt identified session {identified}",
            stream.session_id()
        )));
    }
    Ok(())
}

/// Read the push stream on its own task: `EventStream::next` buffers
/// whole lines and is not cancel-safe, so it cannot be a `select!` arm.
fn spawn_event_pump(mut stream: EventStream) -> (EventRx, EventPump) {
    let (tx, rx) = mpsc::channel(EVENT_PUMP_DEPTH);
    let handle = tokio::spawn(async move {
        loop {
            let item = match stream.next().await {
                Ok(Some(frame)) => Ok(frame),
                Ok(None) => return,
                Err(error) => Err(error),
            };
            let fatal = item.is_err();
            if tx.send(item).await.is_err() || fatal {
                return;
            }
        }
    });
    (rx, EventPump(handle.abort_handle()))
}

/// The steady state: events into the mirror, calls onto the control
/// connection, in the order either arrives.
async fn serve(
    shared: &Shared,
    dialer: &dyn Dialer,
    live: &mut Live,
    calls: &mut mpsc::UnboundedReceiver<Call>,
    publish: &impl Fn(HostEvent),
    incarnation: u64,
) -> End {
    let dropped = |reason: String| End::Dropped { reason, now: false };
    loop {
        tokio::select! {
            biased;
            () = shared.shutdown.requested() => return End::Shutdown,
            frame = live.events.recv() => match frame {
                Some(Ok(EventFrame::Batch(batch))) => apply(&shared.mirror, &batch, publish),
                Some(Ok(EventFrame::Stopping(stopping))) => {
                    return dropped(format!("the session is stopping ({})", stopping.reason));
                }
                Some(Ok(EventFrame::Ended(ended))) => {
                    return dropped(format!("the event stream ended ({})", ended.reason));
                }
                Some(Err(error @ ClientError::RevisionGap { .. })) => {
                    tracing::warn!(host = %dialer.describe(), %error, "resyncing the host mirror");
                    let resynced = subscribe(
                        dialer,
                        &mut live.control,
                        &shared.mirror,
                        Some(&live.facts.session_id),
                        &live.facts.session_id,
                    )
                    .await;
                    match resynced {
                        Ok((events, pump)) => {
                            live._pump = pump;
                            live.events = events;
                            publish(HostEvent::Workspace);
                        }
                        Err(Fail::Retry(reason)) => return dropped(reason),
                        Err(Fail::Terminal(state)) => return dropped(format!("{state:?}")),
                    }
                }
                Some(Err(error)) => return dropped(error.to_string()),
                None => return dropped("the event stream closed".into()),
            },
            Some(call) = calls.recv() => {
                if call.incarnation != incarnation {
                    let _ = call.reply.send(Err(HostCallError::NotConnected));
                    continue;
                }
                if let Err(reason) = run_call(&mut live.control, call).await {
                    return dropped(reason);
                }
            }
            () = shared.reconnect.notified() => {
                let probed =
                    call_within(&mut live.control, ops::SESSION_IDENTIFY, identify_params()).await;
                if let Err(fail) = probed {
                    let reason = match fail {
                        Fail::Retry(reason) => reason,
                        Fail::Terminal(state) => format!("{state:?}"),
                    };
                    return End::Dropped { reason, now: true };
                }
            }
        }
    }
}

/// Fold a batch into the mirror and say so.
///
/// Notifications are announced off every batch, the ones at or below the
/// fence included: `notification.fired` is stripped from every replay,
/// so any that arrives is one this subscription saw live and the snapshot
/// only carries as a pending flag.
fn apply(mirror: &SharedMirror, batch: &EventBatch, publish: &impl Fn(HostEvent)) {
    if mirror.apply_batch(batch) {
        publish(HostEvent::Workspace);
    }
    for envelope in &batch.events {
        if envelope.event != ops::EVENT_NOTIFICATION_FIRED {
            continue;
        }
        match serde_json::from_value::<NotificationFiredEvent>(envelope.data.clone()) {
            Ok(fired) => publish(HostEvent::Notification {
                tab_id: fired.tab_id,
                title: fired.title,
                body: fired.body,
            }),
            Err(error) => tracing::debug!(%error, "notification.fired did not decode"),
        }
    }
}

/// Run one op and answer its caller. `Err` means the connection is
/// finished: the wire died, the op timed out (a late reply would be read
/// as the next op's), or the session latched a stop.
async fn run_call(control: &mut IpcClient, call: Call) -> Result<(), String> {
    let Call {
        op, params, reply, ..
    } = call;
    match tokio::time::timeout(leg(), control.call_raw(&op, params)).await {
        Ok(Ok(result)) => {
            let _ = reply.send(Ok(result));
            Ok(())
        }
        Ok(Err(ClientError::Server { code, message })) => {
            let stopping = ServerCode::from_wire(&code) == ServerCode::ShuttingDown;
            let _ = reply.send(Err(HostCallError::Server { code, message }));
            if stopping {
                return Err("the session is stopping".into());
            }
            Ok(())
        }
        Ok(Err(error)) => {
            let reason = error.to_string();
            let _ = reply.send(Err(HostCallError::Transport(reason.clone())));
            Err(reason)
        }
        Err(_) => {
            let reason = format!("{op} timed out");
            let _ = reply.send(Err(HostCallError::Transport(reason.clone())));
            Err(reason)
        }
    }
}

fn identify_params() -> serde_json::Value {
    serde_json::json!(SessionIdentifyParams {})
}

async fn call_within(
    control: &mut IpcClient,
    op: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, Fail> {
    tokio::time::timeout(leg(), control.call_raw(op, params))
        .await
        .map_err(|_| Fail::Retry(format!("{op} timed out")))?
        .map_err(Fail::from)
}

#[cfg(test)]
mod tests {
    use roost_ipc::messages::{AttachPayloadKind, SESSION_PROTOCOL_VERSION};

    use super::*;

    fn identity(protocol: u32) -> SessionIdentify {
        SessionIdentify {
            app_version: "0.0.27".into(),
            session_protocol: protocol,
            payload_kinds: vec![
                AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT),
                AttachPayloadKind::from(AttachPayloadKind::VT),
            ],
            libghostty_build: "ghostty-a".into(),
            session_id: "s1".into(),
            ..SessionIdentify::default()
        }
    }

    #[test]
    fn a_protocol_mismatch_names_both_versions() {
        let theirs = SESSION_PROTOCOL_VERSION + 1;
        let mismatch = check_compatibility(&identity(theirs), "ghostty-a", RestartAction::None)
            .expect_err("a mismatch");
        let reason = mismatch_reason(&mismatch);
        assert!(
            reason.contains(&format!("speaks {SESSION_PROTOCOL_VERSION}"))
                && reason.contains(&format!("speaks {theirs}")),
            "{reason}"
        );
    }

    #[test]
    fn only_an_untrusted_key_asks_for_trust_and_only_retryable_errors_retry() {
        let untrusted = DialError::UntrustedHostKey {
            host: "h".into(),
            fingerprint: "SHA256:x".into(),
        };
        assert!(matches!(
            dial_failure(untrusted),
            Fail::Terminal(HostState::NeedsTrust { fingerprint }) if fingerprint == "SHA256:x"
        ));
        let changed = DialError::HostKeyChanged {
            host: "h".into(),
            expected: "a".into(),
            got: "b".into(),
        };
        assert!(matches!(
            dial_failure(changed),
            Fail::Terminal(HostState::Failed { .. })
        ));
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(matches!(dial_failure(refused.into()), Fail::Retry(_)));
    }

    #[tokio::test]
    async fn a_call_with_no_connection_is_refused_at_once() {
        let dialer = Arc::new(crate::dial::UnixDialer("/nonexistent/roost.sock".into()));
        let (client, _events) = HostClient::spawn(
            &tokio::runtime::Handle::current(),
            dialer,
            HostClientConfig {
                client_id: "c".into(),
                label: "l".into(),
                libghostty_build: "b".into(),
            },
        );
        assert_eq!(
            client.call(ops::TAB_LIST, serde_json::json!({})).await,
            Err(HostCallError::NotConnected)
        );
        assert_eq!(client.session(), None);
    }
}
