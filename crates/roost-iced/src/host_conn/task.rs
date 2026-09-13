//! The connection owner: one task per connected host, running on the
//! app's engine runtime.
//!
//! It holds three things a UI thread must never hold — a control
//! `IpcClient`, a subscribed `EventStream`, and the authoritative
//! workspace mirror it fences against — and publishes onto the engine
//! feed. Nothing here touches libghostty or Iced; per CLAUDE.md's
//! threading table a background task only moves data.
//!
//! The order of the prologue is the wire contract, not a preference
//! (`ipc.md` #session-sockets): `session.identify` → the compatibility
//! gate → `session.set_theme` → subscribe → `tab.list`. The theme lands
//! **before any `tab.attach`** (plan 037 §3.6) and the snapshot is taken
//! **after** the subscribe so the ack's revision is a floor the snapshot
//! can be fenced against.
//!
//! ## The seam C5 fills
//!
//! Attaching a tab is a *fourth* connection per attached tab —
//! `tab.attach` (an intent on this task's queue, which is why token
//! minting rides the same queue) followed by
//! [`roost_ipc::client::DataConnection`]. C4 deliberately builds none of
//! it: the decoder is main-thread-only, so the data path's shape is
//! C5's to choose. What C4 guarantees it is a live control client, an
//! ordered queue to mint tokens on, and a mirror that already knows
//! which tabs exist.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use roost_ipc::client::{ClientError, EventFrame, EventStream, IpcClient, ServerCode};
use roost_ipc::messages::{
    ops, EventBatch, OscColorsParams, SessionIdentify, SessionIdentifyParams,
    SessionSetThemeParams, TabListResult,
};
use roost_ipc::session_launch;
use roost_ui_model::keys::HostId;
use tokio::sync::{mpsc, Notify};

use super::mirror::{HostMirror, SharedMirror};
use super::queue::{self, HostIntent, HostOpError, OpFault};
use super::state::{
    check_compatibility, ConnectFacts, HostConnState, HostStateMachine, HostTransport, ResumeFacts,
};
use super::upload::Uploads;
use super::{HostIdMinter, HostWorkspaceEvent};
use crate::engine_feed::{EngineFeed, EngineFeedSender};

/// The disconnect signal, level-triggered.
///
/// A bare [`Notify`] is edge-triggered: `notify_waiters` on a task that
/// happens to be *between* two of its `select!` arms is lost, and the
/// only thing that ever stopped the task would be the abort the flush
/// contract now forbids. The flag is what makes the signal durable — a
/// task that misses the wake still sees it on its next check.
#[derive(Debug, Default)]
pub(crate) struct Shutdown {
    requested: AtomicBool,
    wake: Notify,
}

impl Shutdown {
    /// Ask the task to wind down. Idempotent.
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Release);
        // Both calls, for two different waiters. A task keeps two —
        // its connection loop and its grace timer — and `notify_one`
        // alone wakes whichever parked first, leaving the other asleep
        // on a wake that never comes; the loop is the one that must
        // hear this, and it is the one that re-parks (and so ends up at
        // the back of the queue). `notify_waiters` wakes everyone
        // already parked, and `notify_one` then leaves a permit for a
        // waiter that had read the flag but not yet parked when this
        // ran.
        self.wake.notify_waiters();
        self.wake.notify_one();
    }

    /// Resolves once a disconnect has been asked for — however many
    /// times, and from however many places, it is awaited.
    ///
    /// The flag is read *before* parking, so a signal raised in the
    /// window between the two is seen rather than slept through.
    pub(super) async fn requested(&self) {
        while !self.requested.load(Ordering::Acquire) {
            self.wake.notified().await;
        }
    }
}

/// How the *first* attempt treats a socket that is not there.
///
/// Only an explicit Connect may start a daemon. Launch-time
/// auto-reconnect is connect-if-present, and a mid-session drop never
/// spawns at all (plan 037 §3.2) — so the mode is consumed by the first
/// attempt and every retry after it is a plain dial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectMode {
    /// Probe; an absent socket is a plain disconnected state, no daemon.
    IfPresent,
    /// Probe; an absent socket runs the shared spawn ladder.
    SpawnIfMissing,
    /// Dial straight away. What a non-localhost host always does — there
    /// is no local socket to probe and nothing this client could spawn.
    Dial,
}

/// Where a reconnect picks a session's event stream back up: the
/// session it was fenced against, and the mirror holding that fence.
///
/// The fence itself is deliberately **not** a field. It is
/// `mirror.read().revision` read at the moment the resume is offered —
/// a number copied when the checkpoint was made could be behind by
/// however much the mirror's writer applied in between, and asking a
/// session to replay commits the mirror already has is how a batch gets
/// applied twice.
pub(crate) struct Resume {
    pub(crate) session_id: String,
    pub(crate) mirror: Arc<SharedMirror>,
}

impl Resume {
    /// The same checkpoint, on a handle nobody else can write.
    ///
    /// **A checkpoint handed to a task that does not exist yet must be
    /// frozen.** A replaced connection is only *signalled* to stop
    /// (`HostConn::drop` notifies and aborts asynchronously), so it can
    /// still apply commit `N+1` to the handle it holds after the
    /// replacement has read the fence `N`. Were that the same handle,
    /// the session's replay of `N+1` would arrive at a mirror already
    /// past it, [`SharedMirror::apply_batch`] would discard it as
    /// already applied, and the envelopes it carried — a
    /// `notification.fired` from the gap — would never be published.
    /// A frozen copy cannot be advanced by the writer that is leaving.
    ///
    /// The copy is the workspace mirror, projects and tab rows, which is
    /// cheap by construction.
    pub(crate) fn freeze(&self) -> Resume {
        Resume {
            session_id: self.session_id.clone(),
            mirror: Arc::new(SharedMirror::new(self.mirror.snapshot())),
        }
    }
}

/// Everything one connection task needs, fixed for its lifetime.
pub(crate) struct ConnectionConfig {
    /// The saved host's stable id (`HostSnapshot.id`), for logs.
    pub(crate) host: String,
    pub(crate) label: String,
    pub(crate) socket: PathBuf,
    /// How this host is reached. Gates the spawn ladder and the
    /// auto-retry policy (both localhost-only), and decides what the
    /// build-mismatch dialog can offer.
    pub(crate) transport: HostTransport,
    /// Which of the saved host's connections this task is. Every
    /// incarnation it mints is registered under it, so the app can tell
    /// this task's publications from a replaced task's.
    pub(crate) generation: u64,
    /// The incarnation an explicit reconnect displaced, if any — seeds
    /// the first attempt's `Connecting { previous }` so consumers purge
    /// the dead incarnation's state exactly as they do for this task's
    /// own later retries.
    pub(crate) supersedes: Option<HostId>,
    pub(crate) mode: ConnectMode,
    /// Where this host's last connection left the event stream, if it
    /// reached one — seeded by the set for **every** cause, because
    /// what it describes is the session rather than who asked to dial
    /// it. `None` until some connection has reached a session.
    ///
    /// Frozen by [`Resume::freeze`] on the way in; see why there.
    pub(crate) resume: Option<Resume>,
    /// This session's pinned libghostty identity, compared exactly.
    pub(crate) client_build: String,
    /// The client's terminal palette, re-read on every (re)connect so a
    /// theme changed while disconnected is the one the session gets.
    pub(crate) theme: Arc<Mutex<OscColorsParams>>,
    /// The UI's handle on this host's upload lane. Filled at every
    /// `Connected` edge and emptied on every way out of one — see
    /// [`Uploads::open`] and [`serve`].
    pub(crate) uploads: Uploads,
}

/// The scale every budget in this module is stretched by, read once.
///
/// [`session_launch::timeout_scale`] reads the environment on each call,
/// and [`leg`] is per-op work on the control plane — the answer cannot
/// change while the process runs, so it is worth remembering.
pub(crate) fn scale() -> f64 {
    static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *SCALE.get_or_init(session_launch::timeout_scale)
}

/// A single control-plane leg's budget.
pub(crate) fn leg() -> Duration {
    session_launch::IPC_TIMEOUT.mul_f64(scale())
}

/// What `session.set_agent_hooks` gets instead of [`leg`].
///
/// Every other control-plane op is a lookup or a workspace mutation.
/// This one runs a real install on the far side — up to five agents'
/// config files, under an advisory `flock`, on a `$HOME` that is often
/// NFS-mounted on exactly the kind of box people keep host sessions on.
const AGENT_HOOKS_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one queued op may take on the wire.
///
/// A timeout still drops the connection, for the reason every other one
/// does: `IpcClient` is strictly sequential, so abandoning a request
/// mid-flight would leave its reply on the stream to be read as the next
/// op's. That is why this budget is generous rather than lenient — the
/// point is that a slow ensure never reaches it.
fn op_budget(op: &str) -> Duration {
    if op == ops::SESSION_SET_AGENT_HOOKS {
        AGENT_HOOKS_TIMEOUT.mul_f64(scale())
    } else {
        leg()
    }
}

/// What an explicit Connect gives a spawned daemon: the ladder's own
/// defaults, which are derived from the daemon's waits rather than from
/// how patient this particular client feels.
const SPAWN_VERDICT_BUDGET: Duration = session_launch::DEFAULT_VERDICT_BUDGET;
const SPAWN_CONFIRM_BUDGET: Duration = session_launch::DEFAULT_CONFIRM_BUDGET;

/// How long a disconnected task may keep unwinding before it stops
/// waiting for anything at all.
///
/// This is what lets `HostConn::drop` signal instead of aborting: the
/// task always reaches its final flush, and it always reaches it soon.
/// Every wait inside the connection loop is either a `shutdown` arm or
/// bounded by [`leg`], so the grace is a backstop rather than the normal
/// path — but a backstop that answers the queue, which an abort does
/// not.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Frames a subscribed connection may buffer before its pump waits.
/// Backpressure here is correct: the session closes a subscriber that
/// stops reading, and the drain side is the UI, which is fast.
const EVENT_PUMP_DEPTH: usize = 64;

/// The pumped end of a subscription: a frame, or the error that ended
/// the stream.
type EventRx = mpsc::Receiver<Result<EventFrame, ClientError>>;

/// Why one attempt ended.
#[derive(Debug)]
enum AttemptError {
    /// The compatibility gate refused. Terminal — the upgrade flow.
    Incompatible(Box<super::state::BuildMismatch>),
    /// The session said it is going away, with the wire's reason.
    Stopping(String),
    /// Transport or refusal. Retryable where policy allows.
    Transport(String),
    /// The localhost launch ladder could not produce a daemon, and no
    /// retry could. Terminal — see [`spawn_failure`].
    Unrecoverable { reason: String, detail: String },
}

impl From<ClientError> for AttemptError {
    fn from(error: ClientError) -> Self {
        match error.server_code() {
            Some(ServerCode::ShuttingDown) => AttemptError::Stopping("stop".into()),
            _ => AttemptError::Transport(error.to_string()),
        }
    }
}

/// A failed attempt ends the round exactly as a failed *serve* does, so
/// it is spelled as one and the loop has a single ending to handle.
impl From<AttemptError> for ConnEnd {
    fn from(error: AttemptError) -> Self {
        match error {
            AttemptError::Incompatible(mismatch) => ConnEnd::Incompatible(mismatch),
            AttemptError::Stopping(reason) => ConnEnd::Stopping(reason),
            AttemptError::Transport(reason) => ConnEnd::Dropped(reason),
            AttemptError::Unrecoverable { reason, detail } => ConnEnd::Settled { reason, detail },
        }
    }
}

/// Which rung of the localhost launch ladder failed, as
/// [`spawn_failure`] classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpawnStage {
    /// [`session_launch::locate_session_binary`] — no rung found a
    /// binary, or an explicit `ROOST_SESSION_BIN` was unusable.
    Locate,
    /// Reading the launch cwd.
    Cwd,
    /// [`session_launch::spawn_and_read_verdict`] — the exec itself, or
    /// the read of the line it prints.
    Launch,
    /// The daemon's own [`session_launch::Verdict::Error`].
    Verdict,
    /// [`session_launch::confirm_serving`].
    Confirm,
}

/// Whether a failed spawn is worth dialing again, and what to say.
///
/// | Stage | Verdict | `reason` |
/// |---|---|---|
/// | `Locate` | unrecoverable | `cannot find roost-session` |
/// | `Cwd` | unrecoverable | `roost-session failed to start` |
/// | `Launch`, exec-time io error | unrecoverable | `roost-session failed to start` |
/// | `Launch`, anything else (verdict timeout, EOF, an over-long line) | transport | *(the error)* |
/// | `Verdict` | unrecoverable | `roost-session failed to start` |
/// | `Confirm` | transport | *(the error)* |
///
/// **Why every exec failure settles**, including the kinds that read as
/// transient (`Interrupted`, a temporary fork failure): `mode` becomes
/// [`ConnectMode::Dial`] after the first attempt, so **no retry can ever
/// spawn again** — it would dial a socket that nothing is left to
/// create, and the dial's generic io error would overwrite this reason
/// every 250 ms forever. Settling is the only honest verdict under that
/// loop, and ↻ Reconnect is the recovery. A future policy that let a
/// retry spawn would revisit this table first.
///
/// The two timeout rows stay retryable for the mirror-image reason: the
/// daemon *was* exec'd and may still be on its way to binding, and a
/// dial is exactly the right retry for that.
///
/// This is the localhost half. The ssh transport's equivalent verdict
/// lives in [`retryable`](super::reconnect::retryable).
///
/// **How `Launch` tells an exec failure from a verdict-read timeout.**
/// [`session_launch::spawn_and_read_verdict`] attaches a real
/// `io::Error` as a source exactly once — `Command::spawn`'s, through
/// `with_context`. Every other failure it can return is a bare
/// `anyhow!` string, the read's own io error included
/// ([`session_launch::VerdictRead::Io`] stringifies before it leaves
/// the reader). So an `io::Error` anywhere in the chain *is* the exec,
/// and that stays true only while `VerdictRead::Io` carries a `String`.
///
/// Both settled `reason`s are written for the band's ~45 characters, not
/// for the operator — what actually happened travels beside them as
/// `detail`.
fn spawn_failure(stage: SpawnStage, error: &anyhow::Error) -> AttemptError {
    let bin = session_launch::BIN_NAME;
    let detail = format!("{error:#}");
    let reason = match stage {
        SpawnStage::Locate => format!("cannot find {bin}"),
        SpawnStage::Cwd | SpawnStage::Verdict => format!("{bin} failed to start"),
        SpawnStage::Launch if error.chain().any(|cause| cause.is::<std::io::Error>()) => {
            format!("{bin} failed to start")
        }
        SpawnStage::Launch | SpawnStage::Confirm => return AttemptError::Transport(detail),
    };
    AttemptError::Unrecoverable { reason, detail }
}

/// The copy for a socket with nothing behind it — one spelling, shared
/// by the probe that finds it missing and the dial that discovers it.
fn no_session_at(socket: &Path) -> String {
    format!("no session is running at {}", socket.display())
}

/// A dial failed: say what actually happened when we can tell.
///
/// A [`ConnectMode::Dial`] attempt is the *retry* of a localhost
/// connection whose first attempt already reported the honest reason —
/// and `roost_ipc`'s own `io error: No such file` would overwrite it
/// with something that reads like a bug in Roost. `NotFound` and
/// `ConnectionRefused` against the socket mean one thing here, and it is
/// the same thing [`ensure_socket`]'s probe says, so it is said the same
/// way.
///
/// Only in `Dial` mode: the other two modes probed first, so a missing
/// socket after a live probe is a race worth reporting literally.
/// The redial policy is untouched — a session started by hand later
/// still attaches with no ↻.
fn dial_failure(mode: ConnectMode, socket: &Path, error: &roost_ipc::Error) -> AttemptError {
    match (mode, error) {
        (ConnectMode::Dial, roost_ipc::Error::Io(io))
            if matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            AttemptError::Transport(no_session_at(socket))
        }
        _ => AttemptError::Transport(error.to_string()),
    }
}

/// The pump task's lifetime. Dropping this aborts it — on the error
/// path, on cancellation, on a panic, and on [`Live`] going away — so no
/// exit from the prologue can leave a subscribed socket behind. Same
/// footgun `host_conn.rs`'s displaced-establish handles document:
/// dropping an `AbortHandle` on its own aborts nothing.
struct EventPump(tokio::task::AbortHandle);

impl Drop for EventPump {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A live, subscribed connection.
struct Live {
    control: IpcClient,
    events: EventRx,
    pump: EventPump,
    /// Shared with the UI: written here, read there. Never copied onto
    /// the feed.
    mirror: Arc<SharedMirror>,
    /// What the prologue learned about the session on the other end.
    /// Published once, right behind the state this connection reached.
    facts: ConnectFacts,
}

impl Live {
    /// Where this connection would be resumed from.
    ///
    /// Unfrozen on purpose, unlike the set's ([`Resume::freeze`]): the
    /// only writer of this handle is the pump this very task drains, so
    /// once the attempt has ended nothing can advance it behind the
    /// next one's back.
    fn checkpoint(&self) -> Resume {
        Resume {
            session_id: self.facts.session_id.clone(),
            mirror: Arc::clone(&self.mirror),
        }
    }

    /// The line a finished prologue writes.
    fn log(&self, config: &ConnectionConfig) {
        tracing::info!(
            host = %config.host,
            label = %config.label,
            session = %self.facts.session_id,
            revision = self.mirror.read().revision,
            resumed = self.facts.resumed.is_some(),
            "connected to host session"
        );
    }
}

/// The ops an older session answers `unknown-op` to, and whether this
/// *task* has been told about each yet.
///
/// A session that predates one of these is not a fault: the client keeps
/// sending (it has no other way to find out, and the refusal costs one
/// round trip), the connection is unaffected, and one line is the whole
/// story — a line per selection change, or per reconnect's ensure, is
/// noise.
///
/// It deliberately outlives [`Live`]. `session.set_agent_hooks` is
/// re-sent on **every** connect, and a localhost session that drops
/// reconnects on a 250 ms ladder, so a flag rebuilt per connection would
/// say the same sentence about the same unchanging session forever. The
/// facts it latches are properties of the session, not of the wire to
/// it, so [`connect_loop`] owns one for as long as it keeps dialling the
/// same host.
#[derive(Default)]
struct Unsupported {
    /// HS-2 sessions predate `session.set_focus`; their attached tab
    /// suppresses its own notifications.
    focus: bool,
    /// Sessions before plan 046 predate `session.set_agent_hooks`; their
    /// agent hooks are whatever the host itself last set.
    agent_hooks: bool,
}

impl Unsupported {
    /// Note an `unknown-op` refusal, and say whether it is worth a line.
    ///
    /// `false` for every other kind of failure — a `shutting-down` is
    /// not an old session, and swallowing it here would hide it — and
    /// `false` the second time one op says it.
    fn note(&mut self, op: &str, error: &HostOpError) -> Option<&'static str> {
        if !matches!(
            error,
            HostOpError::Rejected {
                code: ServerCode::UnknownOp,
                ..
            }
        ) {
            return None;
        }
        let (seen, note) = match op {
            ops::SESSION_SET_FOCUS => (
                &mut self.focus,
                "this host session predates session.set_focus; its attached \
                 tab suppresses its own notifications",
            ),
            ops::SESSION_SET_AGENT_HOOKS => (
                &mut self.agent_hooks,
                "this host session predates session.set_agent_hooks; this \
                 client's agent-hooks setting does not reach it",
            ),
            _ => return None,
        };
        (!std::mem::replace(seen, true)).then_some(note)
    }
}

/// The task body.
///
/// Two halves, and the split is the queue contract: the connection loop
/// runs until the host reaches a terminal state or the disconnect
/// signal fires, and then — always, on every path including a cancelled
/// one — the queue is closed and everything still on it is answered.
/// A caller awaiting a reply hears `Disconnected`, never a dropped
/// channel.
pub(crate) async fn run(
    config: ConnectionConfig,
    minter: HostIdMinter,
    mut ops_rx: mpsc::Receiver<HostIntent>,
    feed: EngineFeedSender,
    shutdown: Arc<Shutdown>,
) {
    {
        let body = connect_loop(&config, &minter, &mut ops_rx, &feed, &shutdown);
        tokio::pin!(body);
        tokio::select! {
            biased;
            () = &mut body => {}
            () = expired_grace(&shutdown) => {
                tracing::warn!(
                    host = %config.host,
                    "a disconnected host connection did not unwind in time"
                );
            }
        }
    }
    queue::close_and_flush(&mut ops_rx, &HostOpError::Disconnected);
}

/// Resolves [`SHUTDOWN_GRACE`] after a disconnect is requested, and
/// never otherwise.
async fn expired_grace(shutdown: &Shutdown) {
    shutdown.requested().await;
    tokio::time::sleep(SHUTDOWN_GRACE).await;
}

/// Dial, serve, and — where policy allows — dial again.
async fn connect_loop(
    config: &ConnectionConfig,
    minter: &HostIdMinter,
    ops_rx: &mut mpsc::Receiver<HostIntent>,
    feed: &EngineFeedSender,
    shutdown: &Shutdown,
) {
    let mut machine = HostStateMachine::new(config.transport.is_localhost());
    let mut previous: Option<HostId> = config.supersedes;
    let mut mode = config.mode;
    // Task-scoped, not connection-scoped: what it latches is what this
    // *session* cannot do, and reconnecting to it does not make an old
    // session newer. See [`Unsupported`].
    let mut unsupported = Unsupported::default();
    // This task's own checkpoint, taken from every attempt that reached
    // a session and preferred over the one the set seeded, which by then
    // describes a strictly older fence. It saves an in-task retry a trip
    // through the set and nothing more — the two agree by construction.
    //
    // Nothing clears it: every ending that would (`Stopping`, `Settled`,
    // `Incompatible` — the session is gone, unreachable, or must be
    // restarted) also ends this task, and the only ending that loops
    // back here is a `Dropped`, which is the wire and says nothing about
    // the session. The set's copy, which does outlive those, carries the
    // same table as real clearing.
    let mut resume: Option<Resume> = None;

    loop {
        // Mint (and therefore register the ownership) before anything is
        // published under the id: the drain side resolves the owner off
        // that registration, so a `Connecting` must never land first.
        let incarnation = minter.mint(&config.host, config.generation);
        if !publish_state(feed, incarnation, machine.begin_attempt(previous)) {
            return;
        }
        previous = Some(incarnation);

        let checkpoint = resume.as_ref().or(config.resume.as_ref());
        let dialed = tokio::select! {
            biased;
            () = shutdown.requested() => None,
            outcome = attempt(config, mode, checkpoint) => Some(outcome),
        };
        // Only the first attempt may spawn or probe; a retry dials.
        mode = ConnectMode::Dial;

        let ended = match dialed {
            None => ConnEnd::Shutdown,
            Some(Err(error)) => error.into(),
            Some(Ok(live)) => {
                // Taken before the connection is served rather than
                // after: both halves of a checkpoint are known the
                // moment the prologue ends, and the fence is read off
                // the handle at use time, so a `Live` that has since
                // been consumed leaves nothing behind.
                resume = Some(live.checkpoint());
                if !publish_workspace(
                    feed,
                    incarnation,
                    HostWorkspaceEvent::Reset(Arc::clone(&live.mirror)),
                ) || !publish_state(feed, incarnation, machine.connected())
                    // Behind the state, never ahead of it: the set files
                    // facts on the connection the state just installed.
                    || !publish_facts(feed, incarnation, live.facts.clone())
                {
                    ConnEnd::FeedClosed
                } else {
                    // The upload lane opens with the incarnation and
                    // closes with it. The guard is what carries "every
                    // exit from `Connected`" (plan 047 §3.3): every
                    // `ConnEnd` arm below, an explicit reconnect and a
                    // `HostConn::drop` (both of which signal `shutdown`,
                    // which `serve` returns on), and the whole loop's
                    // future being dropped by [`run`]'s grace timer —
                    // that last one runs no code, which is why this is a
                    // `Drop` and not a line after the `await`.
                    let _lane = config.uploads.open(config.socket.clone());
                    serve(
                        config,
                        incarnation,
                        live,
                        ops_rx,
                        feed,
                        shutdown,
                        &mut unsupported,
                    )
                    .await
                }
            }
        };

        // The feed being gone means the app is: there is nobody left to
        // tell, and nobody left to hear a flushed intent either.
        if matches!(ended, ConnEnd::FeedClosed) {
            return;
        }
        // Every other ending leaves `Connected`, and an intent behind a
        // dead connection has no way to succeed later (§3.9). A plain
        // flush, not a close: the same handle serves the retry below.
        queue::flush(ops_rx, &HostOpError::Disconnected);

        let delay = match ended {
            // Handled above; the arm is here only for exhaustiveness.
            ConnEnd::FeedClosed => return,
            ConnEnd::Shutdown => {
                publish_state(feed, incarnation, machine.disconnect_requested());
                return;
            }
            ConnEnd::Incompatible(mismatch) => {
                publish_state(feed, incarnation, machine.needs_restart(*mismatch));
                return;
            }
            ConnEnd::Stopping(reason) => {
                tracing::info!(host = %config.host, %reason, "the host session is going away");
                publish_state(feed, incarnation, machine.stopping());
                return;
            }
            ConnEnd::Settled { reason, detail } => {
                // The operator's copy of the launch ladder's rungs: the
                // band has room for `reason` alone, and this is the one
                // place the whole text is written down for a person who
                // is not looking at `roostctl host status`. Before the
                // publish, so a feed that has already gone still logs.
                tracing::warn!(
                    host = %config.host,
                    %reason,
                    detail = %detail,
                    "localhost session cannot start; not retrying"
                );
                publish_state(feed, incarnation, machine.settled(reason, detail));
                return;
            }
            ConnEnd::Dropped(reason) => {
                let state = machine.dropped(reason, jitter());
                let retry = state.retry_in();
                if !publish_state(feed, incarnation, state) {
                    return;
                }
                // Manual-reconnect only: the task is done, and an
                // explicit Connect starts a fresh one.
                let Some(delay) = retry else { return };
                delay
            }
        };

        tokio::select! {
            biased;
            () = shutdown.requested() => {
                publish_state(feed, incarnation, machine.disconnect_requested());
                return;
            }
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// Dial the session socket.
///
/// Bounded like every other leg. A peer that accepts the connection and
/// then says nothing would otherwise wedge this host in `Connecting` for
/// as long as the process runs.
async fn dial_control(
    config: &ConnectionConfig,
    mode: ConnectMode,
) -> Result<IpcClient, AttemptError> {
    tokio::time::timeout(leg(), IpcClient::connect(&config.socket))
        .await
        .map_err(|_| {
            AttemptError::Transport(format!("dialing {} timed out", config.socket.display()))
        })?
        .map_err(|error| dial_failure(mode, &config.socket, &error))
}

/// The prologue's opening: make sure a session is there, dial it, and
/// gate on `session.identify` before anything else is said.
///
/// Nothing binary exists yet at this point, so every incompatibility is
/// caught on stable JSON.
async fn open_control(
    config: &ConnectionConfig,
    mode: ConnectMode,
) -> Result<(IpcClient, ConnectFacts), AttemptError> {
    ensure_socket(config, mode).await?;
    let mut control = dial_control(config, mode).await?;
    let raw = call(
        &mut control,
        ops::SESSION_IDENTIFY,
        serde_json::json!(SessionIdentifyParams {}),
    )
    .await?;
    let identity: SessionIdentify =
        serde_json::from_value(raw).map_err(|error| undecodable(ops::SESSION_IDENTIFY, &error))?;
    let compatibility = check_compatibility(
        &identity,
        &config.client_build,
        config.transport.restart_action(),
    )
    .map_err(|mismatch| AttemptError::Incompatible(Box::new(mismatch)))?;
    let facts = ConnectFacts::new(&identity, &config.client_build, compatibility);
    if facts.reduced_fidelity {
        // Warn, not info: the connection is a working connection, but
        // what it can render is a documented subset of a terminal, and
        // the log is where a user comparing two screens is pointed.
        tracing::warn!(
            session_build = %facts.skew.session_build,
            client_build = %facts.skew.client_build,
            "libghostty build skew: attaching in the vt fallback, without the \
             inactive screen, soft-wrap flags or per-cell hyperlinks"
        );
    }
    Ok((control, facts))
}

/// A prologue's subscription, and the mirror handle its connection will
/// hold.
///
/// A fresh snapshot becomes a new handle; a resume **adopts the carried
/// one**, which is the whole point — it already holds the rows the
/// fence describes, the replayed batches land on top of them, and the
/// `Reset` published under the new incarnation re-registers the very
/// handle the UI is drawing.
///
/// `facts.resumed` is written here and nowhere else: it describes the
/// *prologue*, so a mid-stream resync that fell back is logged rather
/// than republished as a different verdict for the same connection.
async fn subscribe_prologue(
    socket: &Path,
    control: &mut IpcClient,
    facts: &mut ConnectFacts,
    resume: Option<&Resume>,
) -> Result<(EventRx, EventPump, Arc<SharedMirror>), AttemptError> {
    let plan = SubscribePlan {
        resume,
        session_id: &facts.session_id,
    };
    let offered = plan.offered();
    let (events, pump, subscribed) = subscribe(socket, control, plan).await?;
    let mirror = match (subscribed, offered) {
        (Subscribed::Fresh(mirror), _) => Arc::new(SharedMirror::new(mirror)),
        (Subscribed::Resumed { ack }, Some(resume)) => {
            facts.resumed = Some(ResumeFacts { from_revision: ack });
            Arc::clone(&resume.mirror)
        }
        // Only an offered checkpoint can be resumed, so this pair does
        // not arise — but building a mirror out of nothing would be a
        // silently empty workspace, and a retryable failure is honest.
        (Subscribed::Resumed { .. }, None) => {
            return Err(AttemptError::Transport(
                "the session resumed a stream this attempt never offered".into(),
            ))
        }
    };
    Ok((events, pump, mirror))
}

/// One connect attempt: the wire prologue, in the order `ipc.md` fixes.
///
/// Every connection runs this one, whatever asked for it — there is no
/// second shape a dial can produce.
async fn attempt(
    config: &ConnectionConfig,
    mode: ConnectMode,
    resume: Option<&Resume>,
) -> Result<Live, AttemptError> {
    // 1. Identify, and gate on it.
    let (mut control, mut facts) = open_control(config, mode).await?;

    // 2. Seed the session's palette before anything is attached, so a
    //    query answered while hydrating already carries our colors.
    let theme = config
        .theme
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    call(
        &mut control,
        ops::SESSION_SET_THEME,
        serde_json::json!(SessionSetThemeParams { osc_colors: theme }),
    )
    .await?;

    // 3. Subscribe — resuming from the carried fence where there is one,
    //    else subscribe then snapshot. That order is what makes the
    //    fresh fence sound: the ack names a commit the snapshot is
    //    guaranteed to be at or past.
    let (events, pump, mirror) =
        subscribe_prologue(&config.socket, &mut control, &mut facts, resume).await?;

    let live = Live {
        control,
        events,
        pump,
        mirror,
        facts,
    };
    live.log(config);
    Ok(live)
}

/// Probe, and on the first attempt only, spawn.
async fn ensure_socket(config: &ConnectionConfig, mode: ConnectMode) -> Result<(), AttemptError> {
    if mode == ConnectMode::Dial || socket_live(&config.socket).await {
        return Ok(());
    }
    // Nothing is listening, so only the mode that may start one gets to.
    if mode == ConnectMode::SpawnIfMissing {
        return spawn_session(config).await;
    }
    Err(AttemptError::Transport(no_session_at(&config.socket)))
}

/// Nothing is listening, and the user asked for a connection: climb the
/// shared launch ladder (`roost_ipc::session_launch`, the same rungs
/// `roostctl session start` uses).
async fn spawn_session(config: &ConnectionConfig) -> Result<(), AttemptError> {
    if !config.transport.is_localhost() {
        return Err(AttemptError::Transport(format!(
            "{} and only a localhost session can be started from here",
            no_session_at(&config.socket)
        )));
    }
    let scale = scale();
    let bin = session_launch::locate_session_binary(
        std::env::var_os(session_launch::BIN_ENV).as_deref(),
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
    .map_err(|error| spawn_failure(SpawnStage::Locate, &error))?;
    // The launch cwd seeds the session's first project on a fresh state
    // file only; a UI has no better answer than its own.
    let cwd = std::env::current_dir().map_err(|error| {
        spawn_failure(
            SpawnStage::Cwd,
            &anyhow::Error::new(error).context("read the working directory"),
        )
    })?;

    // Read here rather than inside the launcher, beside the `BIN_ENV`
    // read above: the launcher is a function of what it is handed, and
    // this is the process whose state dir the derivation is relative
    // to. A daemon that inherited the raw value would refuse this UI's
    // own `state.lock` (#397).
    let seam = std::env::var_os(roost_ipc::paths::STATE_DIR_ENV);

    let verdict = session_launch::spawn_and_read_verdict(
        &bin.path,
        &cwd,
        seam.as_deref(),
        SPAWN_VERDICT_BUDGET.mul_f64(scale),
    )
    .await
    .map_err(|error| spawn_failure(SpawnStage::Launch, &error))?;
    if let session_launch::Verdict::Error(reason) = &verdict {
        return Err(spawn_failure(
            SpawnStage::Verdict,
            &anyhow::anyhow!("{reason}"),
        ));
    }
    // Both success verdicts are confirmed rather than trusted: the
    // `already-running` loser can print before the winner has bound.
    session_launch::confirm_serving(&config.socket, SPAWN_CONFIRM_BUDGET.mul_f64(scale))
        .await
        .map_err(|error| spawn_failure(SpawnStage::Confirm, &error))?;
    Ok(())
}

/// Is something answering there?
///
/// The fail-safe reading, borrowed from the unlink side: only `Missing`
/// and `Stale` prove no listener, so an `Indeterminate` probe is treated
/// as live and dialed rather than spawned over.
async fn socket_live(socket: &Path) -> bool {
    use roost_ipc::socket_state;
    !socket_state::probe(socket, socket_state::PROBE_TIMEOUT)
        .await
        .safe_to_unlink()
}

/// What a subscription may offer the session it is about to make.
struct SubscribePlan<'a> {
    /// Where a previous connection left this host's stream, if any.
    resume: Option<&'a Resume>,
    /// The session this attempt actually reached, from
    /// `session.identify`.
    session_id: &'a str,
}

impl<'a> SubscribePlan<'a> {
    /// The plan a mid-connection resync presents: the checkpoint of the
    /// connection it is rebuilding, which by construction names the
    /// session that connection is already talking to.
    fn resuming(resume: &'a Resume) -> SubscribePlan<'a> {
        SubscribePlan {
            resume: Some(resume),
            session_id: &resume.session_id,
        }
    }

    /// The checkpoint this attempt may actually present, or `None` to
    /// subscribe fresh.
    ///
    /// The filter is decided **before any wire is touched**: the session
    /// id is compared locally because `session.identify` has already
    /// run, so a resume that could only come back `session-mismatch`
    /// never costs a round trip — the server's check stays the
    /// authority.
    fn offered(&self) -> Option<&'a Resume> {
        self.resume
            .filter(|resume| resume.session_id == self.session_id)
    }
}

/// How a subscription was established, and what the caller owes the
/// mirror because of it.
enum Subscribed {
    /// The session replayed from the checkpoint's fence. `ack` is the
    /// fence it echoed; there is no snapshot, and the carried mirror is
    /// current by construction.
    Resumed { ack: u64 },
    /// A fresh subscribe and a fenced `tab.list`.
    Fresh(HostMirror),
}

/// Subscribe, resuming from the checkpoint when the session can serve
/// one and taking a fresh snapshot when it cannot.
///
/// The single seam every subscription in this module goes through — the
/// prologue and the resync — so "does this reconnect take a `tab.list`?"
/// has exactly one answer to read.
async fn subscribe(
    socket: &Path,
    control: &mut IpcClient,
    plan: SubscribePlan<'_>,
) -> Result<(EventRx, EventPump, Subscribed), AttemptError> {
    if let Some(resume) = plan.offered() {
        if let Some(resumed) = resume_stream(socket, resume, plan.session_id).await? {
            return Ok(resumed);
        }
    }
    let (events, pump, mirror) = subscribe_and_snapshot(socket, control, plan.session_id).await?;
    Ok((events, pump, Subscribed::Fresh(mirror)))
}

/// Refuse an ack that came back from a different run of the session than
/// `session.identify` reached.
///
/// A subscribe is its own dial, so a restart — or a replaced socket —
/// between the two legs lands the stream on an incarnation whose
/// revisions and rows are a different history, and this client would fold
/// one onto the other (#458). Checked before the pump is spawned: a pump
/// that has started is already folding.
fn require_same_incarnation(stream: &EventStream, identified: &str) -> Result<(), AttemptError> {
    let answered = stream.session_id();
    if answered != identified {
        return Err(AttemptError::Transport(format!(
            "session {answered} answered the subscribe; \
             this attempt identified session {identified}"
        )));
    }
    Ok(())
}

/// Offer the checkpoint. `Ok(None)` is the session refusing by name —
/// the ring no longer reaches back that far, the fence names a commit
/// it never made, or it is not the session that made it — which is
/// never fatal: the caller subscribes fresh instead.
///
/// A fresh dial, because [`IpcClient::subscribe`] takes `self` by value
/// and a refusal therefore consumes the client. The server deliberately
/// leaves the refused connection reusable and a `&mut self` variant
/// would save the exec an ssh re-dial costs, but `roost-ipc` is
/// published and a refusal is the rare path.
async fn resume_stream(
    socket: &Path,
    resume: &Resume,
    identified: &str,
) -> Result<Option<(EventRx, EventPump, Subscribed)>, AttemptError> {
    // Read here rather than carried on the checkpoint: this is the
    // moment the fence has to be true — see [`Resume`].
    let from_revision = resume.mirror.read().revision;
    let dialed = tokio::time::timeout(leg(), async {
        IpcClient::connect(socket)
            .await?
            .resume_events(from_revision, &resume.session_id)
            .await
    })
    .await
    .map_err(|_| AttemptError::Transport(format!("{} timed out", ops::EVENTS_SUBSCRIBE)))?;

    let stream = match dialed {
        Ok(stream) => stream,
        Err(error) => {
            let Some(code) = error.server_code().filter(|code| {
                matches!(
                    code,
                    ServerCode::ReplayExpired
                        | ServerCode::RevisionAhead
                        | ServerCode::SessionMismatch
                )
            }) else {
                return Err(AttemptError::from(error));
            };
            tracing::info!(
                code = code.as_str(),
                from_revision,
                "the session refused to replay from here; subscribing and snapshotting instead"
            );
            return Ok(None);
        }
    };

    require_same_incarnation(&stream, identified)?;
    let ack = stream.revision();
    if ack != from_revision {
        // The ack echoes the fence by contract. A different one with no
        // snapshot behind it would leave the mirror silently stale —
        // every commit between the two discarded as already applied —
        // so this fails the attempt rather than trusting it.
        return Err(AttemptError::Transport(format!(
            "resume ack {ack} does not match from_revision {from_revision}"
        )));
    }
    let (events, pump) = spawn_event_pump(stream);
    Ok(Some((events, pump, Subscribed::Resumed { ack })))
}

/// Subscribe on a fresh connection, then snapshot on the control one,
/// and fence the snapshot against the ack.
///
/// The fence is the higher of the ack's revision and the snapshot's own
/// (`ipc.md` #eventssubscribe). Taking the max rather than the
/// snapshot's alone is the safe reading: a snapshot taken after the ack
/// can only be at or past it, so the pair is a floor either way.
async fn subscribe_and_snapshot(
    socket: &Path,
    control: &mut IpcClient,
    identified: &str,
) -> Result<(EventRx, EventPump, HostMirror), AttemptError> {
    // Bounded: this dials and handshakes, and a peer that accepts
    // without answering must not hold the connection in `Connecting`.
    let stream = tokio::time::timeout(leg(), EventStream::connect(socket))
        .await
        .map_err(|_| AttemptError::Transport(format!("{} timed out", ops::EVENTS_SUBSCRIBE)))??;
    require_same_incarnation(&stream, identified)?;
    let ack = stream.revision();
    let (events, pump) = spawn_event_pump(stream);
    let mirror = snapshot(control, ack).await?;
    Ok((events, pump, mirror))
}

/// `tab.list`, fenced against the subscribe ack.
async fn snapshot(control: &mut IpcClient, ack: u64) -> Result<HostMirror, AttemptError> {
    let raw = call(control, ops::TAB_LIST, serde_json::json!({})).await?;
    let list: TabListResult =
        serde_json::from_value(raw).map_err(|error| undecodable(ops::TAB_LIST, &error))?;
    snapshot_fence(list, ack)
}

/// Fence a snapshot against the subscribe ack, or refuse it.
///
/// A session socket carries the snapshot's revision by contract
/// (`ipc.md` #tablist); a UI socket omits it entirely because it serves
/// no stream to fence against. Falling back to the ack looks harmless
/// and is not: the ack is only a *floor*, so a snapshot actually taken
/// further ahead would leave the fence low and every batch in between
/// would be applied on top of a snapshot that already contains it.
/// Whatever answered without one is not the socket this client
/// subscribed to, so the attempt fails rather than guesses — retryable,
/// exactly like a dial that reached the wrong thing.
fn snapshot_fence(list: TabListResult, ack: u64) -> Result<HostMirror, AttemptError> {
    let Some(revision) = list.revision else {
        return Err(AttemptError::Transport(format!(
            "{} answered without a revision; a session socket must fence its snapshot",
            ops::TAB_LIST
        )));
    };
    Ok(HostMirror::from_list(list, revision.max(ack)))
}

/// Read the push stream on its own task.
///
/// `EventStream::next` is not cancel-safe — it buffers whole lines — so
/// it must never be a `select!` branch. A pump gives the connection loop
/// an `mpsc::Receiver` instead, which is.
fn spawn_event_pump(mut stream: EventStream) -> (EventRx, EventPump) {
    let (tx, rx) = mpsc::channel(EVENT_PUMP_DEPTH);
    let handle = tokio::spawn(async move {
        loop {
            let item = match stream.next().await {
                Ok(Some(frame)) => Ok(frame),
                // A clean close is a documented signal, not an error;
                // ending the channel is how the loop hears it.
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

/// How one round — dial, then serve — ended. Both halves funnel into
/// one handler in [`connect_loop`].
enum ConnEnd {
    Shutdown,
    FeedClosed,
    /// The compatibility gate refused. Only a dial can produce it.
    Incompatible(Box<super::state::BuildMismatch>),
    Stopping(String),
    Dropped(String),
    /// The connection cannot be made and no retry could change that.
    /// Terminal on every transport — see [`spawn_failure`].
    Settled {
        reason: String,
        detail: String,
    },
}

/// The steady state: drain events into the mirror and intents into the
/// control client, in the arrival order of whichever is ready.
async fn serve(
    config: &ConnectionConfig,
    incarnation: HostId,
    mut live: Live,
    ops_rx: &mut mpsc::Receiver<HostIntent>,
    feed: &EngineFeedSender,
    shutdown: &Shutdown,
    unsupported: &mut Unsupported,
) -> ConnEnd {
    loop {
        tokio::select! {
            biased;
            () = shutdown.requested() => return ConnEnd::Shutdown,
            frame = live.events.recv() => {
                match frame {
                    Some(Ok(EventFrame::Batch(batch))) => {
                        if !apply_batch(&live.mirror, batch, incarnation, feed) {
                            return ConnEnd::FeedClosed;
                        }
                    }
                    Some(Ok(EventFrame::Stopping(stopping))) => {
                        return ConnEnd::Stopping(stopping.reason);
                    }
                    Some(Err(error)) => {
                        // A revision gap is loss and nothing else, and
                        // the contract's answer to loss is a resync —
                        // not a reconnect. Everything else is the wire.
                        if !matches!(error, ClientError::RevisionGap { .. }) {
                            return ConnEnd::Dropped(error.to_string());
                        }
                        tracing::warn!(host = %config.host, %error, "resyncing the host mirror");
                        match resync(config, &mut live).await {
                            Ok(()) => {
                                if !publish_workspace(
                                    feed,
                                    incarnation,
                                    HostWorkspaceEvent::Reset(Arc::clone(&live.mirror)),
                                ) {
                                    return ConnEnd::FeedClosed;
                                }
                            }
                            Err(AttemptError::Stopping(reason)) => {
                                return ConnEnd::Stopping(reason)
                            }
                            // A resync that cannot re-subscribe is a
                            // dead connection; reconnecting is the same
                            // work one rung up.
                            Err(AttemptError::Transport(reason)) => {
                                return ConnEnd::Dropped(reason)
                            }
                            // A resync never climbs the launch ladder,
                            // so this is unreachable — but a verdict
                            // that says "no retry can fix this" is
                            // carried through rather than downgraded to
                            // a retryable drop if it ever arrives.
                            Err(AttemptError::Unrecoverable { reason, detail }) => {
                                return ConnEnd::Settled { reason, detail }
                            }
                            Err(AttemptError::Incompatible(_)) => {
                                return ConnEnd::Dropped(
                                    "the session changed build mid-stream".into(),
                                )
                            }
                        }
                    }
                    // The pump ended: the server closed the stream,
                    // which is itself the resync signal.
                    None => return ConnEnd::Dropped("the event stream closed".into()),
                }
            }
            intent = ops_rx.recv() => {
                let Some(intent) = intent else {
                    // Every sender is gone, so the app dropped this
                    // host. Nothing left to serve.
                    return ConnEnd::Shutdown;
                };
                match run_intent(&mut live, unsupported, intent).await {
                    IntentOutcome::Live => {}
                    IntentOutcome::Ends(end) => return end,
                }
            }
        }
    }
}

/// Move a live connection onto a re-established subscription.
///
/// The mirror handle is never replaced, only its contents — the UI holds
/// that `Arc` and a resync is not a new connection. A resume replaces
/// nothing at all: the fence it was granted is the one this very mirror
/// is at, and the gap arrives behind it as ordinary batches.
fn reseat(live: &mut Live, events: EventRx, pump: EventPump, what: Subscribed) {
    // Overwriting the guard is what retires the old pump — and it has
    // to happen before `events` is overwritten, or the stale
    // subscription's task keeps pushing onto a channel nobody drains.
    drop(std::mem::replace(&mut live.pump, pump));
    live.events = events;
    if let Subscribed::Fresh(mirror) = what {
        live.mirror.reset(mirror);
    }
}

/// What one control op left behind.
enum IntentOutcome {
    /// The control connection is still good.
    Live,
    /// The round is over: the session stated it is going away, or the
    /// wire under this op died. `IpcClient` is strictly sequential, so a
    /// leg that failed mid-op cannot be reused — the reply the op
    /// abandoned would be read as the next one's.
    Ends(ConnEnd),
}

/// Send one queued op through the control client and answer its caller.
///
/// `unsupported` is the task's, not this connection's: see
/// [`Unsupported`].
async fn run_intent(
    live: &mut Live,
    unsupported: &mut Unsupported,
    mut intent: HostIntent,
) -> IntentOutcome {
    // Taken rather than cloned: the params are this intent's alone, and
    // `answer` never reads them.
    let params = std::mem::take(&mut intent.params);

    let op = intent.op.clone();
    let sent = tokio::time::timeout(op_budget(&op), live.control.call_raw(&op, params)).await;
    match sent {
        Ok(Ok(result)) => {
            intent.answer(Ok(result));
            IntentOutcome::Live
        }
        Ok(Err(error)) => {
            let (fault, surfaced) = queue::classify(&error);
            // An older session refusing an op it never had is an
            // ordinary `Surfaced` refusal — the connection is fine, and
            // the client is not going to stop having a focus or a config
            // to state — so it is said once and then let be.
            if let Some(note) = unsupported.note(&op, &surfaced) {
                tracing::info!("{note}");
            }
            intent.answer(Err(surfaced));
            match fault {
                OpFault::Surfaced => IntentOutcome::Live,
                OpFault::ShuttingDown => IntentOutcome::Ends(ConnEnd::Stopping("stop".into())),
                OpFault::Transport(reason) => IntentOutcome::Ends(ConnEnd::Dropped(reason)),
            }
        }
        Err(_elapsed) => {
            let timed_out = format!("{op} timed out");
            intent.answer(Err(HostOpError::Transport(timed_out.clone())));
            IntentOutcome::Ends(ConnEnd::Dropped(timed_out))
        }
    }
}

/// Rebuild the mirror after a revision gap: a fresh subscription,
/// replaying the gap where the session can and taking a fresh
/// `tab.list` — fenced against the new ack exactly as at connect —
/// where it cannot.
async fn resync(config: &ConnectionConfig, live: &mut Live) -> Result<(), AttemptError> {
    let resume = live.checkpoint();
    let plan = SubscribePlan::resuming(&resume);
    let (events, pump, subscribed) = subscribe(&config.socket, &mut live.control, plan).await?;
    reseat(live, events, pump, subscribed);
    Ok(())
}

/// Fold a batch in and publish the wake. `false` means the feed is gone.
///
/// The mirror is written in place, so what crosses the feed is the
/// revision and the envelopes the mirror does not model — never a copy
/// of the workspace.
fn apply_batch(
    mirror: &SharedMirror,
    batch: EventBatch,
    incarnation: HostId,
    feed: &EngineFeedSender,
) -> bool {
    let revision = batch.revision;
    if !mirror.apply_batch(&batch) {
        // Below the fence: the snapshot already has it.
        return true;
    }
    publish_workspace(
        feed,
        incarnation,
        HostWorkspaceEvent::Applied {
            revision,
            events: batch.events,
        },
    )
}

fn publish_state(feed: &EngineFeedSender, host: HostId, state: HostConnState) -> bool {
    feed.send(EngineFeed::HostState(host, state))
}

fn publish_workspace(feed: &EngineFeedSender, host: HostId, event: HostWorkspaceEvent) -> bool {
    feed.send(EngineFeed::HostWorkspace(host, event))
}

fn publish_facts(feed: &EngineFeedSender, host: HostId, facts: ConnectFacts) -> bool {
    feed.send(EngineFeed::HostConnectFacts(host, facts))
}

async fn call(
    client: &mut IpcClient,
    op: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, AttemptError> {
    tokio::time::timeout(leg(), client.call_raw(op, params))
        .await
        .map_err(|_| AttemptError::Transport(format!("{op} timed out")))?
        .map_err(AttemptError::from)
}

/// A result that did not decode is schema drift, not a dead wire — but
/// the connection is finished either way, so it lands as `Transport`
/// with the op named.
fn undecodable(op: &str, error: &serde_json::Error) -> AttemptError {
    AttemptError::Transport(format!("{op} did not decode: {error}"))
}

/// A jitter source with no `rand` dependency: the low bits of the
/// monotonic clock are plenty of spread for staggering reconnects, and
/// this is not a security decision.
pub(super) fn jitter() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1_000) / 1_000.0
}

#[cfg(test)]
mod tests {
    use roost_ipc::messages::{EventEnvelope, EventsSubscribeParams, Project};

    use super::*;

    fn feed_items(rx: &mut crate::engine_feed::EngineFeedReceiver) -> Vec<EngineFeed> {
        let mut batch = crate::engine_feed::EngineBatch::default();
        std::iter::from_fn(|| rx.try_next(&mut batch)).collect()
    }

    fn seeded_list(revision: Option<u64>) -> TabListResult {
        seeded_list_named("p", revision)
    }

    /// A one-project snapshot named after the session that answers it, so
    /// a mirror built from one incarnation can be told from another's.
    fn seeded_list_named(project: &str, revision: Option<u64>) -> TabListResult {
        TabListResult {
            projects: vec![Project {
                id: 1,
                name: project.into(),
                cwd: "/tmp".into(),
                position: 0,
                created_at: 0,
                tabs: Vec::new(),
            }],
            revision,
        }
    }

    fn seeded_mirror(revision: u64) -> SharedMirror {
        SharedMirror::new(HostMirror::from_list(seeded_list(Some(revision)), revision))
    }

    /// The `session.identify` result every fake session in this module
    /// answers with — the one shape that clears the compatibility gate
    /// against the build [`config`] claims.
    fn identify_result(session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "app_version": "test",
            "session_protocol": roost_ipc::messages::SESSION_PROTOCOL_VERSION,
            "payload_kinds": super::super::state::CLIENT_PAYLOAD_KINDS,
            "libghostty_build": "gb",
            "session_id": session_id,
            "started_at": "2026-01-01T00:00:00Z",
        })
    }

    /// The session id a [`Fake`] names unless a test names another, and
    /// the one a checkpoint has to name to be offered.
    const SESSION_ID: &str = "s1";

    /// The revision every fake session's `tab.list` and fresh subscribe
    /// agree on.
    const SESSION_REVISION: u64 = 1;

    fn settled(error: &AttemptError) -> (&str, &str) {
        match error {
            AttemptError::Unrecoverable { reason, detail } => (reason.as_str(), detail.as_str()),
            other => panic!("expected a settled verdict, got {other:?}"),
        }
    }

    fn transport(error: &AttemptError) -> &str {
        match error {
            AttemptError::Transport(reason) => reason.as_str(),
            other => panic!("expected a retryable verdict, got {other:?}"),
        }
    }

    /// Every row of the classification table (plan 042 §3.2), including
    /// the two that stay retryable — the ones that decide whether a
    /// localhost host keeps a 250 ms ladder running against a socket
    /// nothing will create.
    #[test]
    fn every_spawn_stage_gets_its_verdict() {
        let plain = anyhow::anyhow!("the whole story");

        let locate = spawn_failure(SpawnStage::Locate, &plain);
        assert_eq!(
            settled(&locate),
            ("cannot find roost-session", "the whole story")
        );

        let cwd = spawn_failure(SpawnStage::Cwd, &plain);
        assert_eq!(
            settled(&cwd),
            ("roost-session failed to start", "the whole story")
        );

        let verdict = spawn_failure(SpawnStage::Verdict, &plain);
        assert_eq!(
            settled(&verdict),
            ("roost-session failed to start", "the whole story")
        );

        // No io error in the chain: the binary was exec'd and the read
        // of its verdict is what expired, so a dial is the right retry.
        assert_eq!(
            transport(&spawn_failure(SpawnStage::Launch, &plain)),
            "the whole story"
        );
        assert_eq!(
            transport(&spawn_failure(SpawnStage::Confirm, &plain)),
            "the whole story"
        );

        let exec = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("spawn /nope/roost-session");
        let launched = spawn_failure(SpawnStage::Launch, &exec);
        assert_eq!(
            settled(&launched).0,
            "roost-session failed to start",
            "an exec that failed can never be retried: only the first attempt may spawn"
        );
        assert!(settled(&launched).1.contains("spawn /nope/roost-session"));
    }

    /// The invariant [`spawn_failure`]'s `Launch` arm rests on, against
    /// the real function rather than a synthetic chain: a failed exec
    /// carries an `io::Error`, and a successful exec whose verdict never
    /// arrives does not.
    #[tokio::test]
    async fn only_a_failed_exec_puts_an_io_error_in_the_verdict_chain() {
        let budget = Duration::from_secs(5);
        let cwd = Path::new("/");

        let missing = session_launch::spawn_and_read_verdict(
            Path::new("/nonexistent/roost-session"),
            cwd,
            None,
            budget,
        )
        .await
        .expect_err("a binary that is not there cannot be exec'd");
        assert_eq!(
            settled(&spawn_failure(SpawnStage::Launch, &missing)).0,
            "roost-session failed to start"
        );

        // `true start` exec's fine and closes its stdout without a
        // readiness line — the shape of a daemon that died on startup
        // *after* the exec, which stays retryable.
        let no_verdict =
            session_launch::spawn_and_read_verdict(Path::new("/usr/bin/true"), cwd, None, budget)
                .await
                .expect_err("no line, so no verdict");
        transport(&spawn_failure(SpawnStage::Launch, &no_verdict));
    }

    /// The launch-path twin: a redial of a socket that was never there
    /// says so, instead of leaking `roost_ipc`'s `io error: No such
    /// file` over the honest reason the first attempt published.
    #[test]
    fn a_redial_of_a_missing_socket_says_no_session_is_running() {
        let socket = Path::new("/run/roost/roost.sock");
        let missing = roost_ipc::Error::Io(std::io::ErrorKind::NotFound.into());
        let refused = roost_ipc::Error::Io(std::io::ErrorKind::ConnectionRefused.into());
        let expected = "no session is running at /run/roost/roost.sock";

        assert_eq!(
            transport(&dial_failure(ConnectMode::Dial, socket, &missing)),
            expected
        );
        assert_eq!(
            transport(&dial_failure(ConnectMode::Dial, socket, &refused)),
            expected
        );

        // A mode that probed first found the socket live; it going away
        // between the probe and the dial is a race, and reporting it
        // literally is what makes that visible.
        assert_ne!(
            transport(&dial_failure(ConnectMode::IfPresent, socket, &missing)),
            expected
        );
        // And an error that is not the socket's absence is never
        // rewritten into a claim about the socket.
        assert_ne!(
            transport(&dial_failure(
                ConnectMode::Dial,
                socket,
                &roost_ipc::Error::Io(std::io::ErrorKind::PermissionDenied.into()),
            )),
            expected
        );
    }

    /// The pass-through contract C5 depends on: the mirror models
    /// workspace facts, and everything it does not model — `tab.effect`
    /// above all — reaches the feed verbatim inside the applied batch.
    ///
    /// And the mirror itself does *not*: the feed carries the wake, the
    /// shared handle carries the state.
    #[tokio::test]
    async fn an_applied_batch_carries_its_effects_through_verbatim() {
        let (feed, mut rx) = crate::engine_feed::channel();
        let mirror = seeded_mirror(4);
        let host = HostId::new(9);
        let effect = EventEnvelope {
            event: ops::EVENT_TAB_EFFECT.into(),
            data: serde_json::json!({"tab_id": "7", "effect": "bell"}),
        };

        assert!(apply_batch(
            &mirror,
            EventBatch {
                revision: 5,
                events: vec![effect.clone()],
            },
            host,
            &feed,
        ));

        let items = feed_items(&mut rx);
        assert_eq!(items.len(), 1);
        let EngineFeed::HostWorkspace(tagged, event) = &items[0] else {
            panic!("a mirror delta is a HostWorkspace item");
        };
        assert_eq!(*tagged, host);
        let HostWorkspaceEvent::Applied { revision, events } = event else {
            panic!("expected an applied batch");
        };
        assert_eq!(*revision, 5);
        assert_eq!(events, &[effect], "the envelope reaches C5 untouched");
        assert_eq!(
            mirror.read().revision,
            5,
            "and the state moved on the shared mirror, not on the feed"
        );
    }

    /// A batch at or below the fence is already in the snapshot, so it
    /// must not reach the UI at all — publishing it would replay commits
    /// the mirror was built from.
    #[tokio::test]
    async fn a_batch_below_the_fence_publishes_nothing() {
        let (feed, mut rx) = crate::engine_feed::channel();
        let mirror = seeded_mirror(4);

        assert!(apply_batch(
            &mirror,
            EventBatch {
                revision: 4,
                events: vec![EventEnvelope {
                    event: ops::EVENT_PROJECT_DELETED.into(),
                    data: serde_json::json!({"project_id": "1"}),
                }],
            },
            HostId::new(9),
            &feed,
        ));
        assert!(feed_items(&mut rx).is_empty());
        assert_eq!(mirror.read().projects.len(), 1, "and nothing was applied");
    }

    /// The hazard [`Resume::freeze`] exists for, walked end to end: a
    /// commit the replaced connection folds in *after* the set has
    /// seeded its replacement is still inside the gap the replacement
    /// asks for, and the `notification.fired` it carries reaches the UI
    /// exactly once, under the new incarnation.
    #[tokio::test]
    async fn a_frozen_checkpoint_survives_the_writer_it_replaces() {
        let (feed, mut rx) = crate::engine_feed::channel();
        let held = Arc::new(seeded_mirror(7));
        let seeded = Resume {
            session_id: "s1".into(),
            mirror: Arc::clone(&held),
        }
        .freeze();

        let missed = EventBatch {
            revision: 8,
            events: vec![EventEnvelope {
                event: ops::EVENT_TAB_NOTIFICATION.into(),
                data: serde_json::json!({"tab_id": "7", "has_notification": true}),
            }],
        };
        // The old task's last apply, landing after the seed.
        assert!(held.apply_batch(&missed));

        // The new task reads its fence here and offers it; commit 8 is
        // inside the gap, so the session replays it.
        assert_eq!(
            seeded.mirror.read().revision,
            7,
            "a shared handle would already have been advanced to 8"
        );
        assert!(apply_batch(
            &seeded.mirror,
            missed.clone(),
            HostId::new(9),
            &feed,
        ));

        let published: Vec<u64> = feed_items(&mut rx)
            .into_iter()
            .filter_map(|item| match item {
                EngineFeed::HostWorkspace(_, HostWorkspaceEvent::Applied { revision, .. }) => {
                    Some(revision)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            published,
            vec![8],
            "the replayed commit must reach the UI exactly once"
        );
    }

    /// A run of commits costs one mirror and N wakes, never N mirrors.
    /// The old shape put a full workspace clone on an *unbounded*
    /// channel per commit, so a chatty host grew the client without
    /// bound.
    #[tokio::test]
    async fn a_burst_of_commits_publishes_wakes_and_not_workspaces() {
        let (feed, mut rx) = crate::engine_feed::channel();
        let mirror = seeded_mirror(0);
        for revision in 1..=64 {
            assert!(apply_batch(
                &mirror,
                EventBatch {
                    revision,
                    events: Vec::new(),
                },
                HostId::new(9),
                &feed,
            ));
        }

        let items = feed_items(&mut rx);
        assert_eq!(items.len(), 64);
        for item in &items {
            let EngineFeed::HostWorkspace(_, HostWorkspaceEvent::Applied { .. }) = item else {
                panic!("a commit is an applied wake and nothing more");
            };
        }
        assert_eq!(
            mirror.read().revision,
            64,
            "the drain reads the latest state, not the one that woke it"
        );
    }

    #[test]
    fn jitter_stays_inside_the_unit_interval() {
        for _ in 0..1_000 {
            let value = jitter();
            assert!((0.0..=1.0).contains(&value), "{value}");
        }
    }

    /// A config for a host that is not there. Only the three fields the
    /// spawn rules read differ between these cases.
    fn config(socket: PathBuf, transport: HostTransport, mode: ConnectMode) -> ConnectionConfig {
        ConnectionConfig {
            host: "h1".into(),
            label: "local".into(),
            socket,
            transport,
            generation: 1,
            supersedes: None,
            mode,
            resume: None,
            client_build: "gb".into(),
            theme: Arc::new(Mutex::new(super::super::blank_theme())),
            uploads: Uploads::default(),
        }
    }

    /// A dial-mode attempt never probes and never spawns, which is what
    /// makes a retry after a mid-session drop safe to run on any host.
    #[tokio::test]
    async fn dial_mode_never_touches_the_spawn_ladder() {
        let config = config(
            PathBuf::from("/nonexistent/roost-host-conn-test.sock"),
            HostTransport::LocalSession,
            ConnectMode::Dial,
        );
        assert!(ensure_socket(&config, ConnectMode::Dial).await.is_ok());
    }

    /// Connect-if-present is the launch rule: an absent socket is a
    /// disconnected host, never a spawned daemon.
    #[tokio::test]
    async fn if_present_reports_an_absent_socket_without_spawning() {
        let socket = std::env::temp_dir().join(format!(
            "roost-host-conn-absent-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&socket);
        let config = config(socket, HostTransport::LocalSession, ConnectMode::IfPresent);
        let error = ensure_socket(&config, ConnectMode::IfPresent)
            .await
            .expect_err("an absent socket is not a connection");
        let AttemptError::Transport(reason) = error else {
            panic!("an absent socket is a transport outcome");
        };
        assert!(reason.contains("no session is running"), "{reason}");
    }

    /// Spawn-if-missing is localhost-only: an `ssh -L` forward that is
    /// down is not something this client can start.
    #[tokio::test]
    async fn a_remote_host_is_never_spawned() {
        let config = config(
            PathBuf::from("/nonexistent/roost-host-conn-remote.sock"),
            HostTransport::UnixSocket,
            ConnectMode::SpawnIfMissing,
        );
        let error = spawn_session(&config).await.expect_err("no spawn");
        let AttemptError::Transport(reason) = error else {
            panic!("expected a transport outcome");
        };
        assert!(reason.contains("localhost"), "{reason}");
    }

    /// The fence rule from `ipc.md` #tablist: a session socket carries
    /// the snapshot's revision. Falling back to the subscribe ack looks
    /// harmless and is not — the ack is only a floor, so a snapshot
    /// taken further ahead would leave the fence low and the batches in
    /// between would be applied twice. The attempt fails instead.
    #[tokio::test]
    async fn a_snapshot_without_a_revision_fails_the_attempt() {
        let error = super::snapshot_fence(seeded_list(None), 7)
            .expect_err("a UI socket's answer is not a session's");
        let AttemptError::Transport(reason) = error else {
            panic!("a contract violation is a failed, retryable attempt");
        };
        assert!(reason.contains("revision"), "{reason}");

        // The pair still fences at the higher of the two.
        let mirror = super::snapshot_fence(seeded_list(Some(4)), 9).unwrap();
        assert_eq!(mirror.revision, 9, "the ack is a floor");
        let mirror = super::snapshot_fence(seeded_list(Some(12)), 9).unwrap();
        assert_eq!(mirror.revision, 12, "and the snapshot may be past it");
    }

    /// The disconnect signal has to be level-triggered: a task that is
    /// between two `select!` arms when it fires still has to see it,
    /// because the signal is now the *only* thing that stops the task.
    #[tokio::test]
    async fn a_disconnect_signalled_with_nobody_parked_is_still_seen() {
        let shutdown = Shutdown::default();
        shutdown.request();
        tokio::time::timeout(Duration::from_secs(5), shutdown.requested())
            .await
            .expect("a signal raised before the wait must not be lost");
        // And it stays raised for every later waiter.
        tokio::time::timeout(Duration::from_secs(5), shutdown.requested())
            .await
            .expect("the signal is level-triggered, not a one-shot");
    }

    /// Both of a task's waiters — the connection loop and its grace
    /// timer — must wake. Waking only the first-parked would leave the
    /// loop asleep behind the timer and turn every disconnect into a
    /// cancellation at the end of the grace.
    #[tokio::test]
    async fn a_disconnect_wakes_every_waiter_that_is_parked_on_it() {
        let shutdown = Arc::new(Shutdown::default());
        let parked: Vec<_> = (0..2)
            .map(|_| {
                let waiting = Arc::clone(&shutdown);
                tokio::spawn(async move { waiting.requested().await })
            })
            .collect();
        tokio::task::yield_now().await;

        shutdown.request();

        for waiter in parked {
            tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .expect("every parked waiter must wake")
                .expect("and not panic");
        }
    }

    /// The disconnect contract end to end, and what lets `HostConn::drop`
    /// signal instead of aborting: the task ends, and everything on its
    /// queue is *answered* rather than dropped with its reply channel.
    #[tokio::test]
    async fn a_finished_task_answers_its_queue_and_then_refuses() {
        let (feed, _rx) = crate::engine_feed::channel();
        let (ops, ops_rx) = super::super::queue::HostOps::channel();
        let shutdown = Arc::new(Shutdown::default());
        let queued = ops.call("tab.open", serde_json::json!({}));

        let task = tokio::spawn(run(
            // Remote + dial, so the attempt fails at once and no retry
            // is scheduled: the task reaches its epilogue on its own.
            config(
                PathBuf::from("/nonexistent/roost-host-conn-epilogue.sock"),
                HostTransport::UnixSocket,
                ConnectMode::Dial,
            ),
            HostIdMinter::new(),
            ops_rx,
            feed,
            Arc::clone(&shutdown),
        ));

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), queued)
                .await
                .expect("a queued intent must be answered, not stranded"),
            Err(HostOpError::Disconnected)
        );
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the task ends")
            .expect("and does not panic");
        assert_eq!(
            ops.call("tab.open", serde_json::json!({})).await,
            Err(HostOpError::Unavailable),
            "the closed queue refuses rather than swallowing"
        );
    }

    #[test]
    fn a_shutting_down_refusal_becomes_a_stop() {
        let stop = AttemptError::from(ClientError::Server {
            code: "shutting-down".into(),
            message: "latched".into(),
        });
        assert!(matches!(stop, AttemptError::Stopping(reason) if reason == "stop"));

        let other = AttemptError::from(ClientError::Server {
            code: "invalid-param".into(),
            message: "nope".into(),
        });
        assert!(matches!(other, AttemptError::Transport(_)));
    }

    fn refused(code: ServerCode) -> HostOpError {
        HostOpError::Rejected {
            code,
            message: "no such op: whatever".into(),
        }
    }

    /// The latch itself: one sentence per op, and each op independent of
    /// the others.
    ///
    /// This says nothing about *who owns* the latch, which is the half
    /// that actually decides whether the user sees one line or one per
    /// reconnect — that is
    /// [`an_old_session_is_noted_once_across_reconnects`], driven through
    /// the real task.
    #[test]
    fn a_refusal_is_noted_once_per_op_and_never_again() {
        let mut flags = Unsupported::default();

        let first = flags
            .note(
                ops::SESSION_SET_AGENT_HOOKS,
                &refused(ServerCode::UnknownOp),
            )
            .expect("the first refusal is worth saying");
        assert!(first.contains("session.set_agent_hooks"), "{first}");
        assert!(
            flags
                .note(
                    ops::SESSION_SET_AGENT_HOOKS,
                    &refused(ServerCode::UnknownOp)
                )
                .is_none(),
            "every reconnect re-sends it; only the first refusal is news"
        );

        // Independent latches: an old session refuses both, and each
        // gets its own sentence.
        assert!(flags
            .note(ops::SESSION_SET_FOCUS, &refused(ServerCode::UnknownOp))
            .is_some_and(|note| note.contains("session.set_focus")));
        assert!(flags
            .note(ops::SESSION_SET_FOCUS, &refused(ServerCode::UnknownOp))
            .is_none());
    }

    /// Everything `tracing` emitted while a guard was held.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    /// A session that serves the whole prologue, refuses
    /// `session.set_agent_hooks` as `unknown-op`, and then goes away so
    /// the client reconnects.
    ///
    /// **Goes away entirely**, not just on the control connection: every
    /// open connection ends on the same generation bump.
    ///
    /// The `events.subscribe` counter is the script, and it is
    /// deterministic because the client's own ladder is: **#1** is the
    /// first connection's real subscribe; **#2** is the retry's, refused
    /// so it drops and tries again; **#3** is the second connection's
    /// real subscribe; **#4** is answered `shutting-down`, which is
    /// terminal and is what ends the task.
    ///
    /// Returns how many agent-hooks requests it served, so the test can
    /// assert the op really reached two separate connections rather than
    /// silently one.
    fn a_session_that_never_heard_of_agent_hooks(socket: &Path) -> Arc<Mutex<usize>> {
        let listener = tokio::net::UnixListener::bind(socket).expect("bind a fake session");
        let served = Arc::new(Mutex::new(0usize));
        let counted = Arc::clone(&served);
        let subscribes = Arc::new(Mutex::new(0u32));
        let (bump, generation) = tokio::sync::watch::channel(0u32);
        let bump = Arc::new(bump);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let served = Arc::clone(&counted);
                let subscribes = Arc::clone(&subscribes);
                let bump = Arc::clone(&bump);
                let mut generation = generation.clone();
                generation.mark_unchanged();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut lines =
                        tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(reader));
                    loop {
                        let line = tokio::select! {
                            _ = generation.changed() => return,
                            line = lines.next_line() => match line {
                                Ok(Some(line)) => line,
                                _ => return,
                            },
                        };
                        let request: serde_json::Value =
                            serde_json::from_str(&line).expect("a request");
                        let id = request["id"].clone();
                        let op = request["op"].as_str().unwrap_or_default().to_string();
                        let mut close_after = false;
                        let response = match op.as_str() {
                            ops::SESSION_IDENTIFY => serde_json::json!({
                                "id": id,
                                "ok": true,
                                "result": identify_result(SESSION_ID),
                            }),
                            ops::SESSION_SET_THEME => {
                                serde_json::json!({"id": id, "ok": true, "result": {}})
                            }
                            ops::TAB_LIST => serde_json::json!({
                                "id": id,
                                "ok": true,
                                "result": seeded_list(Some(1)),
                            }),
                            ops::EVENTS_SUBSCRIBE => {
                                let nth = {
                                    let mut count = subscribes.lock().unwrap();
                                    *count += 1;
                                    *count
                                };
                                match nth {
                                    // A refusal the ladder retries, so
                                    // the second ensure lands on a third
                                    // connection.
                                    2 => serde_json::json!({
                                        "id": id,
                                        "ok": false,
                                        "error": {
                                            "code": "internal",
                                            "message": "not this time",
                                        },
                                    }),
                                    // And the one that ends the task, so
                                    // `run` returns and the log can be
                                    // read.
                                    4 => serde_json::json!({
                                        "id": id,
                                        "ok": false,
                                        "error": {
                                            "code": "shutting-down",
                                            "message": "going away",
                                        },
                                    }),
                                    // A real subscribe. Answering and
                                    // then simply reading on is what a
                                    // push connection looks like: the
                                    // client sends nothing more on it,
                                    // and it ends when the client
                                    // closes it.
                                    _ => serde_json::json!({
                                        "id": id,
                                        "ok": true,
                                        "result": { "revision": 1, "session_id": SESSION_ID },
                                    }),
                                }
                            }
                            ops::SESSION_SET_AGENT_HOOKS => {
                                *served.lock().unwrap() += 1;
                                // The session going away after the
                                // refusal is what makes this a
                                // *reconnect* rather than one long
                                // connection with two ensures on it.
                                // The bump takes the event stream with
                                // it; `close_after` ends this one after
                                // the refusal is on the wire.
                                bump.send_modify(|generation| *generation += 1);
                                close_after = true;
                                serde_json::json!({
                                    "id": id,
                                    "ok": false,
                                    "error": {
                                        "code": "unknown-op",
                                        "message": "no such op: session.set_agent_hooks",
                                    },
                                })
                            }
                            _ => serde_json::json!({
                                "id": id,
                                "ok": false,
                                "error": { "code": "internal", "message": "refused" },
                            }),
                        };
                        let mut body = serde_json::to_vec(&response).expect("encode a response");
                        body.push(b'\n');
                        if tokio::io::AsyncWriteExt::write_all(&mut writer, &body)
                            .await
                            .is_err()
                            || close_after
                        {
                            break;
                        }
                    }
                });
            }
        });
        served
    }

    /// **An old session costs one line for the life of the connection
    /// task, not one per reconnect.**
    ///
    /// `session.set_agent_hooks` is re-sent on every connect and a
    /// dropped localhost session reconnects on a 250 ms ladder, so
    /// a latch rebuilt per connection would repeat the same sentence
    /// about the same unchanging session forever. Driven through the real
    /// [`run`] against a session that refuses the op, across two
    /// connections, because that is the only place the latch's *owner*
    /// is observable — asserting on `Unsupported` alone would pass just
    /// as well if nothing ever called it.
    #[tokio::test]
    async fn an_old_session_is_noted_once_across_reconnects() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("old-session.sock");
        let served = a_session_that_never_heard_of_agent_hooks(&socket);

        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::INFO)
            .without_time()
            .with_ansi(false)
            .finish();
        let restore = tracing::subscriber::set_default(subscriber);

        // Localhost, so a dropped connection is retried by this same
        // task — which is exactly the reconnect under test.
        let config = config(socket, HostTransport::LocalSession, ConnectMode::Dial);
        let (feed, _rx) = crate::engine_feed::channel();
        let (ops_tx, ops_rx) = super::super::HostOps::channel();
        // The app re-sends the op on every connected edge; this stands in
        // for that, and keeps sending so each connection serves one.
        let asking = tokio::spawn(async move {
            loop {
                let _ = ops_tx
                    .call(
                        ops::SESSION_SET_AGENT_HOOKS,
                        serde_json::json!({"mode": "auto", "skip": [], "client": "t"}),
                    )
                    .await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        run(
            config,
            HostIdMinter::new(),
            ops_rx,
            feed,
            Arc::new(Shutdown::default()),
        )
        .await;
        asking.abort();
        drop(restore);

        assert!(
            *served.lock().unwrap() >= 2,
            "the op has to reach two separate connections for this to prove anything, \
             not {}",
            served.lock().unwrap()
        );
        let text = logs.text();
        assert_eq!(
            text.matches("predates session.set_agent_hooks").count(),
            1,
            "an old session is one line, whatever the reconnect count: {text}"
        );
    }

    /// Only `unknown-op` is an old session. Folding anything else in
    /// here would silence a real refusal — a `shutting-down` says the
    /// session is going away, which is the opposite of "this is fine".
    #[test]
    fn any_other_refusal_is_not_an_old_session() {
        let mut flags = Unsupported::default();
        for error in [
            refused(ServerCode::ShuttingDown),
            refused(ServerCode::Internal),
            HostOpError::Transport("the wire died".into()),
            HostOpError::Disconnected,
        ] {
            assert!(flags.note(ops::SESSION_SET_AGENT_HOOKS, &error).is_none());
        }
        // And the latch was never spent, so the real thing still gets
        // its line.
        assert!(flags
            .note(
                ops::SESSION_SET_AGENT_HOOKS,
                &refused(ServerCode::UnknownOp)
            )
            .is_some());
    }

    /// The op that runs a five-agent install on a possibly NFS-mounted
    /// `$HOME` does not share the budget sized for a `tab.list`.
    #[test]
    fn the_agent_hooks_op_gets_its_own_budget() {
        assert!(op_budget(ops::SESSION_SET_AGENT_HOOKS) > op_budget(ops::TAB_LIST));
        assert_eq!(op_budget(ops::TAB_LIST), leg());
    }

    // ---- the upload lane (plan 047 §3.3) -------------------------------

    use std::sync::atomic::{AtomicU64, AtomicUsize};

    use roost_ipc::messages::{SessionPutFileParams, SessionPutFileResult};
    use tokio::sync::oneshot;

    use super::super::upload::{UploadSource, Uploads};

    /// Where [`Fake`] claims to have landed a file — the shape §3.1
    /// pins, all of it inside the paste-safe grammar.
    const FILES_ROOT: &str = "/home/c/.cache/roost-session/files/4b9d1e7f0a3c5e21";

    /// How [`Fake`] answers `session.put_file`.
    #[derive(Clone)]
    enum PutFile {
        /// Land it under [`FILES_ROOT`] and answer honestly.
        Land,
        /// Read the first `n` frames and never answer them; land the
        /// rest. `usize::MAX` is "never answer anything".
        HoldFirst(usize),
        /// Answer with this, whatever was sent.
        Reply(SessionPutFileResult),
        /// Refuse with this code.
        Refuse(&'static str),
    }

    /// How [`Fake`] answers `events.subscribe`.
    ///
    /// Every arm scripts the **resume** only. A fresh subscribe always
    /// succeeds, because a refusal that had nothing to fall back to
    /// would prove nothing about the fallback.
    #[derive(Clone, Copy)]
    enum Subscribe {
        /// Serve it: the ack echoes the fence, as the contract requires.
        Serve,
        /// Ack this revision instead — a session contradicting its own
        /// echo.
        Ack(u64),
        /// Refuse with this code.
        Refuse(&'static str),
        /// Close the connection without answering, while
        /// [`Fake::resume_drops`] lasts — the wire dying on the resume
        /// dial, and then healing.
        Close,
    }

    /// Count one send of a scripted op down, and say whether the script
    /// arms on this one: the counter runs out when `checked_sub` returns
    /// `None`, which is the `Err` this reads.
    fn armed(skips: &AtomicUsize) -> bool {
        skips
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |skips| {
                skips.checked_sub(1)
            })
            .is_err()
    }

    /// A session that serves the whole prologue, scripts
    /// `session.put_file`, and can hold one control op unanswered or cut
    /// its event stream on cue.
    ///
    /// Every connection is served by its own task, which is the point:
    /// an upload dials the socket afresh, so the stall below holds the
    /// *control* connection and nothing else.
    #[derive(Clone)]
    struct Fake {
        /// The incarnation this session names on every ack — `identify`
        /// and `events.subscribe` alike, which is what lets a test point
        /// the two legs of one subscribe at two sessions.
        session_id: &'static str,
        put_file: PutFile,
        /// A control op this session reads and does not answer until
        /// [`Self::release`] is signalled.
        stall: Option<&'static str>,
        /// How many times [`Self::stall`]'s op is answered normally
        /// before the stall arms. `0` stalls the first one; a test whose
        /// subject is a *reconnect* skips the first connection's.
        stall_skips: Arc<AtomicUsize>,
        release: Arc<Shutdown>,
        /// Raised once the stalled op has been read.
        stalled: Arc<Shutdown>,
        /// Raised once a `session.put_file` frame has been read.
        uploading: Arc<Shutdown>,
        /// Close a subscribed connection when this is signalled, at most
        /// `cuts` times — the signal is level-triggered, so a reconnect
        /// must not be cut by the same one.
        cut: Arc<Shutdown>,
        cuts: Arc<AtomicUsize>,
        /// Every `session.identify` — the op a *reconnect* re-runs.
        identifies: Arc<AtomicUsize>,
        /// Every `session.set_theme`, so a re-seeded palette is
        /// countable.
        themes: Arc<AtomicUsize>,
        /// One permit, one `event.batch` frame written onto a subscribed
        /// connection. Edge-triggered on purpose: the level-triggered
        /// signals above cannot express "one more, now".
        emit: Arc<tokio::sync::Semaphore>,
        /// The revision the next emitted batch carries, starting one past
        /// the fence every prologue agrees on.
        next_emit: Arc<AtomicU64>,
        /// A refusal this session answers one named op with, whatever
        /// else it would have said. Unlike [`Self::theme_refusal`] it can
        /// name an op the prologue never sends, so a connection can be
        /// established and *then* refused.
        refuse_op: Option<(&'static str, &'static str)>,
        /// How many times [`Self::refuse_op`]'s op is answered normally
        /// before the refusal arms, so an op the prologue *does* send
        /// can still be refused on the round after it.
        refuse_skips: Arc<AtomicUsize>,
        puts: Arc<AtomicUsize>,
        dials: Arc<AtomicUsize>,
        subscribe: Subscribe,
        /// Remaining resume offers [`Subscribe::Close`] hangs up on.
        resume_drops: Arc<AtomicUsize>,
        /// Every `events.subscribe` this session was sent, in order.
        /// The record "did this reconnect resume?" is read off.
        subscribes: Arc<Mutex<Vec<EventsSubscribeParams>>>,
        /// How many `tab.list` snapshots it has been asked for. A
        /// resumed reconnect takes none, which is the whole of R11.
        tab_lists: Arc<AtomicUsize>,
        /// Subscribed connections whose peer has hung up. Read through
        /// [`Self::held_streams`].
        streams_ended: Arc<AtomicUsize>,
    }

    impl Fake {
        fn new(put_file: PutFile) -> Fake {
            Fake {
                session_id: SESSION_ID,
                put_file,
                stall: None,
                stall_skips: Arc::new(AtomicUsize::new(0)),
                release: Arc::new(Shutdown::default()),
                stalled: Arc::new(Shutdown::default()),
                uploading: Arc::new(Shutdown::default()),
                cut: Arc::new(Shutdown::default()),
                cuts: Arc::new(AtomicUsize::new(0)),
                identifies: Arc::new(AtomicUsize::new(0)),
                themes: Arc::new(AtomicUsize::new(0)),
                emit: Arc::new(tokio::sync::Semaphore::new(0)),
                next_emit: Arc::new(AtomicU64::new(SESSION_REVISION + 1)),
                refuse_op: None,
                refuse_skips: Arc::new(AtomicUsize::new(0)),
                puts: Arc::new(AtomicUsize::new(0)),
                dials: Arc::new(AtomicUsize::new(0)),
                subscribe: Subscribe::Serve,
                resume_drops: Arc::new(AtomicUsize::new(0)),
                subscribes: Arc::new(Mutex::new(Vec::new())),
                tab_lists: Arc::new(AtomicUsize::new(0)),
                streams_ended: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// A plain session that calls itself `session_id`.
        fn naming(session_id: &'static str) -> Fake {
            Fake {
                session_id,
                ..Fake::new(PutFile::Land)
            }
        }

        fn refusing_the_resume(mut self, code: &'static str) -> Fake {
            self.subscribe = Subscribe::Refuse(code);
            self
        }

        fn acking_the_resume(mut self, revision: u64) -> Fake {
            self.subscribe = Subscribe::Ack(revision);
            self
        }

        fn dropping_resumes(mut self, times: usize) -> Fake {
            self.resume_drops.store(times, Ordering::Release);
            self.subscribe = Subscribe::Close;
            self
        }

        fn snapshots(&self) -> usize {
            self.tab_lists.load(Ordering::Acquire)
        }

        fn identifies(&self) -> usize {
            self.identifies.load(Ordering::Acquire)
        }

        /// Subscribed connections whose peer is still on the other end.
        /// Only an event pump reads an event stream, and it holds the
        /// connection for as long as it lives — so zero is the proof that
        /// none was spawned on a stream this session answered.
        fn held_streams(&self) -> usize {
            self.subscribes()
                .len()
                .saturating_sub(self.streams_ended.load(Ordering::Acquire))
        }

        fn subscribes(&self) -> Vec<EventsSubscribeParams> {
            self.subscribes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn stalling(mut self, op: &'static str) -> Fake {
            self.stall = Some(op);
            self
        }

        /// Stall `op`, but only from the `skips + 1`th time it is sent.
        fn stalling_after(mut self, op: &'static str, skips: usize) -> Fake {
            self.stall = Some(op);
            self.stall_skips.store(skips, Ordering::Release);
            self
        }

        /// Refuse one op by name, however often it is sent.
        fn refusing(mut self, op: &'static str, code: &'static str) -> Fake {
            self.refuse_op = Some((op, code));
            self
        }

        /// Refuse `op`, but only from the `skips + 1`th time it is sent.
        fn refusing_after(self, op: &'static str, code: &'static str, skips: usize) -> Fake {
            let fake = self.refusing(op, code);
            fake.refuse_skips.store(skips, Ordering::Release);
            fake
        }

        /// Write one more `event.batch` onto whichever connection is
        /// subscribed, and answer with the revision it carries.
        fn emit_batch(&self) -> u64 {
            let revision = self.next_emit.load(Ordering::Acquire);
            self.emit.add_permits(1);
            revision
        }

        fn cutting(self, times: usize) -> Fake {
            self.cuts.store(times, Ordering::Release);
            self
        }

        fn serve(&self, socket: &Path) {
            let listener = tokio::net::UnixListener::bind(socket).expect("bind a fake session");
            let fake = self.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    fake.dials.fetch_add(1, Ordering::AcqRel);
                    tokio::spawn(fake.clone().connection(stream));
                }
            });
        }

        async fn connection(self, stream: tokio::net::UnixStream) {
            let (reader, mut writer) = stream.into_split();
            let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(reader));
            let mut subscribed = false;
            loop {
                let line = tokio::select! {
                    () = self.cut.requested(),
                        if subscribed && self.cuts.load(Ordering::Acquire) > 0 =>
                    {
                        self.cuts.fetch_sub(1, Ordering::AcqRel);
                        return;
                    }
                    // Edge-triggered: one permit is one frame, so a test
                    // can say "another batch, now" without the level
                    // signals above firing forever.
                    permit = self.emit.acquire(), if subscribed => {
                        permit.expect("the emit semaphore is never closed").forget();
                        let revision = self.next_emit.fetch_add(1, Ordering::AcqRel);
                        let mut frame = serde_json::to_vec(&serde_json::json!({
                            "revision": revision,
                            "events": [],
                        }))
                        .expect("encode a batch");
                        frame.push(b'\n');
                        if tokio::io::AsyncWriteExt::write_all(&mut writer, &frame)
                            .await
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                    line = lines.next_line() => match line {
                        Ok(Some(line)) => line,
                        _ => {
                            if subscribed {
                                self.streams_ended.fetch_add(1, Ordering::AcqRel);
                            }
                            return;
                        }
                    },
                };
                let request: serde_json::Value = serde_json::from_str(&line).expect("a request");
                let id = request["id"].clone();
                let op = request["op"].as_str().unwrap_or_default().to_string();
                if self.stall == Some(op.as_str()) && armed(&self.stall_skips) {
                    self.stalled.request();
                    self.release.requested().await;
                }
                let refused = self
                    .refuse_op
                    .filter(|(refused, _)| *refused == op.as_str() && armed(&self.refuse_skips))
                    .map(|(_, code)| {
                        serde_json::json!({
                            "id": id,
                            "ok": false,
                            "error": { "code": code, "message": "refused by the script" },
                        })
                    });
                let response = match op.as_str() {
                    _ if refused.is_some() => refused.expect("just checked"),
                    ops::SESSION_IDENTIFY => {
                        self.identifies.fetch_add(1, Ordering::AcqRel);
                        serde_json::json!({
                            "id": id,
                            "ok": true,
                            "result": identify_result(self.session_id),
                        })
                    }
                    ops::TAB_LIST => {
                        self.tab_lists.fetch_add(1, Ordering::AcqRel);
                        serde_json::json!({
                            "id": id,
                            "ok": true,
                            "result": seeded_list_named(self.session_id, Some(SESSION_REVISION)),
                        })
                    }
                    ops::EVENTS_SUBSCRIBE => {
                        let params: EventsSubscribeParams =
                            serde_json::from_value(request["params"].clone())
                                .expect("subscribe params");
                        let from_revision = params.from_revision;
                        self.subscribes
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(params);
                        match self.subscribe_answer(from_revision, &id) {
                            Some(answer) => {
                                subscribed = answer["ok"] == serde_json::json!(true);
                                answer
                            }
                            None => return,
                        }
                    }
                    ops::SESSION_SET_THEME => {
                        self.themes.fetch_add(1, Ordering::AcqRel);
                        serde_json::json!({"id": id, "ok": true, "result": {}})
                    }
                    ops::SESSION_PUT_FILE => match self.put_file(&request, &id).await {
                        Some(response) => response,
                        None => return,
                    },
                    _ => serde_json::json!({"id": id, "ok": true, "result": {}}),
                };
                let mut body = serde_json::to_vec(&response).expect("encode a response");
                body.push(b'\n');
                if tokio::io::AsyncWriteExt::write_all(&mut writer, &body)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        /// The scripted `events.subscribe` answer, or `None` for a
        /// connection this session hangs up on instead of answering.
        fn subscribe_answer(
            &self,
            from_revision: Option<u64>,
            id: &serde_json::Value,
        ) -> Option<serde_json::Value> {
            let ack = |revision: u64| {
                serde_json::json!({
                    "id": id,
                    "ok": true,
                    "result": {"revision": revision, "session_id": self.session_id},
                })
            };
            let Some(from) = from_revision else {
                return Some(ack(SESSION_REVISION));
            };
            match self.subscribe {
                Subscribe::Serve => Some(ack(from)),
                Subscribe::Ack(revision) => Some(ack(revision)),
                Subscribe::Refuse(code) => Some(serde_json::json!({
                    "id": id,
                    "ok": false,
                    "error": { "code": code, "message": "cannot replay from there" },
                })),
                Subscribe::Close if self.resume_drops.load(Ordering::Acquire) > 0 => {
                    self.resume_drops.fetch_sub(1, Ordering::AcqRel);
                    None
                }
                Subscribe::Close => Some(ack(from)),
            }
        }

        /// The scripted answer, or `None` for one this session keeps.
        async fn put_file(
            &self,
            request: &serde_json::Value,
            id: &serde_json::Value,
        ) -> Option<serde_json::Value> {
            let params: SessionPutFileParams =
                serde_json::from_value(request["params"].clone()).expect("put_file params");
            let nth = self.puts.fetch_add(1, Ordering::AcqRel);
            self.uploading.request();
            let result = match &self.put_file {
                PutFile::Land => SessionPutFileResult {
                    path: format!("{FILES_ROOT}/{}", params.name),
                    bytes: params.data.len() as u64,
                },
                PutFile::HoldFirst(held) if nth < *held => {
                    // Read and never answer. The client's own budget is
                    // the only thing that ends this.
                    std::future::pending::<()>().await;
                    unreachable!("pending never resolves")
                }
                PutFile::HoldFirst(_) => SessionPutFileResult {
                    path: format!("{FILES_ROOT}/{}", params.name),
                    bytes: params.data.len() as u64,
                },
                PutFile::Reply(reply) => reply.clone(),
                PutFile::Refuse(code) => {
                    return Some(serde_json::json!({
                        "id": id,
                        "ok": false,
                        "error": { "code": code, "message": "refused" },
                    }))
                }
            };
            Some(serde_json::json!({"id": id, "ok": true, "result": result}))
        }
    }

    /// Wait for a fake session's cue, or fail the test rather than hang.
    /// One budget went by and not two, measured in paused virtual time.
    /// The window absorbs the timer's millisecond granularity and the
    /// real time between arming the budget and pausing the clock; it is
    /// nowhere near wide enough to admit a second one.
    fn spent_one_budget(spent: Duration, budget: Duration, what: &str) {
        assert!(
            spent + Duration::from_secs(5) >= budget && spent <= budget + Duration::from_secs(1),
            "{what}: {spent:?} of {budget:?}"
        );
    }

    async fn cued(signal: &Shutdown, what: &str) {
        tokio::time::timeout(Duration::from_secs(10), signal.requested())
            .await
            .unwrap_or_else(|_| panic!("{what}"));
    }

    /// Every host state the task has published, drained as it goes,
    /// with the connect facts and applied revisions that rode behind
    /// them.
    #[derive(Default)]
    struct States(Vec<HostConnState>, Vec<ConnectFacts>, Vec<u64>);

    impl States {
        fn drain(&mut self, rx: &mut crate::engine_feed::EngineFeedReceiver) {
            for item in feed_items(rx) {
                match item {
                    EngineFeed::HostState(_, state) => self.0.push(state),
                    EngineFeed::HostConnectFacts(_, facts) => self.1.push(facts),
                    EngineFeed::HostWorkspace(_, HostWorkspaceEvent::Applied { revision, .. }) => {
                        self.2.push(revision)
                    }
                    _ => {}
                }
            }
        }

        fn connections(&self) -> usize {
            self.0.iter().filter(|state| state.is_connected()).count()
        }

        fn last(&self) -> &HostConnState {
            self.0.last().expect("a task publishes at least one state")
        }

        /// Poll the feed until the task has connected `nth` times.
        async fn until_connected(
            &mut self,
            rx: &mut crate::engine_feed::EngineFeedReceiver,
            nth: usize,
        ) {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    self.drain(rx);
                    if self.connections() >= nth {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the fake session connects");
        }
    }

    /// One connected task against `fake`, and the handles a test drives
    /// it with.
    struct Connected {
        ops: super::super::HostOps,
        shutdown: Arc<Shutdown>,
        task: tokio::task::JoinHandle<()>,
        feed: crate::engine_feed::EngineFeedReceiver,
        states: States,
    }

    impl Connected {
        /// The task, running, with nothing waited on. What a case needs
        /// when the interesting outcome is that `Connected` never
        /// arrives.
        fn spawn(socket: PathBuf, transport: HostTransport) -> Connected {
            Connected::spawn_with(socket, transport, None)
        }

        /// The same, handed a checkpoint its prologue may resume from.
        fn resuming(socket: PathBuf, transport: HostTransport, resume: Resume) -> Connected {
            Connected::spawn_with(socket, transport, Some(resume))
        }

        fn spawn_with(
            socket: PathBuf,
            transport: HostTransport,
            resume: Option<Resume>,
        ) -> Connected {
            let (ops, ops_rx) = super::super::HostOps::channel();
            let mut config = config(socket, transport, ConnectMode::Dial);
            config.uploads = ops.uploads();
            config.resume = resume;
            let (feed, rx) = crate::engine_feed::channel();
            let shutdown = Arc::new(Shutdown::default());
            let task = tokio::spawn(run(
                config,
                HostIdMinter::new(),
                ops_rx,
                feed,
                Arc::clone(&shutdown),
            ));
            Connected {
                ops,
                shutdown,
                task,
                feed: rx,
                states: States::default(),
            }
        }

        async fn start(socket: PathBuf, transport: HostTransport) -> Connected {
            let mut host = Connected::spawn(socket, transport);
            host.states.until_connected(&mut host.feed, 1).await;
            host
        }

        fn upload(
            &self,
            name: &str,
        ) -> oneshot::Receiver<Result<SessionPutFileResult, HostOpError>> {
            self.ops
                .uploads()
                .enqueue(name.into(), UploadSource::Bytes(b"png".to_vec()))
                .expect("a connected host admits an upload")
        }

        async fn stop(mut self) -> States {
            self.shutdown.request();
            tokio::time::timeout(Duration::from_secs(10), self.task)
                .await
                .expect("the task ends")
                .expect("and does not panic");
            self.states.drain(&mut self.feed);
            self.states
        }
    }

    /// Poll `states` until its last published state satisfies `want`.
    /// A task that has published nothing yet simply has not got there.
    async fn until_state(
        connected: &mut Connected,
        what: &str,
        want: impl Fn(&HostConnState) -> bool,
    ) {
        until_state_within(connected, Duration::from_secs(10), what, want).await
    }

    /// [`until_state`] on a stated budget, for a case whose wait is a
    /// production timeout rather than the suite's own patience — the
    /// default is [`leg`] exactly, so such a case would be racing it.
    async fn until_state_within(
        connected: &mut Connected,
        budget: Duration,
        what: &str,
        want: impl Fn(&HostConnState) -> bool,
    ) {
        tokio::time::timeout(budget, async {
            loop {
                connected.states.drain(&mut connected.feed);
                if connected.states.0.last().is_some_and(&want) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}; states were {:?}", connected.states.0));
    }

    // ---- resuming a stream (plan 056 §3.2) -----------------------------

    /// A checkpoint on a mirror of its own, as the set seeds one.
    fn checkpoint(session_id: &str, revision: u64) -> Resume {
        Resume {
            session_id: session_id.into(),
            mirror: Arc::new(seeded_mirror(revision)),
        }
    }

    /// The prologue's own verdict, off the facts it published.
    fn resumed(states: &States) -> Option<ResumeFacts> {
        states
            .1
            .last()
            .expect("a connected task publishes its facts")
            .resumed
    }

    fn reason(state: &HostConnState) -> &str {
        match state {
            HostConnState::Disconnected(disconnected) => &disconnected.reason,
            other => panic!("expected a dropped connection, got {other:?}"),
        }
    }

    /// **R11's headline.** A reconnect to the session the checkpoint
    /// names offers the fence it is at, and the session replays from
    /// there — so the whole-workspace `tab.list` every reconnect used to
    /// cost is simply not sent.
    #[tokio::test]
    async fn a_matching_checkpoint_resumes_and_takes_no_snapshot() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("resume.sock");
        let fake = Fake::new(PutFile::Land);
        fake.serve(&socket);

        let mut host =
            Connected::resuming(socket, HostTransport::UnixSocket, checkpoint(SESSION_ID, 7));
        host.states.until_connected(&mut host.feed, 1).await;

        assert_eq!(
            fake.snapshots(),
            0,
            "a resumed reconnect must not re-list the workspace"
        );
        let subscribes = fake.subscribes();
        assert_eq!(subscribes.len(), 1, "one subscribe, and it was the resume");
        assert_eq!(subscribes[0].from_revision, Some(7));
        assert_eq!(subscribes[0].session_id.as_deref(), Some(SESSION_ID));

        let states = host.stop().await;
        assert_eq!(
            resumed(&states),
            Some(ResumeFacts { from_revision: 7 }),
            "and `host.status` reports the ack's fence, not the request's"
        );
    }

    /// The three names a session has for "I cannot replay from there".
    /// None of them fails the connection: each falls back to the
    /// subscribe-then-snapshot prologue on a fresh dial, and the host
    /// comes up as it always did.
    #[tokio::test]
    async fn every_refusal_falls_back_to_a_fresh_snapshot() {
        for code in ["replay-expired", "revision-ahead", "session-mismatch"] {
            let dir = tempfile::tempdir().expect("temp dir");
            let socket = dir.path().join("refused.sock");
            let fake = Fake::new(PutFile::Land).refusing_the_resume(code);
            fake.serve(&socket);

            let mut host =
                Connected::resuming(socket, HostTransport::UnixSocket, checkpoint(SESSION_ID, 7));
            host.states.until_connected(&mut host.feed, 1).await;

            assert_eq!(fake.snapshots(), 1, "{code}: exactly one fallback snapshot");
            let subscribes = fake.subscribes();
            assert_eq!(
                subscribes
                    .iter()
                    .map(|params| params.from_revision)
                    .collect::<Vec<_>>(),
                vec![Some(7), None],
                "{code}: the refused offer, then a fresh subscribe"
            );

            let states = host.stop().await;
            assert_eq!(resumed(&states), None, "{code}: nothing was replayed");
            assert_eq!(states.connections(), 1, "{code}: and it is a live host");
        }
    }

    /// The ack echoes the fence by contract. One that does not, with no
    /// snapshot behind it, would leave the mirror silently stale — every
    /// commit between the two discarded as already applied — so the
    /// attempt fails instead of trusting it.
    #[tokio::test]
    async fn an_ack_that_does_not_echo_the_fence_fails_the_attempt() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("bad-ack.sock");
        let fake = Fake::new(PutFile::Land).acking_the_resume(9);
        fake.serve(&socket);

        let mut host =
            Connected::resuming(socket, HostTransport::UnixSocket, checkpoint(SESSION_ID, 7));
        until_state(&mut host, "the attempt to fail", |state| {
            matches!(state, HostConnState::Disconnected(_))
        })
        .await;

        assert_eq!(
            reason(host.states.last()),
            "resume ack 9 does not match from_revision 7"
        );
        assert_eq!(fake.snapshots(), 0, "and it never fell back");
    }

    /// A resume dial that dies on the wire is the wire, not a refusal:
    /// no snapshot is attempted, and the checkpoint is **kept** — the
    /// next rung of the ladder presents the very same fence.
    #[tokio::test]
    async fn a_wire_failure_on_the_resume_keeps_the_checkpoint() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("resume-wire.sock");
        let fake = Fake::new(PutFile::Land).dropping_resumes(1);
        fake.serve(&socket);

        // Localhost, because its ladder is the one that retries inside
        // this task.
        let mut host = Connected::resuming(
            socket,
            HostTransport::LocalSession,
            checkpoint(SESSION_ID, 7),
        );
        until_state(&mut host, "the resume dial to die", |state| {
            matches!(state, HostConnState::Disconnected(_))
        })
        .await;
        assert_eq!(
            fake.snapshots(),
            0,
            "a wire failure is not a refusal: nothing may fall back to a snapshot"
        );

        host.states.until_connected(&mut host.feed, 1).await;
        assert_eq!(
            fake.subscribes()
                .iter()
                .map(|params| params.from_revision)
                .collect::<Vec<_>>(),
            vec![Some(7), Some(7)],
            "the checkpoint survives the failure and is offered again"
        );
        assert_eq!(fake.snapshots(), 0, "and the retry resumed too");

        let states = host.stop().await;
        assert_eq!(resumed(&states), Some(ResumeFacts { from_revision: 7 }));
    }

    /// A session that restarted is a different session, and this client
    /// already knows it — `session.identify` ran first. So the fence is
    /// never offered: a `session-mismatch` round trip is spent for
    /// nothing, and over ssh that round trip is an exec.
    #[tokio::test]
    async fn a_restarted_session_is_caught_locally_and_never_offered_a_fence() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("restarted.sock");
        let fake = Fake::new(PutFile::Land);
        fake.serve(&socket);

        let mut host = Connected::resuming(
            socket,
            HostTransport::UnixSocket,
            checkpoint("a-session-that-has-since-restarted", 7),
        );
        host.states.until_connected(&mut host.feed, 1).await;

        let subscribes = fake.subscribes();
        assert_eq!(subscribes.len(), 1, "no offer was made and none refused");
        assert_eq!(subscribes[0].from_revision, None);
        assert_eq!(fake.snapshots(), 1);
        assert_eq!(resumed(&host.stop().await), None);
    }

    // ---- one subscribe, one incarnation (#458) --------------------------

    /// A session that answers the control leg and is then replaced, with
    /// its accepted connection still serving. The recipe for "the socket
    /// moved between the dials": A identified, B answers the subscribe.
    async fn identified_then_replaced(
        socket: &Path,
        a: &Fake,
        b: &Fake,
    ) -> (IpcClient, ConnectFacts) {
        a.serve(socket);
        let config = config(
            socket.to_path_buf(),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
        );
        let identified = open_control(&config, ConnectMode::Dial)
            .await
            .expect("the control leg identifies A");
        std::fs::remove_file(socket).expect("unlink the socket A is bound to");
        b.serve(socket);
        identified
    }

    /// **#458.** A subscribe has two legs, and nothing but this check
    /// says they reached one session: the ack names B, `session.identify`
    /// named A, and the alternative is a mirror of A's rows fenced
    /// against B's revisions.
    #[tokio::test]
    async fn a_subscribe_that_reaches_another_incarnation_fails_before_it_snapshots() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("two-legs.sock");
        let fake_a = Fake::new(PutFile::Land);
        let fake_b = Fake::naming("s2");
        let (mut control, facts) = identified_then_replaced(&socket, &fake_a, &fake_b).await;
        assert_eq!(facts.session_id, SESSION_ID);

        let plan = SubscribePlan {
            resume: None,
            session_id: &facts.session_id,
        };
        let error = match subscribe(&socket, &mut control, plan).await {
            Err(error) => error,
            Ok((_events, _pump, Subscribed::Fresh(mirror))) => panic!(
                "expected a refusal; B answered the subscribe and the mirror holds {:?} \
                 off {} snapshot(s) of A",
                mirror
                    .projects
                    .iter()
                    .map(|project| project.name.as_str())
                    .collect::<Vec<_>>(),
                fake_a.snapshots()
            ),
            Ok((_events, _pump, Subscribed::Resumed { ack })) => {
                panic!("expected a refusal; nothing was offered, yet a resume acked {ack}")
            }
        };

        assert_eq!(
            transport(&error),
            "session s2 answered the subscribe; this attempt identified session s1"
        );
        assert_eq!(fake_a.identifies(), 1, "the control leg identified A");
        assert_eq!(fake_b.identifies(), 0, "and nothing identified B");
        assert_eq!(fake_b.subscribes().len(), 1, "the event leg reached B");
        assert_eq!(
            fake_a.snapshots(),
            0,
            "and A was never listed onto B's fence"
        );
    }

    /// The resume leg is a dial too, so it can land on the same mismatch.
    /// B echoes the fence the checkpoint names — only the incarnation
    /// tells the two apart — and the refusal lands *before* the pump: B's
    /// stream is dropped unread, where a pump would have started folding
    /// B's events onto A's mirror.
    #[tokio::test]
    async fn a_resume_ack_naming_another_incarnation_is_refused_before_the_pump_starts() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("two-legs-resume.sock");
        let fake_a = Fake::new(PutFile::Land);
        let fake_b = Fake::naming("s2");
        let (mut control, facts) = identified_then_replaced(&socket, &fake_a, &fake_b).await;
        assert_eq!(facts.session_id, SESSION_ID);

        let resume = checkpoint(SESSION_ID, 7);
        let plan = SubscribePlan::resuming(&resume);
        let error = match subscribe(&socket, &mut control, plan).await {
            Err(error) => error,
            Ok(_) => panic!("expected a refusal; B answered the resume this attempt offered A"),
        };

        assert_eq!(
            transport(&error),
            "session s2 answered the subscribe; this attempt identified session s1"
        );
        assert_eq!(
            fake_b.subscribes()[0].from_revision,
            Some(7),
            "the fence was offered to B, which echoed it"
        );
        assert_eq!(
            fake_a.snapshots(),
            0,
            "and a mismatched ack fails the attempt rather than falling back"
        );

        tokio::time::timeout(Duration::from_secs(10), async {
            while fake_b.held_streams() > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("B's stream is dropped, not pumped: a pump would hold it open");
    }

    /// A localhost drop retries inside the same task, so it never goes
    /// back to the set for a checkpoint — it keeps its own, taken from
    /// the connection that just ended. Same fence, one fewer trip.
    #[tokio::test]
    async fn an_in_task_retry_resumes_from_the_connection_that_just_ended() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("in-task-retry.sock");
        let fake = Fake::new(PutFile::Land).cutting(1);
        fake.serve(&socket);

        // No seeded checkpoint: this host has never connected before.
        let mut host = Connected::start(socket, HostTransport::LocalSession).await;
        assert_eq!(fake.snapshots(), 1, "the first connection is a fresh one");

        fake.cut.request();
        until_state(&mut host, "the stream to drop", |state| {
            matches!(state, HostConnState::Disconnected(_))
        })
        .await;
        host.states.until_connected(&mut host.feed, 2).await;

        assert_eq!(
            fake.subscribes()
                .iter()
                .map(|params| params.from_revision)
                .collect::<Vec<_>>(),
            vec![None, Some(SESSION_REVISION)],
            "the retry offers what the ended connection had applied"
        );
        assert_eq!(fake.snapshots(), 1, "and takes no second snapshot");
        assert_eq!(
            resumed(&host.stop().await),
            Some(ResumeFacts {
                from_revision: SESSION_REVISION
            })
        );
    }

    /// **The W3 criterion.** An upload is admitted and completes while
    /// the control leg is stalled on an op the session never answers —
    /// and the control leg is exactly where it was, so releasing it
    /// answers, and the next op round-trips.
    ///
    /// This is the whole reason uploads are not intents: the connection
    /// loop awaits each control call inline, so an upload queued behind
    /// one could not even be *received*, let alone run.
    #[tokio::test]
    async fn an_upload_runs_while_the_control_leg_is_stalled() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("stalled.sock");
        let fake = Fake::new(PutFile::Land).stalling(ops::TAB_OPEN);
        fake.serve(&socket);
        let host = Connected::start(socket, HostTransport::UnixSocket).await;

        // Wedge the control leg first, so the upload has to be admitted
        // and run past a loop that is mid-`await`.
        let stalled = tokio::spawn(host.ops.call(ops::TAB_OPEN, serde_json::json!({})));
        cued(&fake.stalled, "the session read the control op").await;

        let landed = tokio::time::timeout(
            Duration::from_secs(10),
            host.ops
                .put_file("shot.png".into(), UploadSource::Bytes(b"png".to_vec())),
        )
        .await
        .expect("an upload must not wait on the control leg")
        .expect("and the fake session lands it");
        assert_eq!(landed.path, format!("{FILES_ROOT}/shot.png"));
        assert_eq!(landed.bytes, 3);

        // Nothing about the control leg moved: it is still waiting for
        // its answer, and it takes one.
        fake.release.request();
        assert!(tokio::time::timeout(Duration::from_secs(10), stalled)
            .await
            .expect("the released op answers")
            .expect("and its task does not panic")
            .is_ok());
        assert!(tokio::time::timeout(
            Duration::from_secs(10),
            host.ops.call(ops::SESSION_SET_FOCUS, serde_json::json!({}))
        )
        .await
        .expect("and the queue keeps draining afterwards")
        .is_ok());

        let states = host.stop().await;
        assert_eq!(states.connections(), 1, "one incarnation throughout");
    }

    /// Cancelling an attempt mid-snapshot retires its pump.
    ///
    /// The precondition below is what stops this test from passing by
    /// never entering the window: the fake pushes onto `subscribes`
    /// before writing the subscribe ack, and `tab.list` is only sent
    /// after that ack lands, so a read stall on `tab.list` implies
    /// exactly one subscribe with its pump already holding the
    /// stream — `streams_ended` only moves at EOF.
    #[tokio::test]
    async fn cancelling_an_attempt_mid_snapshot_retires_its_pump() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("cancel-mid-snapshot.sock");
        let fake = Fake::new(PutFile::Land).stalling(ops::TAB_LIST);
        fake.serve(&socket);

        // An assertion failure (or the negative control) must not leave
        // the fake's control task wedged on `release.requested()`.
        struct ReleaseOnDrop(Arc<Shutdown>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.0.request();
            }
        }
        let _release = ReleaseOnDrop(Arc::clone(&fake.release));

        // Not `Connected::start`: that waits for a `Connected` state
        // this attempt never reaches while `tab.list` is stalled.
        let host = Connected::spawn(socket, HostTransport::UnixSocket);
        cued(&fake.stalled, "the session read tab.list").await;

        assert_eq!(
            fake.subscribes().len(),
            1,
            "the pump's subscribe reached the session before the stall"
        );
        assert_eq!(fake.held_streams(), 1, "and the pump is holding its stream");

        host.stop().await;

        tokio::time::timeout(Duration::from_secs(10), async {
            while fake.held_streams() > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelling the attempt retires its pump, releasing the stream");
    }

    /// An upload the session never answers spends its own budget and
    /// nothing else's: the host is still `Connected` afterwards.
    ///
    /// The clock is paused once the frame has reached the session, so
    /// the ~30 s budget is spent in virtual time — and the elapsed
    /// virtual time is asserted, which is what tells this budget from
    /// the 10 s control leg's.
    #[tokio::test]
    async fn a_never_answered_upload_times_out_without_touching_the_host() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("held.sock");
        let fake = Fake::new(PutFile::HoldFirst(usize::MAX));
        fake.serve(&socket);
        let mut host = Connected::start(socket, HostTransport::UnixSocket).await;

        let waiting = host.upload("shot.png");
        cued(&fake.uploading, "the session read the upload").await;

        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let answered = waiting.await.expect("answered, not dropped");
        let spent = started.elapsed();

        let budget = super::super::upload::budget(3);
        assert!(
            matches!(&answered, Err(HostOpError::Transport(reason))
                if reason.contains(ops::SESSION_PUT_FILE) && reason.contains("timed out")),
            "{answered:?}"
        );
        spent_one_budget(
            spent,
            budget,
            "the upload's own budget, not the control leg's",
        );

        host.states.drain(&mut host.feed);
        assert!(
            host.states.last().is_connected(),
            "a timed-out upload is not a dropped host: {:?}",
            host.states.last()
        );
    }

    /// An upload in flight when the connection drops is **answered** —
    /// `Ok(Err(Disconnected))` on the raw receiver, not a sender dropped
    /// with the subtask — and the lane is closed behind it.
    #[tokio::test]
    async fn an_upload_in_flight_when_the_connection_drops_is_answered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("cut.sock");
        // Unix-socket + dial: a dropped connection ends the task instead
        // of retrying, so the lane's own cancellation is the only thing
        // that can answer this upload.
        let fake = Fake::new(PutFile::HoldFirst(usize::MAX)).cutting(1);
        fake.serve(&socket);
        let host = Connected::start(socket, HostTransport::UnixSocket).await;

        let waiting = host.upload("shot.png");
        cued(&fake.uploading, "the session read the upload").await;
        fake.cut.request();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), waiting)
                .await
                .expect("an upload must not outlive its incarnation unanswered"),
            Ok(Err(HostOpError::Disconnected)),
            "answered, rather than dropped with its reply channel"
        );
        assert_eq!(
            host.ops
                .uploads()
                .enqueue("late.png".into(), UploadSource::Bytes(vec![1]))
                .err(),
            Some(HostOpError::Disconnected),
            "and the lane closed with the incarnation"
        );
        host.stop().await;
    }

    /// The same, through a **reconnect**: the old incarnation's upload is
    /// answered as its lane closes, and the new incarnation opens one of
    /// its own that works.
    ///
    /// An explicit reconnect is the same shape one rung up — the app
    /// drops the `HostConn`, which signals the task below.
    #[tokio::test]
    async fn a_reconnect_answers_the_old_upload_and_opens_a_new_lane() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("reconnect.sock");
        // Localhost, so the drop is retried by this same task and the
        // second incarnation is what serves the upload below.
        let fake = Fake::new(PutFile::HoldFirst(1)).cutting(1);
        fake.serve(&socket);
        let mut host = Connected::start(socket, HostTransport::LocalSession).await;

        let waiting = host.upload("first.png");
        cued(&fake.uploading, "the session read the upload").await;
        fake.cut.request();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), waiting)
                .await
                .expect("the old incarnation answers before it goes"),
            Ok(Err(HostOpError::Disconnected))
        );

        host.states.until_connected(&mut host.feed, 2).await;
        let landed = tokio::time::timeout(
            Duration::from_secs(10),
            host.ops
                .put_file("second.png".into(), UploadSource::Bytes(b"png".to_vec())),
        )
        .await
        .expect("the new incarnation has a lane of its own")
        .expect("and it works");
        assert_eq!(landed.path, format!("{FILES_ROOT}/second.png"));

        let states = host.stop().await;
        assert_eq!(states.connections(), 2, "two incarnations, two lanes");
    }

    /// `HostConn::drop` signals the task's shutdown — quitting, removing
    /// the host, or an explicit reconnect replacing the connection. An
    /// upload in flight when that happens is answered too.
    #[tokio::test]
    async fn an_upload_in_flight_when_the_host_is_dropped_is_answered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("dropped.sock");
        let fake = Fake::new(PutFile::HoldFirst(usize::MAX));
        fake.serve(&socket);
        let host = Connected::start(socket, HostTransport::UnixSocket).await;

        let waiting = host.upload("shot.png");
        cued(&fake.uploading, "the session read the upload").await;
        // Exactly what `HostConn::drop` does.
        host.shutdown.request();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), waiting)
                .await
                .expect("a detached upload must not outlive the app"),
            Ok(Err(HostOpError::Disconnected))
        );
        host.stop().await;
    }

    /// The lane's guard is what carries the one exit that runs no code
    /// in the connection task — [`run`]'s grace timer dropping the whole
    /// loop's future. Both halves are answered: the uploads in flight
    /// and the ones still queued behind them.
    #[tokio::test]
    async fn dropping_the_lane_answers_what_is_running_and_what_is_queued() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("lane.sock");
        let fake = Fake::new(PutFile::HoldFirst(usize::MAX));
        fake.serve(&socket);

        let uploads = Uploads::default();
        let lane = uploads.open(socket);
        // Two more than run at once, so half of these are still on the
        // channel when the lane closes.
        let waiting: Vec<_> = (0..4)
            .map(|nth| {
                uploads
                    .enqueue(format!("{nth}.png"), UploadSource::Bytes(b"png".to_vec()))
                    .expect("the lane is open")
            })
            .collect();
        cued(&fake.uploading, "the session read an upload").await;

        drop(lane);

        for reply in waiting {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(10), reply)
                    .await
                    .expect("every upload is answered, running or queued"),
                Ok(Err(HostOpError::Disconnected))
            );
        }
    }

    /// The reply re-check is really on the path, not just unit-tested:
    /// a session that answers with somebody else's file name is refused
    /// rather than pasted.
    #[tokio::test]
    async fn a_hostile_reply_is_refused_on_the_wire() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("hostile.sock");
        let fake = Fake::new(PutFile::Reply(SessionPutFileResult {
            path: format!("{FILES_ROOT}/other.png"),
            bytes: 3,
        }));
        fake.serve(&socket);
        let mut host = Connected::start(socket, HostTransport::UnixSocket).await;

        let refused = tokio::time::timeout(Duration::from_secs(10), host.upload("shot.png"))
            .await
            .expect("the reply comes back")
            .expect("answered");
        let Err(HostOpError::Local(message)) = refused else {
            panic!("a reply the client cannot use is its own refusal, got {refused:?}");
        };
        assert!(message.contains("a different file name"), "{message}");

        host.states.drain(&mut host.feed);
        assert!(host.states.last().is_connected());
        host.stop().await;
    }

    /// A file that grew past the cap since it was inspected is refused
    /// **before anything is dialed** — the read is capped at one byte
    /// past the limit, and the session never hears about it.
    #[tokio::test]
    async fn a_file_over_the_cap_is_refused_before_any_dial() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("never-dialed.sock");
        let fake = Fake::new(PutFile::Land);
        fake.serve(&socket);

        let grown = dir.path().join("grown.png");
        std::fs::write(
            &grown,
            vec![0u8; roost_ipc::messages::MAX_PUT_FILE_BYTES as usize + 1],
        )
        .expect("write an over-cap file");

        let uploads = Uploads::default();
        let _lane = uploads.open(socket);
        let refused = uploads
            .enqueue("grown.png".into(), UploadSource::Path(grown))
            .expect("the lane admits it; the read is what refuses");
        let refused = tokio::time::timeout(Duration::from_secs(10), refused)
            .await
            .expect("the refusal comes back")
            .expect("answered");
        assert!(
            matches!(
                &refused,
                Err(HostOpError::Rejected {
                    code: ServerCode::TooLarge,
                    ..
                })
            ),
            "{refused:?}"
        );
        assert_eq!(
            fake.dials.load(Ordering::Acquire),
            0,
            "an over-cap file never reaches the wire"
        );
    }
}
