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
//! gate → `session.connect {takeover: true}` → `session.set_theme` →
//! subscribe → `tab.list`. The theme lands **before any `tab.attach`**
//! (plan 037 §3.6) and the snapshot is taken **after** the subscribe so
//! the ack's revision is a floor the snapshot can be fenced against.
//!
//! ## The seam C5 fills
//!
//! Attaching a tab is a *fourth* connection per attached tab —
//! `tab.attach` (an intent on this task's queue, which is why token
//! minting rides the same queue) followed by
//! [`roost_ipc::client::DataConnection`]. C4 deliberately builds none of
//! it: the decoder is main-thread-only, so the data path's shape is
//! C5's to choose. What C4 guarantees it is a live control client with
//! the lease, an ordered queue to mint tokens on, and a mirror that
//! already knows which tabs exist.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use roost_ipc::client::{ClientError, EventFrame, EventStream, IpcClient, ServerCode};
use roost_ipc::messages::{
    ops, EventBatch, OscColorsParams, SessionConnectParams, SessionConnectResult, SessionIdentify,
    SessionIdentifyParams, SessionSetThemeParams, TabListResult,
};
use roost_ipc::session_launch;
use roost_ui_model::keys::HostId;
use tokio::sync::{mpsc, Notify};

use super::mirror::{HostMirror, SharedMirror};
use super::queue::{self, HostIntent, HostOpError, OpFault};
use super::state::{check_compatibility, HostConnState, HostStateMachine, HostTransport};
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
    async fn requested(&self) {
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
    /// The lease the previous connection held, when this task is one the
    /// reconnect schedule asked for (plan 040 §3.7). It seeds
    /// [`connect_loop`]'s own `held_lease`, which [`attempt`] presents.
    /// `None` for every explicit Connect.
    pub(crate) held_lease: Option<String>,
    /// This session's pinned libghostty identity, compared exactly.
    pub(crate) client_build: String,
    /// The client's terminal palette, re-read on every (re)connect so a
    /// theme changed while disconnected is the one the session gets.
    pub(crate) theme: Arc<Mutex<OscColorsParams>>,
}

/// The scale every budget in this module is stretched by, read once.
///
/// [`session_launch::timeout_scale`] reads the environment on each call,
/// and [`leg`] is per-op work on the control plane — the answer cannot
/// change while the process runs, so it is worth remembering.
fn scale() -> f64 {
    static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *SCALE.get_or_init(session_launch::timeout_scale)
}

/// A single control-plane leg's budget.
pub(crate) fn leg() -> Duration {
    session_launch::IPC_TIMEOUT.mul_f64(scale())
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
    /// Transport, refusal, or spawn failure. Retryable where policy
    /// allows.
    Transport(String),
}

impl From<ClientError> for AttemptError {
    fn from(error: ClientError) -> Self {
        match error.server_code() {
            Some(ServerCode::ShuttingDown) => AttemptError::Stopping("stop".into()),
            Some(ServerCode::TakenOver) => AttemptError::Stopping("taken-over".into()),
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
        }
    }
}

/// A live, subscribed connection.
struct Live {
    control: IpcClient,
    lease: String,
    events: EventRx,
    pump: tokio::task::AbortHandle,
    /// Shared with the UI: written here, read there. Never copied onto
    /// the feed.
    mirror: Arc<SharedMirror>,
    /// This session answered `unknown-op` to `session.set_focus`, and
    /// has been told about once. HS-2 sessions predate the op: the
    /// client keeps sending it (the UI has no other way to know, and the
    /// refusal costs one round trip), but one line per connection is the
    /// whole story — a line per selection change is noise.
    focus_unsupported: bool,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.pump.abort();
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
    // The lease this task last held — or, seeded from the config, the
    // one the connection this task replaces held. `Some` means an
    // attempt nobody asked for, and such an attempt has to ask before
    // it takes the session back — see [`attempt`].
    let mut held_lease: Option<String> = config.held_lease.clone();

    loop {
        // Mint (and therefore register the ownership) before anything is
        // published under the id: the drain side resolves the owner off
        // that registration, so a `Connecting` must never land first.
        let incarnation = minter.mint(&config.host, config.generation);
        if !publish_state(feed, incarnation, machine.begin_attempt(previous)) {
            return;
        }
        previous = Some(incarnation);

        let dialed = tokio::select! {
            biased;
            () = shutdown.requested() => None,
            outcome = attempt(config, mode, &mut held_lease) => Some(outcome),
        };
        // Only the first attempt may spawn or probe; a retry dials.
        mode = ConnectMode::Dial;

        let ended = match dialed {
            None => ConnEnd::Shutdown,
            Some(Err(error)) => error.into(),
            Some(Ok(live)) => {
                // Published as well as kept — [`attempt`] has already
                // recorded it as ours. This task dies with the link, and
                // the app side is the only place a lease can outlive the
                // connection that minted it (plan 040 §3.7).
                if !publish_lease(feed, incarnation, live.lease.clone())
                    || !publish_workspace(
                        feed,
                        incarnation,
                        HostWorkspaceEvent::Reset(Arc::clone(&live.mirror)),
                    )
                    || !publish_state(feed, incarnation, machine.connected())
                {
                    ConnEnd::FeedClosed
                } else {
                    serve(config, incarnation, live, ops_rx, feed, shutdown).await
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
                publish_state(feed, incarnation, machine.stopping(&reason));
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

/// One attempt, with the auto-retry's guard in front of it.
///
/// `held_lease` is `Some` only on a retry this task scheduled itself,
/// and that is the whole distinction. Reconnecting is a takeover by
/// construction (`ipc.md` #sessionconnect), so an auto-retry that
/// simply reconnected would silently steal the session back whenever
/// the drop *was* a takeover whose best-effort `session.stopping`
/// envelope never made it — takeover ping-pong between two clients,
/// neither of which the user asked for. Presenting the old lease first
/// settles it: `taken-over` is the session saying somebody else drives
/// it now, and that is terminal.
///
/// A lease also arrives a second way: [`ConnectionConfig::held_lease`],
/// which the app sets when this whole *task* is the retry (plan 040
/// §3.7 — an ssh drop tears the tunnel down, so its ladder re-enters at
/// `open_ssh` and every attempt is a fresh task). A task seeded that way
/// runs this guard on its **first** attempt, which is the same probe a
/// localhost auto-retry runs on its second, for the same reason.
///
/// An explicit Connect never comes through here with a lease. Taking
/// the session back on purpose is exactly what that button means.
///
/// `held_lease` is also where this attempt's *own* lease is written
/// back, which is why it is `&mut` — see [`connect`].
async fn attempt(
    config: &ConnectionConfig,
    mode: ConnectMode,
    held_lease: &mut Option<String>,
) -> Result<Live, AttemptError> {
    if let Some(lease) = held_lease.as_deref() {
        if lease_was_taken(&config.socket, lease).await {
            return Err(AttemptError::Stopping("taken-over".into()));
        }
    }
    connect(config, mode, held_lease).await
}

/// Present the old lease and see whether the session still recognizes
/// it as ours.
async fn lease_was_taken(socket: &Path, lease: &str) -> bool {
    // `events.subscribe` is the cheapest leased op there is: no
    // workspace effect, and the stream it would open is dropped on the
    // next line. Any answer at all is enough — this asks about the
    // lease, not about the stream.
    let probed = tokio::time::timeout(leg(), EventStream::connect(socket, lease)).await;
    match probed {
        Ok(outcome) => proves_takeover(outcome.as_ref().err()),
        // A probe that could not finish proves nothing; see below.
        Err(_elapsed) => false,
    }
}

/// Does this probe outcome prove the session was taken from us?
///
/// Only `taken-over` does, and only because the session keeps exactly
/// one tombstone for the most recently displaced lease. Everything else
/// proceeds to the ordinary takeover reconnect:
///
/// * `connect-required` — the tombstone was forgotten (a second
///   takeover displaced it) or the session restarted. Nothing here says
///   somebody else is driving.
/// * a success — the lease is still ours, so the drop was the wire, not
///   a takeover.
/// * a transport failure or a timeout — proves nothing at all, and
///   refusing to reconnect on a failed probe would strand a client
///   whose session is perfectly fine.
fn proves_takeover(error: Option<&ClientError>) -> bool {
    matches!(
        error.and_then(ClientError::server_code),
        Some(ServerCode::TakenOver)
    )
}

/// One connect attempt: the wire prologue, in the order `ipc.md` fixes.
///
/// `held_lease` is an out-parameter as much as an in-one: step 2 is
/// where ownership actually moves, so that is where the caller starts
/// holding the new lease — see the comment there.
async fn connect(
    config: &ConnectionConfig,
    mode: ConnectMode,
    held_lease: &mut Option<String>,
) -> Result<Live, AttemptError> {
    ensure_socket(config, mode).await?;

    // Bounded like every other leg. A peer that accepts the connection
    // and then says nothing would otherwise wedge this host in
    // `Connecting` for as long as the process runs.
    let mut control = tokio::time::timeout(leg(), IpcClient::connect(&config.socket))
        .await
        .map_err(|_| {
            AttemptError::Transport(format!("dialing {} timed out", config.socket.display()))
        })?
        .map_err(|error| AttemptError::Transport(error.to_string()))?;

    // 1. Identify, and gate on it. Nothing binary exists yet, so every
    //    incompatibility is caught on stable JSON.
    let raw = call(
        &mut control,
        ops::SESSION_IDENTIFY,
        serde_json::json!(SessionIdentifyParams {}),
    )
    .await?;
    let identity: SessionIdentify =
        serde_json::from_value(raw).map_err(|error| undecodable(ops::SESSION_IDENTIFY, &error))?;
    check_compatibility(
        &identity,
        &config.client_build,
        config.transport.restart_action(),
    )
    .map_err(|mismatch| AttemptError::Incompatible(Box::new(mismatch)))?;

    // 2. Claim the lease. Reconnect IS takeover — the lease outlives the
    //    connection it was minted on, so a client that reconnects has to
    //    take its own session back deliberately (`ipc.md`
    //    #sessionconnect).
    let raw = call(
        &mut control,
        ops::SESSION_CONNECT,
        serde_json::json!(SessionConnectParams { takeover: true }),
    )
    .await?;
    let connected: SessionConnectResult =
        serde_json::from_value(raw).map_err(|error| undecodable(ops::SESSION_CONNECT, &error))?;
    let lease = connected.lease;
    // Ownership moved on the wire in the call above, which is why the
    // write is here and not on the success path: it tombstoned whatever
    // lease we held before, so from this line on *this* is the one we
    // hold. A step below that fails must not leave the caller presenting
    // a tombstone to its next attempt — the far side would answer
    // `taken-over` and the host would settle as somebody else's, with
    // nobody else ever involved (plan 040 §3.7).
    *held_lease = Some(lease.clone());

    // 3. Seed the session's palette before anything is attached, so a
    //    query answered while hydrating already carries our colors.
    let theme = config
        .theme
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    call(
        &mut control,
        ops::SESSION_SET_THEME,
        serde_json::json!(SessionSetThemeParams {
            lease: lease.clone(),
            osc_colors: theme,
        }),
    )
    .await?;

    // 4. Subscribe, then snapshot. That order is what makes the fence
    //    sound: the ack names a commit the snapshot is guaranteed to be
    //    at or past.
    let (events, pump, mirror) =
        subscribe_and_snapshot(&config.socket, &lease, &mut control).await?;

    tracing::info!(
        host = %config.host,
        label = %config.label,
        session = %identity.session_id,
        revision = mirror.revision,
        "connected to host session"
    );
    Ok(Live {
        control,
        lease,
        events,
        pump,
        mirror: Arc::new(SharedMirror::new(mirror)),
        focus_unsupported: false,
    })
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
    Err(AttemptError::Transport(format!(
        "no session is running at {}",
        config.socket.display()
    )))
}

/// Nothing is listening, and the user asked for a connection: climb the
/// shared launch ladder (`roost_ipc::session_launch`, the same rungs
/// `roostctl session start` uses).
async fn spawn_session(config: &ConnectionConfig) -> Result<(), AttemptError> {
    if !config.transport.is_localhost() {
        return Err(AttemptError::Transport(format!(
            "no session is running at {} and only a localhost session can be started from here",
            config.socket.display()
        )));
    }
    let scale = scale();
    let bin = session_launch::locate_session_binary(
        std::env::var_os(session_launch::BIN_ENV).as_deref(),
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
    .map_err(|error| AttemptError::Transport(format!("{error:#}")))?;
    // The launch cwd seeds the session's first project on a fresh state
    // file only; a UI has no better answer than its own.
    let cwd = std::env::current_dir()
        .map_err(|error| AttemptError::Transport(format!("read the working directory: {error}")))?;

    let verdict = session_launch::spawn_and_read_verdict(
        &bin.path,
        &cwd,
        SPAWN_VERDICT_BUDGET.mul_f64(scale),
    )
    .await
    .map_err(|error| AttemptError::Transport(format!("{error:#}")))?;
    if let session_launch::Verdict::Error(reason) = &verdict {
        return Err(AttemptError::Transport(format!(
            "{} start failed: {reason}",
            session_launch::BIN_NAME
        )));
    }
    // Both success verdicts are confirmed rather than trusted: the
    // `already-running` loser can print before the winner has bound.
    session_launch::confirm_serving(&config.socket, SPAWN_CONFIRM_BUDGET.mul_f64(scale))
        .await
        .map_err(|error| AttemptError::Transport(format!("{error:#}")))?;
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

/// Subscribe on a fresh connection, then snapshot on the control one,
/// and fence the snapshot against the ack.
///
/// The fence is the higher of the ack's revision and the snapshot's own
/// (`ipc.md` #eventssubscribe). Taking the max rather than the
/// snapshot's alone is the safe reading: a snapshot taken after the ack
/// can only be at or past it, so the pair is a floor either way.
async fn subscribe_and_snapshot(
    socket: &Path,
    lease: &str,
    control: &mut IpcClient,
) -> Result<(EventRx, tokio::task::AbortHandle, HostMirror), AttemptError> {
    // Bounded: this dials and handshakes, and a peer that accepts
    // without answering must not hold the connection in `Connecting`.
    let stream = tokio::time::timeout(leg(), EventStream::connect(socket, lease))
        .await
        .map_err(|_| AttemptError::Transport(format!("{} timed out", ops::EVENTS_SUBSCRIBE)))??;
    let ack = stream.revision();
    let (events, pump) = spawn_event_pump(stream);

    match snapshot(control, ack).await {
        Ok(mirror) => Ok((events, pump, mirror)),
        // The pump is already reading its own socket, and nothing else
        // holds it yet — an early return that merely dropped the abort
        // handle would leave it detached and subscribed forever.
        Err(error) => {
            pump.abort();
            Err(error)
        }
    }
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
fn spawn_event_pump(mut stream: EventStream) -> (EventRx, tokio::task::AbortHandle) {
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
    (rx, handle.abort_handle())
}

/// How one round — dial, then serve — ended. Both halves produce the
/// same four outcomes, so both funnel into one handler in [`run`].
enum ConnEnd {
    Shutdown,
    FeedClosed,
    /// The compatibility gate refused. Only a dial can produce it.
    Incompatible(Box<super::state::BuildMismatch>),
    Stopping(String),
    Dropped(String),
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
                if let Some(end) = run_intent(&mut live, intent).await {
                    return end;
                }
            }
        }
    }
}

/// Send one queued op through the control client and answer its caller.
/// `Some(_)` means the connection is finished.
async fn run_intent(live: &mut Live, mut intent: HostIntent) -> Option<ConnEnd> {
    // Taken rather than cloned: the params are this intent's alone, and
    // `answer` never reads them.
    let mut params = std::mem::take(&mut intent.params);
    if intent.needs_lease {
        let Some(object) = params.as_object_mut() else {
            intent.answer(Err(HostOpError::Rejected {
                code: ServerCode::InvalidParam,
                message: "a lease-gated op needs an object for params".into(),
            }));
            return None;
        };
        object.insert(
            "lease".into(),
            serde_json::Value::String(live.lease.clone()),
        );
    }

    let op = intent.op.clone();
    let sent = tokio::time::timeout(leg(), live.control.call_raw(&op, params)).await;
    match sent {
        Ok(Ok(result)) => {
            intent.answer(Ok(result));
            None
        }
        Ok(Err(error)) => {
            let (fault, surfaced) = queue::classify(&error);
            // An older session refusing the op it never had is an
            // ordinary `Surfaced` refusal — the connection is fine, and
            // the client is not going to stop having focus to report —
            // so it is said once and then let be.
            if op == ops::SESSION_SET_FOCUS
                && matches!(
                    &surfaced,
                    HostOpError::Rejected {
                        code: ServerCode::UnknownOp,
                        ..
                    }
                )
                && !std::mem::replace(&mut live.focus_unsupported, true)
            {
                tracing::info!(
                    "this host session predates session.set_focus; its attached \
                     tab suppresses its own notifications"
                );
            }
            intent.answer(Err(surfaced));
            match fault {
                OpFault::Surfaced => None,
                // Somebody else drives this session now: terminal, and
                // the banner says so.
                OpFault::LeaseLost(ServerCode::TakenOver) => {
                    Some(ConnEnd::Stopping("taken-over".into()))
                }
                // `connect-required` is not a stop: the session is fine,
                // our lease is not. Reconnecting — which is a takeover —
                // is the documented recovery, so drop into it.
                OpFault::LeaseLost(_) => {
                    Some(ConnEnd::Dropped("the lease is no longer valid".into()))
                }
                OpFault::ShuttingDown => Some(ConnEnd::Stopping("stop".into())),
                OpFault::Transport(reason) => Some(ConnEnd::Dropped(reason)),
            }
        }
        Err(_elapsed) => {
            intent.answer(Err(HostOpError::Transport(format!("{op} timed out"))));
            Some(ConnEnd::Dropped(format!("{op} timed out")))
        }
    }
}

/// Rebuild the mirror after a revision gap: a fresh subscription and a
/// fresh `tab.list`, fenced against the new ack exactly as at connect.
async fn resync(config: &ConnectionConfig, live: &mut Live) -> Result<(), AttemptError> {
    let (events, pump, mirror) =
        subscribe_and_snapshot(&config.socket, &live.lease, &mut live.control).await?;
    // Replacing the pump aborts the old one through `Live`'s field, so
    // the stale subscription's task cannot keep pushing.
    live.pump.abort();
    live.events = events;
    live.pump = pump;
    // The handle stays the same one the UI already holds; only its
    // contents are replaced.
    live.mirror.reset(mirror);
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

fn publish_lease(feed: &EngineFeedSender, host: HostId, lease: String) -> bool {
    feed.send(EngineFeed::HostLease(host, lease))
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
    use roost_ipc::messages::{EventEnvelope, Project};

    use super::*;

    fn feed_items(rx: &mut crate::engine_feed::EngineFeedReceiver) -> Vec<EngineFeed> {
        let mut batch = crate::engine_feed::EngineBatch::default();
        std::iter::from_fn(|| rx.try_next(&mut batch)).collect()
    }

    fn seeded_list(revision: Option<u64>) -> TabListResult {
        TabListResult {
            projects: vec![Project {
                id: 1,
                name: "p".into(),
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
            held_lease: None,
            client_build: "gb".into(),
            theme: Arc::new(Mutex::new(super::super::blank_theme())),
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

    /// A session socket that answers exactly one request, reports which
    /// op it was, and refuses it.
    ///
    /// `taken-over` is reserved for `events.subscribe` so the two shapes
    /// the case below tells apart end differently: a task that ran the
    /// lease guard settles as `TakenOver`, and one that went straight to
    /// the wire prologue drops.
    fn one_request_session(socket: &Path) -> tokio::sync::oneshot::Receiver<String> {
        let listener = tokio::net::UnixListener::bind(socket).expect("bind a fake session");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (reader, mut writer) = stream.into_split();
            let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(reader));
            let Ok(Some(line)) = lines.next_line().await else {
                return;
            };
            let request: serde_json::Value = serde_json::from_str(&line).expect("a request");
            let op = request["op"].as_str().unwrap_or_default().to_string();
            let code = if op == ops::EVENTS_SUBSCRIBE {
                "taken-over"
            } else {
                "internal"
            };
            let mut body = serde_json::to_vec(&serde_json::json!({
                "id": request["id"],
                "ok": false,
                "error": { "code": code, "message": "refused" },
            }))
            .expect("encode a response");
            body.push(b'\n');
            let _ = tokio::io::AsyncWriteExt::write_all(&mut writer, &body).await;
            let _ = tx.send(op);
        });
        rx
    }

    /// Run one whole task against [`one_request_session`], and report
    /// the op that session saw first alongside the state the task
    /// settled in.
    async fn first_op_and_final_state(
        socket: PathBuf,
        held_lease: Option<String>,
    ) -> (String, HostConnState) {
        let seen = one_request_session(&socket);
        let mut config = config(socket, HostTransport::Ssh, ConnectMode::Dial);
        config.held_lease = held_lease;
        let (feed, mut rx) = crate::engine_feed::channel();
        let (_ops, ops_rx) = super::super::HostOps::channel();
        run(
            config,
            HostIdMinter::new(),
            ops_rx,
            feed,
            Arc::new(Shutdown::default()),
        )
        .await;
        let op = seen.await.expect("the session read a request");
        let final_state = feed_items(&mut rx)
            .into_iter()
            .filter_map(|item| match item {
                EngineFeed::HostState(_, state) => Some(state),
                _ => None,
            })
            .next_back()
            .expect("a task publishes at least one state");
        (op, final_state)
    }

    /// A lease that arrives on the **config** is presented before the
    /// first dial, not after it.
    ///
    /// This is what makes an ssh auto-reconnect as safe as a localhost
    /// auto-retry: an ssh drop tears its tunnel down, so the retry is a
    /// *fresh task* whose own `held_lease` starts empty — and a fresh
    /// task that simply reconnected would take the session back from
    /// another client with no probe at all, `takeover: true` being the
    /// wire contract. The control case is the same task with no lease:
    /// it opens with the prologue, exactly as an explicit Connect does.
    #[tokio::test]
    async fn a_config_seeded_lease_is_presented_before_the_first_dial() {
        let dir = tempfile::tempdir().expect("temp dir");

        let (op, state) = first_op_and_final_state(
            dir.path().join("carried.sock"),
            Some("lease-from-the-last-connection".into()),
        )
        .await;
        assert_eq!(
            op,
            ops::EVENTS_SUBSCRIBE,
            "the first attempt asks about the lease before dialing"
        );
        assert_eq!(
            state,
            HostConnState::TakenOver,
            "and a session that says somebody else drives it is terminal"
        );

        let (op, state) = first_op_and_final_state(dir.path().join("bare.sock"), None).await;
        assert_eq!(
            op,
            ops::SESSION_IDENTIFY,
            "with no lease there is nothing to ask about"
        );
        assert!(matches!(state, HostConnState::Disconnected(_)), "{state:?}");
    }

    /// A session that grants a lease and then refuses everything after
    /// it, reporting the lease of every `events.subscribe` it is shown.
    ///
    /// So the prologue fails at step 3, `session.set_theme` — after the
    /// `session.connect` that actually moved ownership — and each
    /// attempt's probe reports which lease that attempt believes it
    /// holds. The first probe is answered `connect-required` so the
    /// attempt behind it proceeds (that answer proves nothing); the
    /// second is answered `taken-over`, which is terminal and is what
    /// ends the task.
    ///
    /// One connection at a time is enough, and deliberate: a failed
    /// prologue drops its control client, so the next accept is the
    /// retry's.
    fn a_session_that_grants_a_lease_then_fails(
        socket: &Path,
        granted: &str,
    ) -> mpsc::UnboundedReceiver<String> {
        let listener = tokio::net::UnixListener::bind(socket).expect("bind a fake session");
        let granted = granted.to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut probes = 0;
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut lines =
                    tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(reader));
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: serde_json::Value =
                        serde_json::from_str(&line).expect("a request");
                    let id = request["id"].clone();
                    let response = match request["op"].as_str().unwrap_or_default() {
                        ops::SESSION_IDENTIFY => serde_json::json!({
                            "id": id,
                            "ok": true,
                            "result": {
                                "app_version": "test",
                                "session_protocol":
                                    roost_ipc::messages::SESSION_PROTOCOL_VERSION,
                                "payload_kinds": [super::super::state::REQUIRED_PAYLOAD_KIND],
                                "libghostty_build": "gb",
                                "session_id": "s1",
                                "started_at": "2026-01-01T00:00:00Z",
                            },
                        }),
                        ops::SESSION_CONNECT => serde_json::json!({
                            "id": id,
                            "ok": true,
                            "result": { "lease": granted, "revision": 1 },
                        }),
                        ops::EVENTS_SUBSCRIBE => {
                            let _ = tx.send(
                                request["params"]["lease"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                            );
                            probes += 1;
                            let code = if probes == 1 {
                                "connect-required"
                            } else {
                                "taken-over"
                            };
                            serde_json::json!({
                                "id": id,
                                "ok": false,
                                "error": { "code": code, "message": "refused" },
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
                    {
                        break;
                    }
                }
            }
        });
        rx
    }

    /// A lease minted by a `session.connect` whose prologue then failed
    /// is still the lease this task holds.
    ///
    /// Step 2 is where ownership moves on the wire: the session grants
    /// the new lease and tombstones the one it replaced. A step after it
    /// failing does not undo that — so a task that went on presenting
    /// the lease it started with would probe with a tombstone, be told
    /// `taken-over`, and settle terminally as somebody else's session
    /// with no other client ever involved (plan 040 §3.7).
    #[tokio::test]
    async fn a_lease_minted_before_a_failed_prologue_is_the_one_presented_next() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("granting.sock");
        let mut probed = a_session_that_grants_a_lease_then_fails(&socket, "lease-minted-here");

        // Localhost, so the failed attempt is retried by this same task
        // and the second attempt's probe is what the session sees next.
        let mut config = config(socket, HostTransport::LocalSession, ConnectMode::Dial);
        config.held_lease = Some("lease-the-connect-replaced".into());
        let (feed, _rx) = crate::engine_feed::channel();
        let (_ops, ops_rx) = super::super::HostOps::channel();
        run(
            config,
            HostIdMinter::new(),
            ops_rx,
            feed,
            Arc::new(Shutdown::default()),
        )
        .await;

        let mut presented = Vec::new();
        while let Ok(lease) = probed.try_recv() {
            presented.push(lease);
        }
        assert_eq!(
            presented,
            vec![
                "lease-the-connect-replaced".to_string(),
                "lease-minted-here".to_string()
            ],
            "the retry presents the lease the failed prologue minted, not the one it started with"
        );
    }

    /// The takeover-ping-pong guard. A bare EOF cannot tell a dead wire
    /// from a takeover whose `session.stopping` envelope was lost, so an
    /// auto-retry asks with the old lease before it takes the session
    /// back — and only the one answer that *proves* somebody else drives
    /// it stops the retry.
    #[test]
    fn only_a_taken_over_probe_stops_an_auto_retry() {
        assert!(proves_takeover(Some(&ClientError::Server {
            code: "taken-over".into(),
            message: "someone else".into(),
        })));

        // The tombstone was forgotten, or the session restarted: nothing
        // here says another client is driving, so the reconnect (which
        // is a takeover) proceeds.
        assert!(!proves_takeover(Some(&ClientError::Server {
            code: "connect-required".into(),
            message: "no lease".into(),
        })));
        // The session is going down. Reconnecting says so honestly;
        // refusing to would report the wrong reason.
        assert!(!proves_takeover(Some(&ClientError::Server {
            code: "shutting-down".into(),
            message: "latched".into(),
        })));
        // A probe that could not reach anybody proves nothing at all —
        // stranding a client whose session is fine is the worse failure.
        assert!(!proves_takeover(Some(&ClientError::Disconnected)));
        // The lease still answers: the drop was the wire.
        assert!(!proves_takeover(None));
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
        let queued = ops.call("tab.open", serde_json::json!({}), false);

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
            ops.call("tab.open", serde_json::json!({}), false).await,
            Err(HostOpError::Unavailable),
            "the closed queue refuses rather than swallowing"
        );
    }

    #[test]
    fn a_shutting_down_refusal_becomes_a_stop_and_a_takeover_its_own_reason() {
        let stop = AttemptError::from(ClientError::Server {
            code: "shutting-down".into(),
            message: "latched".into(),
        });
        assert!(matches!(stop, AttemptError::Stopping(reason) if reason == "stop"));

        let taken = AttemptError::from(ClientError::Server {
            code: "taken-over".into(),
            message: "someone else".into(),
        });
        assert!(matches!(taken, AttemptError::Stopping(reason) if reason == "taken-over"));

        let other = AttemptError::from(ClientError::Server {
            code: "invalid-param".into(),
            message: "nope".into(),
        });
        assert!(matches!(other, AttemptError::Transport(_)));
    }
}
