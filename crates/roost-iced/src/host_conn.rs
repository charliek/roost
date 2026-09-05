//! Host sessions, client side: one connection owner per connected host.
//!
//! A "host" is a saved `roost-session` daemon (`HostSnapshot` in
//! `state.json`). Connecting one mints a fresh [`HostId`] and starts a
//! task that owns the control connection, the subscribed event stream,
//! and a mirror of that session's workspace; everything it learns
//! reaches the UI through the engine feed, in the same FIFO as local
//! events.
//!
//! Four pieces, each testable on its own:
//!
//! * [`state`] — the connection state machine, the compatibility gate,
//!   and the jittered capped backoff. Pure.
//! * [`mirror`] — the client-side projection of a host's workspace,
//!   fenced against the event stream. Pure, plus the shared handle the
//!   task writes and the UI reads.
//! * [`queue`] — the per-host op queue (plan 037 §3.9). One worker, one
//!   order, flushed with errors when the connection dies.
//! * [`task`] — the connection owner that wires the three to a socket.
//!
//! # Incarnations
//!
//! [`HostId`] identifies a connection *instance*, not a host
//! (`roost-ui-model`'s `keys.rs` states the contract). Every connect
//! attempt that follows one which published data mints a fresh id, and
//! the `Connecting` state carries the id it replaces — so a consumer
//! purges the dead incarnation and rebuilds from the fresh mirror off a
//! single message. Anything still keyed on the old id (a delayed
//! callback, a queued message) then matches nothing and is dropped by
//! the ordinary lookup, which is the whole staleness mechanism.
//!
//! # What C4 deliberately does not build
//!
//! The attach data plane. A host tab's bytes and snapshot payloads are
//! C5's, because the decoder and the hydrated `Terminal` are
//! main-thread-only; C4 stops at handing C5 a live control client with
//! the lease, an ordered queue to mint `tab.attach` tokens on, and a
//! mirror that already knows which tabs exist.
//!
//! # How C5/C6/C7 consume this
//!
//! * **State** is pulled, not pushed. [`HostConnSet::mirror`] and
//!   [`HostConnSet::connected`] hand out an [`Arc<SharedMirror>`]; a
//!   consumer calls [`SharedMirror::read`] while it draws and gets
//!   whatever the connection task has written by then. There is no
//!   per-commit snapshot to keep, and none to go stale.
//! * **Wakes** are pushed, and they coalesce.
//!   [`HostWorkspaceEvent::Applied`] says the mirror moved; several may
//!   describe commits the mirror has already passed by the time the
//!   drain runs, which is correct — the mirror is the authority.
//! * **Per-commit facts** ride the item, not the mirror.
//!   `Applied::events` is the batch verbatim, and it is the only place
//!   C5's `tab.effect` and C8's `notification.fired` exist. A consumer
//!   that skips items loses them; a consumer that reads the mirror
//!   instead never had them.

// The C5/C6/C7 seam. Everything a connection needs to *run* is live
// from C4 — launch auto-reconnect drives the whole task — but the
// accessors the sidebar, the palette verbs and the attach path will
// call have no caller yet. One module-level expectation rather than
// thirty item-level ones; it fires as unfulfilled the moment the last
// of them is used, which is exactly when it should be deleted.
#![expect(
    dead_code,
    reason = "the host-client API surface C5/C6/C7 consume lands with them"
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use roost_ipc::messages::{ops, EventEnvelope, OscColorsParams, RetrySchedule};
use roost_ipc::ssh::{SshFailure, SshTunnel};
use roost_ui_model::keys::{HostId, TabKey};
use roost_ui_model::theme::Theme;
use tokio::task::AbortHandle;

pub(crate) mod mirror;
pub(crate) mod queue;
pub(crate) mod reconnect;
pub(crate) mod restart;
pub(crate) mod state;
pub(crate) mod task;

pub(crate) use mirror::SharedMirror;
pub(crate) use queue::{HostIntent, HostOpError, HostOps};
pub(crate) use reconnect::{Decision, DropInput};
pub(crate) use state::{HostConnState, HostTransport};
pub(crate) use task::{ConnectMode, Shutdown};

/// How far wall-clock time may run past an armed delay before the
/// handler reads it as a suspend rather than a busy event loop.
///
/// tokio's timer is `Instant`-based and the platforms Roost ships on
/// exclude suspend from `CLOCK_MONOTONIC`, so a lid closed one second
/// after a drop wakes with the timer firing at once and attempts 2–10
/// burning while the radio is still associating. `SystemTime` is the
/// right clock to catch that precisely because it *does* advance across
/// suspend.
///
/// Thirty seconds because no scheduling overshoot on a loaded UI thread
/// comes near it, so the only thing that trips this is a machine that
/// was actually asleep — or a forward NTP step, which costs one extra
/// base-delay wait and nothing else.
const SUSPEND_SKEW: Duration = Duration::from_secs(30);

/// Who asked for a connection.
///
/// [`ConnectMode`] answers *what to do* with a target; this answers *who
/// is waiting*, and the two are genuinely independent — an IPC
/// `host.connect` from `roostctl` arrives as [`ConnectMode::Dial`],
/// exactly like a click on the sidebar's ↻. Deriving "a human asked"
/// from the mode would therefore conflate the two, and the one thing
/// that must never happen is a modal opening to ask a machine a
/// question (plan 039 §3.5).
///
/// The bootstrap consent gate is what branches on it
/// (`app::bootstrap::offer_for`), and it is the *only* thing that does:
/// a card is offered to `User` and to nobody else. The toast and the
/// band still turn on attendedness, which is a different question —
/// "who hears about this?" rather than "who could answer a modal?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestOrigin {
    /// A person: a palette row, the sidebar's ↻, a banner, a dialog.
    User,
    /// A machine: the IPC op set (`roostctl host connect`), and the
    /// launch-time auto-reconnect, which nobody is sitting in front of
    /// either.
    Ipc,
}

/// Why this attempt exists.
///
/// [`RequestOrigin`] cannot answer it: `roostctl host connect` is an
/// explicit machine-driven connect and arrives as exactly the same
/// origin and mode an auto-reconnect would. So the two questions are
/// separate — "is there somebody there?" and "did anybody ask for *this*
/// attempt?" — and the second is the one a retry ladder turns on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptCause {
    /// Somebody asked for this one: the sidebar's ↻, a palette row, the
    /// Add Host dialog, `roostctl host connect`. It supersedes whatever
    /// a schedule had in mind, so it clears the outage entry outright.
    Explicit,
    /// The schedule asked. It leaves the outage alone — the attempt
    /// counter, and the lease [`Outage`] carries, are what bound the
    /// ladder and keep it from stealing a session back.
    AutoReconnect,
}

/// Why reaching a host failed: the classified family when the far side
/// is what refused, and the line the band and the toast render.
///
/// The pair travels together on purpose. The classifier
/// ([`roost_ipc::ssh::classify_ssh_failure`]) exists precisely so a
/// caller can route on "the binary is not installed over there" without
/// matching substrings of user-facing copy — but the copy is what the
/// UI shows, so collapsing to one or the other loses something. The
/// bootstrap offer and the retry ladder both branch on
/// [`Self::family`] — the ladder on [`Self::truncated`] as well, which
/// is why the two travel together — while the band and the toast render
/// [`Self::message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectFailure {
    /// `None` when the failure was this side's — a scratch directory
    /// that could not be made, a target string that means nothing, a
    /// socket that would not answer — which has no family and no remedy
    /// on the far host.
    pub(crate) family: Option<roost_ipc::ssh::SshFailure>,
    /// Whether the stderr [`Self::family`] was classified from was
    /// incomplete — a drain that ran out of time, or a byte cap that
    /// discarded the leading bytes. `false` for a failure whose verdict
    /// was not read out of a tail at all (this side's own failure, an
    /// establish that timed out). It is here so a retry policy can
    /// refuse to act on evidence it did not fully see.
    pub(crate) truncated: bool,
    pub(crate) message: String,
}

impl ConnectFailure {
    /// A failure with no far-side family: this client's own, or a
    /// transport that has no classifier.
    pub(crate) fn unclassified(message: impl Into<String>) -> Self {
        Self {
            family: None,
            truncated: false,
            message: message.into(),
        }
    }

    /// A classified far-side failure, rendered for `target`.
    pub(crate) fn classified(target: &str, failure: roost_ipc::ssh::SshFailure) -> Self {
        Self {
            message: failure.message(target),
            family: Some(failure),
            truncated: false,
        }
    }
}

/// A bare message with no family — what a panicked or cancelled engine
/// op reports (`spawn_engine_op`), and the shape every other op's error
/// already has.
impl From<String> for ConnectFailure {
    fn from(message: String) -> Self {
        Self::unclassified(message)
    }
}

impl From<roost_ipc::ssh::SshTunnelError> for ConnectFailure {
    fn from(error: roost_ipc::ssh::SshTunnelError) -> Self {
        Self {
            family: error.failure().cloned(),
            truncated: error.truncated(),
            message: error.to_string(),
        }
    }
}

impl From<roost_ipc::ssh::VerifyError> for ConnectFailure {
    fn from(error: roost_ipc::ssh::VerifyError) -> Self {
        Self {
            family: error.failure().cloned(),
            truncated: error.truncated(),
            // `{:#}` is what both callers printed before this type
            // existed — the anyhow chain on the socket arm, the ssh
            // error's own message on the other.
            message: format!("{error:#}"),
        }
    }
}

/// A host's mirror moving forward, as it reaches the UI.
///
/// The mirror itself does **not** travel on the feed: the task writes
/// [`SharedMirror`] in place and this is only the notification that it
/// moved, so a chatty host cannot pile full-workspace clones onto an
/// unbounded channel. A consumer reads the current state through
/// [`HostConnSet::mirror`] at drain time.
#[derive(Debug)]
pub(crate) enum HostWorkspaceEvent {
    /// The mirror was built or rebuilt from a fenced `tab.list`.
    /// Everything keyed on the previous contents is stale — the server
    /// is authoritative, so this is purge-then-rebuild, never a merge.
    ///
    /// It carries the handle because this is the one item that
    /// introduces it: a reset is the only point at which the set learns
    /// which [`SharedMirror`] an incarnation reads from.
    Reset(Arc<SharedMirror>),
    /// One commit applied. The mirror already reflects it.
    ///
    /// `events` is the batch verbatim, because the mirror deliberately
    /// models workspace facts only: `tab.effect` (C5's to apply) and
    /// `notification.fired` (C8's) ride here rather than being folded
    /// away. They are exact per-commit even though the mirror behind
    /// them may already be further ahead.
    Applied {
        revision: u64,
        events: Vec<EventEnvelope>,
    },
}

/// Which connection an incarnation was minted by: the saved host, and
/// *which* of that host's connections.
///
/// The generation is what makes the attribution safe across a rapid
/// disconnect + reconnect. Aborting a task is not instantaneous, so the
/// replaced task can still mint and publish after its replacement is
/// live; keyed on the host name alone those publications would land on
/// the replacement — a stale `Connecting` purging the new mirror, a
/// stale `Connected` resurrecting a connection that is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Registration {
    host: String,
    generation: u64,
}

/// Mints connection-instance ids, and remembers which connection each
/// one belongs to.
///
/// The ownership half is not bookkeeping for its own sake: feed items
/// are tagged with the incarnation alone, so without a registry the app
/// would have to *guess* which host a `Connecting` came from — and two
/// hosts reconnecting at once would make that guess wrong. A task
/// registers its new id before it publishes anything under it, so the
/// lookup on the drain side either succeeds or is genuinely stale.
///
/// One per app, shared by every host, so two hosts' incarnations can
/// never collide.
#[derive(Debug, Clone)]
pub(crate) struct HostIdMinter {
    next: Arc<AtomicU32>,
    owners: Arc<Mutex<HashMap<HostId, Registration>>>,
}

impl HostIdMinter {
    pub(crate) fn new() -> Self {
        Self {
            // 1: `HostId::LOCAL` is 0 and reserved.
            next: Arc::new(AtomicU32::new(1)),
            owners: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A fresh incarnation, registered to one connection of `host`.
    pub(crate) fn mint(&self, host: &str, generation: u64) -> HostId {
        let mut raw = self.next.fetch_add(1, Ordering::Relaxed);
        // 32 bits of instance ids is ~4 billion connects; wrapping onto
        // LOCAL would alias a host's tabs onto the local workspace's, so
        // skip zero rather than let that happen.
        if raw == HostId::LOCAL.raw() {
            raw = self.next.fetch_add(1, Ordering::Relaxed);
        }
        let id = HostId::new(raw);
        self.lock().insert(
            id,
            Registration {
                host: host.to_string(),
                generation,
            },
        );
        id
    }

    fn registration(&self, id: HostId) -> Option<Registration> {
        self.lock().get(&id).cloned()
    }

    fn forget_id(&self, id: HostId) {
        self.lock().remove(&id);
    }

    /// Drop every registration for a host, whatever generation minted
    /// it.
    ///
    /// By name and not by generation on purpose: this runs at
    /// disconnect and at the start of a reconnect, *before* the
    /// replacement task exists, so it is also what reaps the ids a
    /// previous generation minted on its way out. Anything the outgoing
    /// task registers after this point is dropped by the generation
    /// check rather than by being absent, and the next `forget` sweeps
    /// it up.
    fn forget_host(&self, host: &str) {
        self.lock().retain(|_, owner| owner.host != host);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<HostId, Registration>> {
        self.owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The sentinel lives in `roost_ipc`, beside the classifier that acts on
/// it: `roostctl host add --verify` reads the same targets this does, and
/// a target that meant two different things depending on which binary
/// read it would be a bug nothing could see.
pub(crate) use roost_ipc::ssh::LOCALHOST_TARGET;

/// One control-plane leg's budget, as the connection task sizes it. Also
/// what bounds the attach path's data dial (`app::host_tab`) — the two
/// legs run against the same socket, and a bound one of them can outwait
/// is not a bound.
pub(crate) use task::leg as leg_budget;

/// The client's terminal palette in the wire's spelling.
///
/// The same colors [`crate::app::terminal_tab`] seeds a local terminal
/// with — a host's server-side terminals answer queries from this, so
/// the two ends agree on what "color 4" is.
pub(crate) fn theme_colors(theme: &Theme) -> OscColorsParams {
    let hex = |color: roost_vt::ColorRgb| format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b);
    OscColorsParams {
        foreground: hex(theme.foreground),
        background: hex(theme.background),
        cursor: hex(theme.cursor),
        palette: theme.palette.iter().copied().map(hex).collect(),
    }
}

/// §3.8's band line for an armed retry: `reconnecting in 8s (3/10)`.
///
/// Written once, when the retry is armed, and stating the delay rather
/// than counting down — a live countdown needs a per-second ticker and a
/// sidebar redraw per host in backoff, for a number nobody acts on. The
/// `(3/10)` is what makes the give-up legible before it happens.
///
/// The seconds round *up*, so the first jittered delay off a one-second
/// base reads `1s` rather than `0s`.
fn retry_line(delay: Duration, attempt: u32, budget: u32) -> state::Disconnected {
    let seconds = delay.as_millis().div_ceil(1_000).max(1);
    state::Disconnected {
        reason: format!("reconnecting in {seconds}s ({attempt}/{budget})"),
        detail: None,
        retry_in: Some(delay),
    }
}

/// §3.8's line for a ladder that spent its budget. The ↻ Reconnect row
/// never left the screen, so this says what stopped, not what is lost.
fn gave_up_copy(attempts: u32, family_copy: Option<&str>) -> String {
    match family_copy {
        Some(copy) => format!("reconnect gave up after {attempts} tries — {copy}"),
        None => format!("reconnect gave up after {attempts} tries"),
    }
}

/// A disconnected state's one-line reason, when it has one.
fn disconnected_reason(state: &HostConnState) -> Option<&str> {
    match state {
        HostConnState::Disconnected(disconnected) => Some(disconnected.reason.as_str()),
        _ => None,
    }
}

/// Open a tunnel to `target` and warm its mux, as one failable step.
///
/// A failed establish is shut down here rather than dropped: `SshTunnel`
/// tears itself down *blocking* in `Drop`, and dropping it on a runtime
/// worker would park that worker on a `-O exit`. Shutting it down first
/// leaves Drop with nothing to do — and retires the scratch directory
/// this attempt claimed, which is its own and nobody else's.
async fn establish_tunnel(
    host: &str,
    target: &roost_ipc::ssh::SshTarget,
) -> Result<Arc<SshTunnel>, ConnectFailure> {
    let tunnel = SshTunnel::open(host, target, roost_ipc::ssh::SshTunnelOptions::from_env())
        .await
        .map_err(ConnectFailure::from)?;
    if let Err(error) = tunnel.establish().await {
        tunnel.shutdown().await;
        return Err(error.into());
    }
    Ok(Arc::new(tunnel))
}

/// One saved host's connection, as the app holds it.
struct HostConn {
    label: String,
    socket: PathBuf,
    transport: HostTransport,
    /// Which of this host's connections this is. Every id the task
    /// mints is registered under it, and the drain side drops anything
    /// stamped with an older one.
    generation: u64,
    ops: HostOps,
    shutdown: Arc<Shutdown>,
    /// The incarnation currently being served, once its `Connecting`
    /// has been drained off the feed.
    incarnation: Option<HostId>,
    state: HostConnState,
    /// The last client focus this connection was told, so an unchanged
    /// one is not resent (`Some(None)` is "told: nothing here is
    /// focused"; the outer `None` is "never told").
    ///
    /// Reset whenever the incarnation changes or the connection leaves
    /// `Connected`, because a session that just came up believes its own
    /// headless default — the dedup must never be what stops a fresh
    /// incarnation from hearing the truth.
    focus_sent: Option<Option<i64>>,
}

impl Drop for HostConn {
    fn drop(&mut self) {
        // Quitting disconnects; it never stops the session (D8).
        //
        // Signal, don't abort. The task's own shutdown closes its queue
        // and answers every intent still on it with `Disconnected`; an
        // abort here would instead drop those reply channels, and a
        // caller awaiting one would see a bare cancellation. The signal
        // is a guarantee rather than a request because the task bounds
        // its own unwind against it (`task::SHUTDOWN_GRACE`), so there
        // is nothing left for a hard abort to protect.
        self.shutdown.request();
    }
}

/// What an explicitly disconnected host leaves behind: the rows its last
/// connection published, kept so the section renders dimmed rather than
/// empty (plan 037 §3.1 — "rows stay listed at reduced opacity", because
/// the session still holds those shells).
///
/// A *dropped* connection needs none of this: its `HostConn` is still in
/// the set, so [`HostConnSet::section`] finds the mirror the ordinary
/// way. Only an explicit disconnect removes the connection, and this is
/// what stands in for it until a reconnect (which purges and rebuilds
/// from a fresh `tab.list`) or a `host.remove` (which drops it with the
/// host).
struct RetainedSection {
    label: String,
    incarnation: HostId,
    mirror: Arc<SharedMirror>,
    /// Fixed at `Disconnected`: nothing drives a retained section, so it
    /// can never be anything else. Stored rather than synthesized
    /// because [`HostSectionView`] borrows it.
    state: HostConnState,
}

/// One saved host's live facts, as the sidebar reads them.
pub(crate) struct HostSectionView<'a> {
    pub(crate) label: &'a str,
    pub(crate) state: &'a HostConnState,
    pub(crate) incarnation: Option<HostId>,
    pub(crate) mirror: Option<&'a Arc<SharedMirror>>,
}

/// Every connected host, and the mirrors their tasks publish.
///
/// Inert with zero hosts: nothing is spawned, nothing is drained, and
/// the sidebar is exactly today's (roadmap D8's zero-change rule).
pub(crate) struct HostConnSet {
    runtime: tokio::runtime::Handle,
    feed: crate::engine_feed::EngineFeedSender,
    minter: HostIdMinter,
    /// The client's palette, shared with every task so a reconnect
    /// seeds the theme that is current *now*, not the one at spawn.
    theme: Arc<Mutex<OscColorsParams>>,
    client_build: String,
    /// Everything this set holds for one saved host, keyed on the saved
    /// host's stable id (`HostSnapshot.id`). See [`HostEntry`] — one
    /// entry per host is what makes "what does this host currently
    /// have?" one read and one teardown rather than six.
    entries: HashMap<String, HostEntry>,
    mirrors: HashMap<HostId, Arc<SharedMirror>>,
    /// Handed out by [`HostConnSet::connect`], never reused. See
    /// [`Registration`].
    next_generation: u64,
    /// Handed out by [`HostConnSet::open_ssh`], never reused.
    next_ssh_request: u64,
    /// In-flight establishes whose [`SshState`] is gone — displaced by a
    /// second [`HostConnSet::open_ssh`] or removed by a
    /// [`HostConnSet::disconnect`].
    ///
    /// Dropping an `AbortHandle` does not abort its task, and while the
    /// app is running that is exactly right: a displaced establish is
    /// deliberately left to land, so [`Self::tunnel_ready`] can drop its
    /// answer against [`SshState::request`] and hand the tunnel to
    /// [`Self::discard_tunnel`] — the only thing that sends `-O exit`
    /// and removes the scratch directory. Aborting one mid-flight would
    /// instead run [`SshTunnel`]'s *blocking* `Drop` on a runtime
    /// worker.
    ///
    /// This exists purely so the exit path can still reach them, because
    /// at exit nothing will ever drain their answer: a
    /// just-daemonized `ControlPersist=60s` master would outlive the
    /// app. [`Self::abandon_reconnects`] is the only place they are
    /// aborted.
    displaced: Vec<AbortHandle>,
}

/// Everything the set holds for one saved host, keyed on
/// `HostSnapshot.id`.
///
/// Created only where one of its fields is first written —
/// [`HostConnSet::mint_generation`], [`HostConnSet::connect`],
/// [`HostConnSet::open_ssh`], [`HostConnSet::begin_outage`] and
/// [`HostConnSet::set_bootstrap_note`] with a note — and removed only by
/// [`HostConnSet::remove`]. Nothing that *clears* a field creates one,
/// and nothing prunes an entry whose fields have all gone: an entry
/// holding a generation alone is exactly what [`Self::generation`] says
/// must outlive the connection.
#[derive(Default)]
struct HostEntry {
    conn: Option<HostConn>,
    /// Never `Some` while [`Self::conn`] is — `forget` clears it before
    /// `connect` inserts, and `disconnect` inserts it only after taking
    /// the connection.
    retained: Option<RetainedSection>,
    /// The last generation minted for this host — but outliving the
    /// [`HostConn`] it was minted for, which is the whole point. A
    /// disconnected host has no [`Self::conn`] and would otherwise read
    /// as "never connected"; a poller watching this number for an edge
    /// would then see it fall back to `0` and count the next attempt
    /// twice. `0` means never minted. Written by
    /// [`HostConnSet::mint_generation`]; dropped only by
    /// [`HostConnSet::remove`], with the host itself.
    generation: u64,
    /// The ssh transport, for a host reached over one.
    ///
    /// **This is where the tunnels live, and the seam is deliberate.**
    /// A tunnel is opened, torn down and read for its last failure at
    /// exactly the three moments a connection is started, dropped and
    /// reported — all of which this set already owns — and it already
    /// holds the runtime handle the establish runs on and the feed the
    /// answer comes back over. Hanging them off `App` instead would mean
    /// duplicating `connect`/`disconnect`/`remove`'s lifecycle in a
    /// second place and threading the runtime and the feed there twice.
    ssh: Option<SshState>,
    /// What an ssh host's retry ladder must carry across its own
    /// attempts. See [`Outage`].
    outage: Option<Outage>,
    /// What a bootstrap is doing to this host right now, or how it
    /// ended.
    ///
    /// It sits in front of every other reason
    /// [`HostConnSet::section_reason`] can give, because while a
    /// bootstrap runs it is the *most current* thing true about the
    /// host: the connect failure that started it is exactly what the job
    /// is answering, and going on showing it would leave the band
    /// describing a question that is already being dealt with.
    bootstrap_note: Option<String>,
}

/// One ssh host's outage: everything a retry ladder has to carry that
/// the retry itself would otherwise destroy (plan 040 §3.4).
///
/// It exists because the two natural homes are both wiped by the very
/// call that needs them. [`HostConnSet::open_ssh`] inserts a *fresh*
/// [`SshState`] on every attempt, and [`HostConnSet::connect`] calls
/// `forget` before it builds the config, destroying the old
/// [`HostConn`]. So the ladder's depth and the lease it must present
/// live here, in a store neither of them touches.
///
/// Created at the first drop of an outage and destroyed by anything
/// that ends one: a successful connect, an explicit attempt, a
/// disconnect, a remove, a give-up, a terminal settle.
pub(crate) struct Outage {
    ladder: reconnect::ReconnectLadder,
    /// The lease the connection that just died held, copied off
    /// [`SshState::lease`] on every drop that has one (§3.7). What the
    /// next attempt presents before taking the session back.
    lease: Option<String>,
    /// The retry waiting to fire, if one is. Deliberately without a
    /// request stamp beside it — see [`HostConnSet::arm_reconnect`].
    armed: Option<Armed>,
    /// Why the armed rung is armed: the classified copy of the failure
    /// that took the [`Decision::Retry`], published as
    /// `host.status`'s `retry.reason` (plan 044 §3.3, #399). Same
    /// wording and same source as what [`gave_up_copy`] appends — but
    /// not always the same *value*: the give-up reflects the drop that
    /// exhausted the ladder, and if that drop is a bare EOF it appends
    /// nothing while this field still holds the previous rung's family.
    ///
    /// Three rules, and each is load-bearing. It is written **only**
    /// where a `Retry` is taken, never earlier: a late same-generation
    /// `Dropped` reaches [`HostConnSet::schedule_reconnect`] as
    /// `Session(None)` (the overlay returns `None` under `seen`) and a
    /// write before the already-armed guard would erase the family the
    /// rung was actually armed for. It is *carried* by
    /// [`HostConnSet::restart_ladder`], which re-arms with no drop at
    /// all — the suspend reset is the same outage. And it dies with the
    /// struct in [`HostConnSet::clear_outage`], because an outage that
    /// is over has no rung to explain.
    ///
    /// `None` is ordinary: a bare bridge EOF has no family, and that is
    /// the usual first drop of an outage.
    family: Option<String>,
    /// The tunnel the drop left behind, kept for its `last_error` alone.
    dead_tunnel: Option<DeadTunnel>,
}

/// One armed retry: the timer, when it was armed, and for how long.
///
/// The two clock fields are §3.5's suspend detection. They are stored
/// rather than derived because the comparison is wall-clock against a
/// monotonic sleep, and only the arming side knows both halves.
struct Armed {
    handle: AbortHandle,
    at: SystemTime,
    delay: Duration,
}

/// A dead tunnel's failure slot, re-read once more when the timer fires.
///
/// C1 made `serve` record before the client-visible EOF, which is the
/// primary closure of the record-vs-EOF race; this is the belt. One
/// client runs several concurrent execs, so a *different* exec can
/// still record a graver family after the one whose EOF the connection
/// task saw — and dialing ten times into a changed host key is exactly
/// what §3.3 refuses. `shutdown()` has already run on this tunnel by the
/// time it lands here, so holding the `Arc` costs nothing and its
/// eventual `Drop` is a no-op.
enum DeadTunnel {
    /// The tunnel itself, with the generation already folded into the
    /// reason the band is showing.
    Tunnel { tunnel: Arc<SshTunnel>, seen: u64 },
    /// The same slot, injected. `SshTunnel::record` is private to
    /// `roost-ipc` and only that crate's own exec paths call it, so a
    /// unit test cannot move a real tunnel's `last_error` — this is the
    /// only way the fire-time re-check is reachable from one.
    #[cfg(test)]
    Recorded {
        generation: u64,
        failure: SshFailure,
        truncated: bool,
        seen: u64,
    },
}

impl DeadTunnel {
    /// A failure recorded since the drop, if there is one and it is
    /// news: `(generation, family, truncated)`.
    fn late_failure(&self) -> Option<(u64, SshFailure, bool)> {
        match self {
            Self::Tunnel { tunnel, seen } => {
                let recorded = tunnel.last_error()?;
                (recorded.generation > *seen).then_some((
                    recorded.generation,
                    recorded.failure,
                    recorded.truncated,
                ))
            }
            #[cfg(test)]
            Self::Recorded {
                generation,
                failure,
                truncated,
                seen,
            } => (generation > seen).then(|| (*generation, failure.clone(), *truncated)),
        }
    }
}

/// One saved host's ssh transport, from the moment a connect is asked
/// for until the host is disconnected or removed.
///
/// The entry exists *before* the tunnel does: an establish takes a full
/// TCP + auth handshake, and the window between asking and answering is
/// exactly when a user can cancel, disconnect, or ask again. `request`
/// is what closes it — an answer stamped with anything but the current
/// request is answering a question this host has moved past.
struct SshState {
    /// The target as the registry spells it, for [`SshFailure::message`].
    target: String,
    /// Which establish this entry is waiting on, or was last answered
    /// by.
    request: u64,
    label: String,
    mode: ConnectMode,
    /// Whether somebody is waiting on this attempt's answer. Only an
    /// attended failure raises a status line; an unattended one is the
    /// band's business alone.
    ///
    /// Derived from [`Self::cause`] *and* the mode: somebody asked for
    /// this particular attempt, and it is not the launch probe. Origin
    /// would be the wrong input — `roostctl host connect`'s reply
    /// carries no outcome, so that status line is its only failure
    /// surface and an `origin == User` rule would silence it.
    attended: bool,
    /// Who asked, for the bootstrap consent gate: an IPC dial and a
    /// user's click are both `Dial`, so this is the only place the
    /// difference survives. Every auto-reconnect stamps `Ipc`, which is
    /// what keeps a ladder from raising a card.
    origin: RequestOrigin,
    /// Whether anybody asked for *this* attempt, as opposed to who
    /// would hear about it. It is what an explicit connect clears an
    /// outage off, and what decides whether the next dial carries
    /// [`Outage::lease`].
    cause: AttemptCause,
    /// This attempt's connection generation, minted by
    /// [`HostConnSet::open_ssh`] when the handshake started and reused
    /// verbatim by the [`HostConnSet::connect`] a working tunnel
    /// reaches — so the two are the same number and an establish that
    /// never answers still has one.
    ///
    /// It is what makes [`Self::lease`] attributable: a feed item is
    /// tagged with an incarnation, and the connection an incarnation
    /// belongs to is only half the question here.
    generation: Option<u64>,
    /// The lease the connection under this attempt was granted, once it
    /// reached `Connected`.
    ///
    /// It lands here rather than on the [`Outage`] because of when it
    /// arrives (§3.7): at `Connected` no outage exists yet — one is
    /// created at the first drop — and a successful auto-reconnect
    /// *clears* the outage at exactly the moment the new lease is
    /// published. This entry, by contrast, is created at `open_ssh`,
    /// stamped here, and still untouched at the drop that copies it out.
    lease: Option<String>,
    /// `None` until the establish answers, and again once the tunnel is
    /// torn down.
    tunnel: Option<Arc<SshTunnel>>,
    /// The handshake this attempt spawned, while it is still running.
    ///
    /// Retained for exactly one caller, the exit path's
    /// [`HostConnSet::abandon_reconnects`]. A *superseded* establish is
    /// deliberately left to finish — its answer is dropped against
    /// [`Self::request`], and aborting it mid-flight would run
    /// [`SshTunnel`]'s blocking teardown on a runtime worker for no
    /// gain.
    establish: Option<AbortHandle>,
    /// The highest [`SshTunnel::last_error`] generation already folded
    /// into a reported reason. A tunnel bumps its generation once per
    /// `ssh` exec, so this is what separates "the failure the band is
    /// already showing" from "a new one since".
    seen: u64,
    /// Why the last attempt failed, classified. Two writers: an
    /// establish that never reached [`HostConnSet::connect`] (which
    /// publishes no `HostConnState` at all, so this is the only thing
    /// the band has to render), and [`HostConnSet::overlay_ssh_reason`],
    /// whose family is what C5b routes a bootstrap offer off.
    failure: Option<ConnectFailure>,
    /// Whether this attempt ever got a working connection.
    ///
    /// The bootstrap offer's other gate (plan 039 §3.5). A *first*
    /// connect and a long-lived session going away underneath the user
    /// arrive identically — the same `Disconnected` transition, the same
    /// `NotFound`/`NoSession` family, and the same [`Self::origin`],
    /// which is the establish's and stays `User` for hours. This is the
    /// only fact that separates them, and without it `roostctl session
    /// stop` on the far side would throw a consent card over whatever
    /// the user was typing locally.
    reached_connected: bool,
}

/// One finished establish, on its way back to the main thread.
///
/// The socket the connection dials cannot be known until the tunnel is
/// up, so the ssh path is two steps where the other two transports are
/// one: [`HostConnSet::open_ssh`] spawns the handshake, this lands on
/// the engine feed, and the drain hands it to
/// [`HostConnSet::tunnel_ready`] — which is the *only* place a tunnel's
/// bridge socket becomes a `ConnectionConfig`.
pub(crate) struct HostTunnelReady {
    pub(crate) host: String,
    pub(crate) request: u64,
    pub(crate) result: Result<Arc<SshTunnel>, ConnectFailure>,
}

impl HostConnSet {
    pub(crate) fn new(
        runtime: tokio::runtime::Handle,
        feed: crate::engine_feed::EngineFeedSender,
        theme: &Theme,
    ) -> Self {
        Self {
            runtime,
            feed,
            minter: HostIdMinter::new(),
            theme: Arc::new(Mutex::new(theme_colors(theme))),
            client_build: roost_vt::libghostty_build(),
            entries: HashMap::new(),
            mirrors: HashMap::new(),
            next_generation: 0,
            next_ssh_request: 0,
            displaced: Vec::new(),
        }
    }

    /// Whether this set holds any *live connection* — never "any state".
    /// A host with only a retained section, an outage or a generation is
    /// still a host this set has nothing running for.
    pub(crate) fn is_empty(&self) -> bool {
        !self.entries.values().any(|entry| entry.conn.is_some())
    }

    /// This host's entry, created if it has none.
    ///
    /// **The only entry-creating call in the set**, and it belongs at
    /// exactly the five sites where the old parallel maps inserted:
    /// [`Self::mint_generation`], [`Self::connect`], [`Self::open_ssh`],
    /// [`Self::begin_outage`] and [`Self::set_bootstrap_note`] with a
    /// note. Every read and every *clearing* write goes through
    /// `entries.get`/`get_mut` instead, so clearing a field on a host
    /// this set has never heard of stays the no-op it has always been.
    fn entry_mut(&mut self, host: &str) -> &mut HostEntry {
        self.entries.entry(host.to_string()).or_default()
    }

    /// Number the attempt that is starting now, and record it as this
    /// host's current generation.
    ///
    /// Called where an *attempt* begins — [`Self::open_ssh`] for a host
    /// reached over ssh, [`Self::connect`] for every other transport —
    /// so an establish that never comes up still advances the number
    /// `host.status` reports. A value handed out is never handed out
    /// again: the counter is set-wide and only ever climbs.
    fn mint_generation(&mut self, host: &str) -> u64 {
        self.next_generation += 1;
        let minted = self.next_generation;
        self.entry_mut(host).generation = minted;
        minted
    }

    /// Start (or restart) a connection to one saved host.
    ///
    /// An existing connection for the same saved host is dropped first —
    /// its task is aborted and its incarnation forgotten — so "Connect"
    /// on an already-connected host is a deliberate reconnect, which on
    /// this wire is a takeover.
    ///
    /// `cause` decides one thing here: whether the task starts holding
    /// the previous connection's lease — see [`Self::carried_lease`].
    pub(crate) fn connect(
        &mut self,
        host: &str,
        label: &str,
        socket: PathBuf,
        transport: HostTransport,
        mode: ConnectMode,
        cause: AttemptCause,
    ) {
        // The incarnation this reconnect displaces, threaded into the
        // replacement task so its FIRST `Connecting` carries it — that
        // is the one message consumers purge dead-incarnation state off,
        // and without it an explicit reconnect would leak everything the
        // old connection's tabs left behind (attach state, terminals,
        // inbox rows).
        let supersedes = self
            .entries
            .get(host)
            .and_then(|entry| entry.conn.as_ref()?.incarnation);
        let held_lease = self.carried_lease(host, cause);
        self.forget(host);
        // An ssh attempt started at [`Self::open_ssh`] and was numbered
        // there, so the connect a working tunnel reaches carries that
        // number rather than minting a second one — one attempt is one
        // generation, whether or not it ever gets a socket. The
        // `unwrap_or_else` is the impossible case (an ssh connect with
        // no entry behind it) taking a fresh number rather than
        // reusing one.
        let generation = match transport {
            HostTransport::Ssh => self
                .entries
                .get(host)
                .and_then(|entry| entry.ssh.as_ref()?.generation),
            _ => None,
        }
        .unwrap_or_else(|| self.mint_generation(host));

        let (ops, ops_rx) = HostOps::channel();
        let shutdown = Arc::new(Shutdown::default());
        let config = task::ConnectionConfig {
            host: host.to_string(),
            label: label.to_string(),
            socket: socket.clone(),
            transport,
            generation,
            supersedes,
            mode,
            held_lease,
            client_build: self.client_build.clone(),
            theme: Arc::clone(&self.theme),
        };
        // Detached on purpose: the task owns its own shutdown, bounds it
        // (`task::SHUTDOWN_GRACE`), and answers its queue on the way
        // out, so a handle kept here would only be a way to cut that
        // short. `HostConn::drop` signals instead.
        self.runtime.spawn(task::run(
            config,
            self.minter.clone(),
            ops_rx,
            self.feed.clone(),
            Arc::clone(&shutdown),
        ));

        self.entry_mut(host).conn = Some(HostConn {
            label: label.to_string(),
            socket,
            transport,
            generation,
            ops,
            shutdown,
            incarnation: None,
            focus_sent: None,
            // What the task is actually doing the moment it is spawned.
            // The feed's first `Connecting` replaces it — this is only
            // what a frame drawn in between reads, so it must not be a
            // state the machine cannot produce.
            state: HostConnState::Connecting { previous: None },
        });
    }

    /// Start an ssh-reached host's connection: open its tunnel, warm the
    /// mux, and only then dial.
    ///
    /// Two steps because the socket does not exist yet. The other two
    /// transports resolve a path and hand it straight to [`Self::connect`];
    /// an ssh target has no socket until a `bridge.sock` is bound, and
    /// binding it costs a full TCP + auth handshake. So the handshake runs
    /// on the engine runtime, its answer comes back over the engine feed
    /// as [`HostTunnelReady`], and [`Self::tunnel_ready`] is what calls
    /// `connect`. Nothing about the tunnel is ever awaited on the UI
    /// thread.
    ///
    /// Any tunnel this host already had is torn down first: a Connect is
    /// an unconditional reconnect on this wire, and reusing a mux whose
    /// master may already be wedged is exactly how a reconnect fails to
    /// be one.
    ///
    /// The teardown is *awaited by the replacement*, in the same task, so
    /// one host never has two `ssh` masters at once. That is hygiene
    /// rather than safety: what keeps a teardown from deleting the
    /// replacement's files is that every attempt claims a scratch
    /// directory of its own (`roost_ipc::ssh::scratch_dir_name`), so an
    /// overlap — a double Connect, an establish that lands late — has
    /// nothing to collide on.
    ///
    /// `origin` is passed in because it cannot be derived: an IPC
    /// `host.connect` arrives as `Dial`, indistinguishable from a click,
    /// so a modal opened off the mode would be asking `roostctl` a
    /// question (plan 039 §3.5).
    ///
    /// `cause` decides the other two things: whether this attempt clears
    /// the [`Outage`] a schedule was working through, and whether
    /// anybody is waiting to hear how it went. Attendedness is `cause`
    /// *and* mode rather than mode alone — a ladder mints ten `Dial`s
    /// nobody asked for, and each of them would otherwise raise a status
    /// line.
    pub(crate) fn open_ssh(
        &mut self,
        host: &str,
        label: &str,
        target: roost_ipc::ssh::SshTarget,
        mode: ConnectMode,
        origin: RequestOrigin,
        cause: AttemptCause,
    ) {
        // A fresh attempt supersedes whatever the last bootstrap left on
        // the band: either it worked and this connect is its sequel, or
        // it did not and the user is trying again — and in both cases
        // this attempt's own outcome is the newer answer.
        if let Some(entry) = self.entries.get_mut(host) {
            entry.bootstrap_note = None;
        }
        if cause == AttemptCause::Explicit {
            self.clear_outage(host);
        }
        let previous = self.take_tunnel(host);
        self.next_ssh_request += 1;
        let request = self.next_ssh_request;
        // Here rather than in [`Self::connect`]: for an ssh host the
        // attempt starts with the handshake, and most of the ways one
        // fails — no route, a refused port, a changed host key — never
        // reach a connect at all. Numbering it there would leave a
        // ten-rung ladder reporting one generation while the band
        // counts `(1/10)`…`(10/10)`.
        let generation = self.mint_generation(host);
        let raw_target = target.raw.clone();

        let feed = self.feed.clone();
        let host_id = host.to_string();
        let establish = self.runtime.spawn(async move {
            if let Some(previous) = previous {
                previous.shutdown().await;
            }
            let result = establish_tunnel(&host_id, &target).await;
            feed.send(crate::engine_feed::EngineFeed::HostTunnel(Box::new(
                HostTunnelReady {
                    host: host_id,
                    request,
                    result,
                },
            )));
        });

        let superseded = self.entry_mut(host).ssh.replace(SshState {
            target: raw_target,
            request,
            label: label.to_string(),
            mode,
            attended: cause == AttemptCause::Explicit && mode != ConnectMode::IfPresent,
            origin,
            cause,
            generation: Some(generation),
            lease: None,
            tunnel: None,
            establish: Some(establish.abort_handle()),
            seen: 0,
            failure: None,
            // Cleared with the entry, not carried: this is a fact about
            // *this* attempt.
            reached_connected: false,
        });
        self.park_displaced(superseded);
    }

    /// Keep hold of an establish whose entry has just gone, so the exit
    /// path can still reach it — see [`Self::displaced`]. Nothing here
    /// aborts anything: while the app runs, a displaced establish is
    /// left to land and be discarded.
    ///
    /// Finished handles are pruned on the way in, so a session that
    /// reconnects all day cannot grow this list.
    fn park_displaced(&mut self, entry: Option<SshState>) {
        let Some(establish) = entry.and_then(|entry| entry.establish) else {
            return;
        };
        self.displaced.retain(|handle| !handle.is_finished());
        if !establish.is_finished() {
            self.displaced.push(establish);
        }
    }

    /// Fold a finished establish back in, and dial if it came up.
    ///
    /// Returns the status line this attempt owes the user: `Some` only
    /// for a failure the user asked for. An unattended failure is the
    /// band's business alone, and a success says nothing — the state
    /// machine's own `Connecting` is already on its way.
    pub(crate) fn tunnel_ready(&mut self, ready: HostTunnelReady) -> Option<String> {
        let HostTunnelReady {
            host,
            request,
            result,
        } = ready;
        // Both stale paths hand the tunnel to [`Self::discard_tunnel`]
        // rather than letting it fall out of scope here: dropping one on
        // the UI thread would run its blocking teardown on the UI thread.
        match self.entries.get(&host).and_then(|entry| entry.ssh.as_ref()) {
            None => {
                tracing::debug!(%host, "dropped a tunnel establish for a host that is no longer reached over ssh");
                self.discard_tunnel(result);
                return None;
            }
            Some(entry) if entry.request != request => {
                tracing::debug!(%host, request, "dropped a superseded tunnel establish");
                self.discard_tunnel(result);
                return None;
            }
            Some(_) => {}
        }
        match result {
            Ok(tunnel) => {
                let entry = self
                    .entries
                    .get_mut(&host)
                    .and_then(|entry| entry.ssh.as_mut())
                    .expect("the entry checked above");
                let socket = tunnel.bridge_socket().to_path_buf();
                // The generation the band is already square with. A
                // per-connection exec that fails after this point bumps
                // past it, and that is what `apply_state` overlays.
                entry.seen = tunnel
                    .last_error()
                    .map_or(0, |recorded| recorded.generation);
                entry.failure = None;
                entry.tunnel = Some(tunnel);
                entry.establish = None;
                let (label, mode, cause) = (entry.label.clone(), entry.mode, entry.cause);
                tracing::info!(%host, socket = %socket.display(), "ssh tunnel established");
                self.connect(&host, &label, socket, HostTransport::Ssh, mode, cause);
                None
            }
            Err(failure) => {
                let reason = failure.message.clone();
                tracing::warn!(%host, %reason, "ssh tunnel could not be established");
                let mut disconnected = state::Disconnected {
                    reason: reason.clone(),
                    detail: None,
                    retry_in: None,
                };
                self.schedule_reconnect(
                    &host,
                    DropInput::Establish(&failure),
                    failure.truncated,
                    &mut disconnected,
                );
                // A failed *establish* publishes no `HostConnState` at
                // all, and `section_reason` prefers the live connection's
                // own — so without this the band would show attempt
                // one's reason for the whole ladder and neither `(2/10)`
                // nor the give-up copy would ever appear (§3.8).
                self.write_disconnected(&host, disconnected);
                let entry = self
                    .entries
                    .get_mut(&host)
                    .and_then(|entry| entry.ssh.as_mut())
                    .expect("the entry checked above");
                entry.establish = None;
                entry.failure = Some(failure);
                entry.attended.then_some(reason)
            }
        }
    }

    /// Why this host's band says what it says, when there is a reason
    /// worth naming.
    ///
    /// Four sources, in the order they can be current: a bootstrap
    /// running (or just finished) for this host, the live connection's
    /// own state, an establish that never reached one, and the retained
    /// section a disconnect left behind.
    pub(crate) fn section_reason(&self, host: &str) -> Option<&str> {
        let entry = self.entries.get(host)?;
        if let Some(note) = entry.bootstrap_note.as_ref() {
            return Some(note.as_str());
        }
        if let Some(conn) = entry.conn.as_ref() {
            return disconnected_reason(&conn.state);
        }
        if let Some(failure) = entry.ssh.as_ref().and_then(|ssh| ssh.failure.as_ref()) {
            return Some(failure.message.as_str());
        }
        disconnected_reason(&entry.retained.as_ref()?.state)
    }

    /// The long form behind [`Self::section_reason`], when the reason is
    /// a band line too short to carry what happened.
    ///
    /// Only the connection state has one — a settled localhost launch
    /// failure writes it (`task::spawn_failure`), and nothing else does
    /// — so unlike `section_reason` there is no ladder of sources here:
    /// a bootstrap note and an establish failure are already their own
    /// full text.
    pub(crate) fn section_detail(&self, host: &str) -> Option<&str> {
        self.section(host)?.state.detail()
    }

    /// Which connection attempt this host is on, as `host.status`
    /// reports it: the generation [`Self::mint_generation`] last handed
    /// it, `0` before the first. One per attempt *started* — an ssh
    /// establish that never comes up counts, which is what makes it an
    /// edge a poller can wait on across a whole retry ladder. See
    /// [`HostEntry::generation`] for why it survives a disconnect.
    pub(crate) fn generation(&self, host: &str) -> u64 {
        self.entries.get(host).map_or(0, |entry| entry.generation)
    }

    /// This host's armed retry, if one is waiting to fire.
    ///
    /// The ssh ladder answers first and in full: it is the only one that
    /// knows an attempt number, a budget, when the timer was armed, and
    /// — [`Outage::family`] — why.
    /// [`ReconnectLadder::next`](reconnect::ReconnectLadder::next) bumps
    /// its counter before it hands back the `attempt` [`Self::arm_retry`]
    /// writes into the band, so the number here is the same `3` the
    /// band's `(3/10)` shows.
    ///
    /// A localhost retry is the connection task's own backoff and its
    /// counter never leaves the task, so the delay is all there is —
    /// structurally, not by a branch: an [`Outage`] is only ever opened
    /// for an ssh host, so the fallthrough below cannot reach a family.
    pub(crate) fn retry_schedule(&self, host: &str) -> Option<RetrySchedule> {
        let entry = self.entries.get(host)?;
        if let Some(outage) = entry.outage.as_ref() {
            if let Some(armed) = outage.armed.as_ref() {
                return Some(RetrySchedule {
                    delay_ms: armed.delay.as_millis() as u64,
                    attempt: Some(outage.ladder.attempts()),
                    budget: Some(outage.ladder.budget()),
                    armed_at: Some(roost_engine::workspace::rfc3339(armed.at)),
                    reason: outage.family.clone(),
                });
            }
        }
        // An ssh host's retries live on the ladder alone. Its state's
        // `retry_in` mirrors the rung that was armed and outlives the
        // timer — the dead connection stays on the entry for the whole
        // establish that follows — so reading it here would report a
        // retry nothing holds.
        let conn = entry.conn.as_ref()?;
        if matches!(conn.transport, HostTransport::Ssh) {
            return None;
        }
        let delay = conn.state.retry_in()?;
        Some(RetrySchedule {
            delay_ms: delay.as_millis() as u64,
            ..RetrySchedule::default()
        })
    }

    /// The classified family behind this host's last ssh failure, if it
    /// was the far side that refused.
    ///
    /// The routing half of [`Self::section_reason`]'s middle rung: the
    /// band renders the message, and C5b decides whether to offer a
    /// bootstrap off the family — `NotFound` ("nothing to exec over
    /// there") and `NoSession` ("a binary, but nothing running") being
    /// the two that have an answer.
    pub(crate) fn ssh_failure(&self, host: &str) -> Option<&roost_ipc::ssh::SshFailure> {
        self.entries
            .get(host)?
            .ssh
            .as_ref()?
            .failure
            .as_ref()?
            .family
            .as_ref()
    }

    /// Who asked for this host's current ssh attempt.
    pub(crate) fn ssh_origin(&self, host: &str) -> Option<RequestOrigin> {
        self.entries
            .get(host)
            .and_then(|entry| Some(entry.ssh.as_ref()?.origin))
    }

    /// Whether this host's current ssh attempt ever reached a working
    /// connection. See [`SshState::reached_connected`] — it is what
    /// separates a first connect from a session dropping under the user.
    pub(crate) fn ssh_reached_connected(&self, host: &str) -> bool {
        self.entries
            .get(host)
            .is_some_and(|entry| entry.ssh.as_ref().is_some_and(|ssh| ssh.reached_connected))
    }

    /// This host's outage, created if this is the drop that starts one.
    ///
    /// This is where the lease moves house: [`SshState::lease`] is still
    /// the one the connection that just died held — `open_ssh` has not
    /// run again yet — so this is the last moment it can be copied
    /// somewhere the next attempt's `open_ssh` will not wipe (§3.7).
    pub(crate) fn begin_outage(&mut self, host: &str) -> &mut Outage {
        // Cloned before the entry is borrowed mutably, which is also
        // the order today's two maps forced.
        let lease = self
            .entries
            .get(host)
            .and_then(|entry| entry.ssh.as_ref()?.lease.clone());
        let outage = self.entry_mut(host).outage.get_or_insert_with(|| Outage {
            ladder: reconnect::ReconnectLadder::default(),
            lease: None,
            armed: None,
            family: None,
            dead_tunnel: None,
        });
        // Every drop, not only the first: an entry holding a lease is by
        // construction the connection that just died, so it supersedes
        // whatever the outage carries. `None` does not clear, because
        // that is what a retry which never reached `Connected` leaves
        // behind — and the outage's own lease is still the one to
        // present.
        if lease.is_some() {
            outage.lease = lease;
        }
        outage
    }

    /// The lease one attempt starts holding, which is the whole takeover
    /// guard (§3.7).
    ///
    /// A fresh task's `held_lease` is `None`, so an auto-reconnect that
    /// carried nothing would reconnect with `takeover: true` and no
    /// probe — silently taking the session back from another client
    /// whenever the drop *was* a takeover whose `session.stopping`
    /// envelope was lost. An explicit Connect carries nothing on
    /// purpose: taking the session back is exactly what that button
    /// means.
    fn carried_lease(&self, host: &str, cause: AttemptCause) -> Option<String> {
        match cause {
            AttemptCause::Explicit => None,
            AttemptCause::AutoReconnect => self.entries.get(host)?.outage.as_ref()?.lease.clone(),
        }
    }

    /// Drain one `EngineFeed::HostLease`: the lease a connection was
    /// granted, on its way to the entry that outlives the connection.
    ///
    /// Attributed through [`Self::owner_of`] like every other feed item
    /// — a lease minted by a connection this set has since replaced
    /// would otherwise become the one the *next* outage presents, which
    /// is a lease two connections old. Hosts not reached over ssh keep
    /// nothing: their task holds its own lease across its own retries,
    /// which is the case this entry exists to cover for ssh.
    ///
    /// The stamped generation is a *second* question, and the ssh path
    /// is where the two come apart: [`Self::open_ssh`] installs a fresh
    /// [`SshState`] while the old [`HostConn`] is still on the entry — an
    /// ssh connect does not reach [`Self::connect`], and so does not
    /// `forget`, until its tunnel is up an establish later. So "is this
    /// connection still current?" can be yes while "is this the attempt
    /// that opened it?" is no, and only the second keeps a lease from
    /// landing on the attempt an explicit connect just installed.
    pub(crate) fn apply_lease(&mut self, incarnation: HostId, lease: String) {
        let Some(host) = self.owner_of(incarnation) else {
            return;
        };
        let Some(minted) = self.minter.registration(incarnation) else {
            return;
        };
        let Some(entry) = self
            .entries
            .get_mut(&host)
            .and_then(|entry| entry.ssh.as_mut())
        else {
            return;
        };
        if entry.generation == Some(minted.generation) {
            entry.lease = Some(lease);
        }
    }

    /// Say what a bootstrap is doing to this host, or stop saying it.
    ///
    /// Progress *and* the failure copy go through here, because to the
    /// band they are the same thing — the most recent true sentence
    /// about a host nothing else is currently explaining. It outlives
    /// the job on purpose: a classified failure the user has not seen
    /// yet must not vanish the moment the job's task ends.
    pub(crate) fn set_bootstrap_note(&mut self, host: &str, note: Option<String>) {
        match note {
            Some(note) => self.entry_mut(host).bootstrap_note = Some(note),
            // Never `entry_mut`: clearing a note on a host this set has
            // never heard of has always been a no-op, and routing it
            // through the entry API would leave a phantom entry behind.
            None => {
                if let Some(entry) = self.entries.get_mut(host) {
                    entry.bootstrap_note = None;
                }
            }
        }
    }

    /// Replace a generic drop reason with the tunnel's own, when the
    /// tunnel has hit something newer than what has already been
    /// reported.
    ///
    /// This is what turns "the connection closed" into "the host key for
    /// box has CHANGED". The connection task only ever sees its end of a
    /// Unix socket: the `ssh` exec behind that socket is what failed, and
    /// its stderr is on the tunnel. The generation is what keeps the
    /// overlay honest — a tunnel bumps it once per exec, so an error from
    /// *before* this attempt started (already reported, or belonging to a
    /// connection that has since ended) can never be shown as the reason
    /// for this one.
    ///
    /// **Returns the family it folded in**, owned, because the retry
    /// decision is taken *around* this call rather than inside it: all
    /// three early returns below are ordinary outcomes, and the middle
    /// one — a tunnel with nothing recorded — is precisely the bare
    /// bridge EOF that is the headline retryable case (§3.4). A decision
    /// taken in here would never fire for the most common drop.
    fn overlay_ssh_reason(
        &mut self,
        host: &str,
        disconnected: &mut state::Disconnected,
    ) -> Option<(SshFailure, bool)> {
        let entry = self.entries.get_mut(host)?.ssh.as_mut()?;
        let recorded = entry.tunnel.as_ref().and_then(|t| t.last_error())?;
        if recorded.generation <= entry.seen {
            return None;
        }
        entry.seen = recorded.generation;
        let folded = (recorded.failure.clone(), recorded.truncated);
        let failure = ConnectFailure {
            truncated: recorded.truncated,
            ..ConnectFailure::classified(&entry.target, recorded.failure)
        };
        disconnected.reason = failure.message.clone();
        // Recorded as well as rendered: this is the *only* place a
        // per-connection exec's family — and whether it was read out of
        // complete evidence — reaches the app layer. It is the family a
        // NotFound/NoSession offer routes off (plan 039 §3.5), and the
        // one the retry ladder refuses to spend attempts on (plan 040
        // §3.3).
        entry.failure = Some(failure);
        Some(folded)
    }

    /// Decide what one drop means for this host's retry ladder, arm the
    /// timer when it means another attempt, and write §3.8's line.
    ///
    /// Two callers, one for each shape a drop has: `apply_state`'s
    /// `Disconnected` arm with the family [`Self::overlay_ssh_reason`]
    /// just folded in, and `tunnel_ready`'s `Err` arm with the whole
    /// [`ConnectFailure`]. The second is not optional — without it the
    /// ladder stalls on its first failed retry, which is the common
    /// case, because the network is usually still down.
    fn schedule_reconnect(
        &mut self,
        host: &str,
        input: DropInput<'_>,
        truncated: bool,
        disconnected: &mut state::Disconnected,
    ) {
        // What a give-up appends, when the failure had a family worth
        // naming. A bare EOF has none, and "gave up — the connection
        // closed" says nothing the first half did not.
        let family_copy = match input {
            DropInput::Session(Some(_)) => Some(disconnected.reason.clone()),
            DropInput::Establish(failure) if failure.family.is_some() => {
                Some(failure.message.clone())
            }
            _ => None,
        };

        if !self
            .entries
            .get(host)
            .is_some_and(|entry| entry.outage.is_some())
        {
            // §3.2's gates 1 and 2, read once here and never again:
            // `open_ssh` installs a fresh `SshState` with
            // `reached_connected: false` on every attempt, so re-reading
            // them on the second failure would find `false` and refuse —
            // the ladder would stop dead after one retry. From this point
            // the existence of the outage entry *is* the record of
            // eligibility.
            let eligible = self
                .entries
                .get(host)
                .is_some_and(|entry| entry.ssh.as_ref().is_some_and(|ssh| ssh.reached_connected));
            if !eligible || !reconnect::retryable(input, truncated) {
                return;
            }
        }
        // Outside the gate, not inside it: this creates the entry on the
        // drop that starts an outage *and* refreshes the lease it carries
        // on every later one. Called only under the gate the refresh is
        // dead code, and the case it exists for is real — an attempt
        // whose `session.connect` was granted a lease and whose prologue
        // then failed publishes that lease without ever reaching
        // `Connected`, so nothing clears the outage and its lease is a
        // tombstone from that moment on (§3.7).
        self.begin_outage(host);
        // `HostConn::drop` publishes its own `disconnect_requested()`,
        // and a late `Dropped` can still arrive under the *same*
        // generation — which `owner_of` does not filter. So a second
        // decision must not stack a second timer on the first, must
        // not let `apply_state`'s own `conn.state = next` overwrite the
        // armed retry's line with the bare reason it carried, and must
        // leave `Outage::family` alone: this second pass computed
        // `family_copy == None` (the overlay early-returned under
        // `seen`), so writing it here would erase the family the armed
        // rung is for.
        if let Some(outage) = self
            .entries
            .get(host)
            .and_then(|entry| entry.outage.as_ref())
        {
            if let Some(armed) = &outage.armed {
                *disconnected = retry_line(
                    armed.delay,
                    outage.ladder.attempts(),
                    outage.ladder.budget(),
                );
                return;
            }
        }
        // Cloned before `apply_state` hands the tunnel to
        // `shutdown_tunnel`: this is the last moment its failure slot is
        // reachable, and the timer re-reads it before it dials.
        let dead = self
            .entries
            .get(host)
            .and_then(|entry| entry.ssh.as_ref())
            .and_then(|entry| {
                Some(DeadTunnel::Tunnel {
                    tunnel: Arc::clone(entry.tunnel.as_ref()?),
                    seen: entry.seen,
                })
            });
        let Some(outage) = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.outage.as_mut())
        else {
            return;
        };
        if dead.is_some() {
            outage.dead_tunnel = dead;
        }
        let decision = outage.ladder.next(input, truncated, task::jitter());
        let budget = outage.ladder.budget();
        match decision {
            Decision::Retry { delay, attempt } => {
                // Assigned, not merged: a decision describes the failure
                // that caused it, so a rung armed by a bare EOF after
                // one armed by a refused port is honestly `None` —
                // merging would publish a cause this rung does not have.
                //
                // [`Self::restart_ladder`] carries the older copy
                // forward instead, and is not a counter-example to this:
                // it re-arms the *same* outage with no drop at all, so
                // there is no newer failure for the field to describe.
                outage.family = family_copy;
                *disconnected = self.arm_retry(host, delay, attempt, budget);
            }
            Decision::Exhausted { attempts } => {
                tracing::info!(%host, attempts, "ssh host reconnect gave up");
                disconnected.reason = gave_up_copy(attempts, family_copy.as_deref());
                disconnected.retry_in = None;
                // Nothing is armed in this state, so nothing else would
                // ever clean the entry — and the lease it holds belongs
                // to an outage that is over (§3.7).
                self.clear_outage(host);
            }
            // The band keeps the family's own copy: "must not be tried"
            // and "gave up trying" are different things to tell somebody
            // about a possible machine-in-the-middle.
            Decision::NonRetryable => self.clear_outage(host),
        }
    }

    /// Take a [`Decision::Retry`]: say so in the log, arm its timer, and
    /// hand back §3.8's band line.
    ///
    /// Both schedulers end here — the drop's own decision and the
    /// suspend reset's — so the event C5's lane asserts on is emitted in
    /// one place.
    fn arm_retry(
        &mut self,
        host: &str,
        delay: Duration,
        attempt: u32,
        budget: u32,
    ) -> state::Disconnected {
        tracing::info!(
            %host,
            attempt,
            delay_ms = delay.as_millis() as u64,
            "ssh host reconnect scheduled"
        );
        self.arm_reconnect(host, delay);
        retry_line(delay, attempt, budget)
    }

    /// Put one `ReconnectDue` on the feed after `delay`, replacing (and
    /// aborting) whatever this host had armed.
    ///
    /// The stamp is [`SshState::request`] read **now**. Reading it at
    /// arm time rather than storing it on the [`Outage`] is what lets
    /// the ladder survive its own re-entry: every `open_ssh` bumps the
    /// counter, so a stamp taken at outage creation could never match
    /// again (§3.4).
    fn arm_reconnect(&mut self, host: &str, delay: Duration) {
        let Some(request) = self
            .entries
            .get(host)
            .and_then(|entry| Some(entry.ssh.as_ref()?.request))
        else {
            return;
        };
        let feed = self.feed.clone();
        let due = host.to_string();
        let timer = self.runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            feed.send(crate::engine_feed::EngineFeed::ReconnectDue { host: due, request });
        });
        let Some(outage) = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.outage.as_mut())
        else {
            timer.abort();
            return;
        };
        if let Some(previous) = outage.armed.replace(Armed {
            handle: timer.abort_handle(),
            at: SystemTime::now(),
            delay,
        }) {
            previous.handle.abort();
        }
    }

    /// An armed retry came due. `Some` means dial — and *as what*: the
    /// caller re-enters through `App::host_reconnect_requested` with
    /// exactly this pair, which is the only door an auto-reconnect may
    /// use (§3.4).
    ///
    /// The pair is answered here rather than at the call site because
    /// both halves are this scheduler's own facts. The origin is the
    /// load-bearing consent gate (§3.6) — `Ipc`, because nobody asked —
    /// and the cause is what keeps the ladder from clearing the outage
    /// it is walking.
    pub(crate) fn reconnect_due(
        &mut self,
        host: &str,
        stamped: u64,
    ) -> Option<(RequestOrigin, AttemptCause)> {
        // `is_some_and`, never `is_none_or`: `disconnect` takes the
        // whole [`HostEntry::ssh`], and an `is_none_or` spelling would
        // let a fired timer resurrect a host the user just
        // disconnected.
        if !self
            .entries
            .get(host)
            .and_then(|entry| entry.ssh.as_ref())
            .is_some_and(|ssh| ssh.request == stamped)
        {
            tracing::debug!(%host, stamped, "dropped a superseded reconnect timer");
            return None;
        }
        // Consumption takes the handle, so no dead one lingers behind a
        // later "is a retry pending?" read.
        let Some(armed) = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.outage.as_mut()?.armed.take())
        else {
            tracing::debug!(%host, "a reconnect came due with nothing armed for it");
            return None;
        };
        // [`SUSPEND_SKEW`] is what this measures. `duration_since` errors
        // on a backward clock step, which is not a suspend and must not
        // panic — this runs on the UI thread, where a panic is the crash
        // report.
        let slept = SystemTime::now()
            .duration_since(armed.at)
            .unwrap_or(armed.delay);
        if slept.saturating_sub(armed.delay) > SUSPEND_SKEW {
            tracing::info!(
                %host,
                slept_ms = slept.as_millis() as u64,
                "an ssh host's retry ladder woke into a new outage"
            );
            // Reset and re-arm at the base delay rather than dialing: a
            // reset-then-dial spends a full `ConnectTimeout 15`
            // establish against a radio that is still associating, which
            // is precisely the attempt the reset exists to protect.
            self.restart_ladder(host);
            return None;
        }
        // The belt to C1's braces — see [`DeadTunnel`].
        if let Some((generation, family, truncated)) = self.late_family(host) {
            if !reconnect::retryable(DropInput::Session(Some(&family)), truncated) {
                self.settle_late(host, generation, family, truncated);
                return None;
            }
        }
        Some((RequestOrigin::Ipc, AttemptCause::AutoReconnect))
    }

    /// A failure the dead tunnel recorded since the drop, if it is news.
    fn late_family(&self, host: &str) -> Option<(u64, SshFailure, bool)> {
        self.entries
            .get(host)?
            .outage
            .as_ref()?
            .dead_tunnel
            .as_ref()?
            .late_failure()
    }

    /// A family found at fire time that no retry may be spent on: the
    /// host settles on that family's own copy instead of dialing.
    fn settle_late(&mut self, host: &str, generation: u64, family: SshFailure, truncated: bool) {
        let Some(entry) = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.ssh.as_mut())
        else {
            return;
        };
        let failure = ConnectFailure {
            truncated,
            ..ConnectFailure::classified(&entry.target, family)
        };
        tracing::warn!(
            %host,
            reason = %failure.message,
            "an ssh failure landed after the drop; the retry ladder settles instead of dialing"
        );
        entry.seen = generation;
        let reason = failure.message.clone();
        entry.failure = Some(failure);
        self.write_disconnected(
            host,
            state::Disconnected {
                reason,
                detail: None,
                retry_in: None,
            },
        );
        self.clear_outage(host);
    }

    /// Treat a woken ladder as a new outage: back to attempt one, at the
    /// base delay, with a fresh stamp — and no dial.
    ///
    /// [`Outage::family`] is carried, not cleared: nothing dropped here,
    /// so the last classified failure is still the honest answer to why
    /// this host is retrying (plan 044 §3.3). That is the opposite of
    /// the assignment at a `Decision::Retry`, and deliberately so — a
    /// decision has a *new* failure to describe and must not merge an
    /// old one, whereas a reset has no failure of its own and is still
    /// the same outage.
    fn restart_ladder(&mut self, host: &str) {
        let Some(outage) = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.outage.as_mut())
        else {
            return;
        };
        outage.ladder.reset();
        // `Session(None)` is the bare-EOF row, which is always
        // retryable: the only question here is how long until the next
        // attempt, and the ladder has just been reset to answer it with
        // the base delay.
        let decision = outage
            .ladder
            .next(DropInput::Session(None), false, task::jitter());
        let budget = outage.ladder.budget();
        let Decision::Retry { delay, attempt } = decision else {
            return;
        };
        let line = self.arm_retry(host, delay, attempt, budget);
        self.write_disconnected(host, line);
    }

    /// Write a disconnected line onto the connection the band reads.
    ///
    /// Only over one that is already disconnected. The dead `HostConn` a
    /// drop leaves on the entry is what `section_reason` prefers and what
    /// the ladder has to keep current; a connection that is still
    /// *serving* is a different thing, and an establish failing beside
    /// it (a ↻ on a connected host) is its own task's news to publish.
    fn write_disconnected(&mut self, host: &str, disconnected: state::Disconnected) {
        if let Some(conn) = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.conn.as_mut())
        {
            if matches!(conn.state, HostConnState::Disconnected(_)) {
                conn.state = HostConnState::Disconnected(disconnected);
            }
        }
    }

    /// End this host's outage: the ladder, the lease, the dead tunnel
    /// and any armed timer go together.
    fn clear_outage(&mut self, host: &str) {
        // `get_mut`, never `entry_mut`: ending an outage a host does
        // not have must not conjure an entry for it.
        if let Some(Outage {
            armed: Some(armed), ..
        }) = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.outage.take())
        {
            armed.handle.abort();
        }
    }

    /// Stop everything this set has in flight that could still dial.
    ///
    /// The exit path's, and it needs both halves (§3.4). Aborting the
    /// armed handles covers the waiting window; the establish
    /// [`Self::open_ssh`] spawned is the other one — a quit timed into
    /// it would leave a just-daemonized `ControlPersist=60s` master
    /// outliving the app, which is exactly what the lane's
    /// zero-`ssh`-children check measures.
    ///
    /// [`Self::displaced`] is part of that second half: an establish
    /// whose entry has gone is invisible to the walk over
    /// [`HostEntry::ssh`], and nothing will drain its answer once the
    /// app is on its way out.
    pub(crate) fn abandon_reconnects(&mut self) {
        for (host, entry) in self.entries.iter_mut() {
            if let Some(armed) = entry.outage.take().and_then(|outage| outage.armed) {
                tracing::debug!(%host, "aborting an armed reconnect on the way out");
                armed.handle.abort();
            }
            if let Some(establish) = entry.ssh.as_mut().and_then(|ssh| ssh.establish.take()) {
                tracing::debug!(%host, "aborting an in-flight ssh establish on the way out");
                establish.abort();
            }
        }
        for establish in self.displaced.drain(..) {
            if !establish.is_finished() {
                tracing::debug!("aborting a displaced ssh establish on the way out");
                establish.abort();
            }
        }
    }

    /// Retire an establish's answer because the app is on its way out.
    ///
    /// [`crate::engine_feed::EngineFeed::Quit`] only latches the exit —
    /// the drain keeps running — so an answer queued behind it still
    /// reaches the UI thread. Handing it to [`Self::tunnel_ready`] there
    /// would dial a fresh connection while the app tears down, and
    /// [`Self::abandon_reconnects`] cannot undo that: the establish
    /// handle is already spent, and the connection task `connect` spawns
    /// is not one this set tracks.
    ///
    /// Discarding rather than aborting is the same rule
    /// [`Self::discard_tunnel`] carries: a tunnel that *did* come up
    /// holds a live `ssh` master and a scratch directory, and only
    /// `shutdown` retires both.
    pub(crate) fn discard_ready(&mut self, ready: HostTunnelReady) {
        tracing::debug!(
            host = %ready.host,
            request = ready.request,
            "discarding a tunnel establish that answered during shutdown"
        );
        if let Some(entry) = self
            .entries
            .get_mut(&ready.host)
            .and_then(|entry| entry.ssh.as_mut())
        {
            if entry.request == ready.request {
                entry.establish = None;
            }
        }
        self.discard_tunnel(ready.result);
    }

    /// Retire a tunnel whose answer nobody is waiting for any more.
    ///
    /// An establish that lands after its host was disconnected, or after
    /// a second Connect superseded it, still came back holding a live
    /// `ssh` master and a bound `bridge.sock`. Dropping that `Arc` here
    /// would run [`SshTunnel`]'s *blocking* `Drop` — a `-O exit` round
    /// trip — on the UI thread. Shutting it down on the engine runtime
    /// instead leaves `Drop` with nothing to do, and is also what retires
    /// its scratch directory rather than leaving one behind.
    ///
    /// A failed establish carries nothing to clean up: there is no tunnel
    /// on that arm.
    fn discard_tunnel(&self, result: Result<Arc<SshTunnel>, ConnectFailure>) {
        if let Ok(tunnel) = result {
            self.runtime.spawn(async move { tunnel.shutdown().await });
        }
    }

    /// Tear down a host's tunnel, if it has one. The entry stays — its
    /// `failure` is what the band reads, and its `request` is what a
    /// still-in-flight establish is dropped against.
    ///
    /// The shutdown runs on the engine runtime rather than here: it is a
    /// `-O exit` round trip plus a drain of in-flight connections, and
    /// `SshTunnel`'s own `Drop` does that *blocking*. Awaiting it there
    /// would be the UI thread waiting on ssh; running it here means Drop
    /// finds the tunnel already closed and returns immediately.
    fn shutdown_tunnel(&mut self, host: &str) {
        let Some(tunnel) = self.take_tunnel(host) else {
            return;
        };
        self.runtime.spawn(async move { tunnel.shutdown().await });
    }

    /// Detach a host's tunnel from the set, leaving the entry behind.
    /// The caller owns the shutdown from here — either spawned
    /// ([`Self::shutdown_tunnel`]) or awaited ahead of a replacement
    /// ([`Self::open_ssh`]).
    fn take_tunnel(&mut self, host: &str) -> Option<Arc<SshTunnel>> {
        let tunnel = self.entries.get_mut(host)?.ssh.as_mut()?.tunnel.take()?;
        tracing::debug!(%host, "tearing down an ssh tunnel");
        Some(tunnel)
    }

    /// Stop driving a host. Disconnect is never Stop: the session keeps
    /// running, and its tabs with it.
    ///
    /// Returns the incarnation that was live, so the caller can purge
    /// the app state keyed on it (attach machinery, client terminals,
    /// inbox rows) — a disconnect with no reconnect never publishes a
    /// `Connecting { previous }` for consumers to purge off. C7's
    /// disconnect verb is the caller.
    ///
    /// The **rows** are the one thing that does not go: the shells they
    /// name are still running over there, so the last mirror is retained
    /// and the section renders dimmed rather than empty (§3.1). That is
    /// the same rule a *dropped* connection already follows; an explicit
    /// disconnect only has to say it out loud, because it is the path
    /// that removes the `HostConn` the rows would otherwise hang off.
    pub(crate) fn disconnect(&mut self, host: &str) -> Option<HostId> {
        // Before the early return: a host whose establish is still in
        // flight has no `HostConn` yet, and it is exactly the one whose
        // tunnel would otherwise be left holding an `ssh` master for a
        // connection nobody asked for any more. The entry goes with it —
        // an explicit disconnect ends this transport's life, so the
        // in-flight establish is dropped when it lands and the next
        // Connect opens a fresh tunnel.
        self.shutdown_tunnel(host);
        let removed = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.ssh.take());
        self.park_displaced(removed);
        // Being reconnected eight seconds after asking to disconnect is
        // the one outcome nobody wants, and the lease the entry holds
        // has no owner left either.
        self.clear_outage(host);
        let Some(conn) = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.conn.take())
        else {
            self.minter.forget_host(host);
            return None;
        };
        let incarnation = conn.incarnation;
        // A host that never finished connecting published no rows, so
        // there is nothing to keep and its section lists none.
        if let Some((incarnation, mirror)) = incarnation
            .and_then(|incarnation| Some((incarnation, self.mirrors.remove(&incarnation)?)))
        {
            let retained = RetainedSection {
                label: conn.label.clone(),
                incarnation,
                mirror,
                state: HostConnState::Disconnected(state::Disconnected {
                    reason: "disconnected".into(),
                    detail: None,
                    retry_in: None,
                }),
            };
            // Not an `entry_mut` site: the entry exists by
            // construction — `conn` was just taken out of it.
            if let Some(entry) = self.entries.get_mut(host) {
                entry.retained = Some(retained);
            }
        }
        // `Drop` signals the task's own shutdown, which closes its queue
        // and answers everything still on it with `Disconnected`.
        drop(conn);
        self.minter.forget_host(host);
        incarnation
    }

    /// Forget a host entirely — the connection, and the rows it left
    /// behind. `host.remove`'s half of the disconnect (the registry
    /// entry is the app's to drop).
    pub(crate) fn remove(&mut self, host: &str) -> Option<HostId> {
        let incarnation = self.disconnect(host);
        // The only entry removal in the set. After `disconnect` the
        // entry holds nothing but the retained section, the generation
        // and the bootstrap note — exactly the three this used to clear
        // one map at a time.
        self.entries.remove(host);
        incarnation
    }

    /// Drop the connection and everything keyed on its incarnation,
    /// retained rows included.
    ///
    /// This is the reconnect path, and it purges rather than retains on
    /// purpose: the fresh `tab.list` is authoritative, so a reconnect is
    /// purge-then-rebuild and never a merge (§3.2).
    fn forget(&mut self, host: &str) {
        if let Some(conn) = self
            .entries
            .get_mut(host)
            .and_then(|entry| entry.conn.take())
        {
            if let Some(incarnation) = conn.incarnation {
                self.mirrors.remove(&incarnation);
            }
            // `Drop` notifies and aborts.
        }
        if let Some(entry) = self.entries.get_mut(host) {
            entry.retained = None;
        }
        self.minter.forget_host(host);
    }

    fn purge(&mut self, incarnation: HostId) {
        self.mirrors.remove(&incarnation);
        self.minter.forget_id(incarnation);
    }

    /// The op queue for a host, by saved id.
    pub(crate) fn ops(&self, host: &str) -> Option<&HostOps> {
        self.entries
            .get(host)
            .and_then(|entry| Some(&entry.conn.as_ref()?.ops))
    }

    /// Enqueue one control-plane op on a host's queue.
    ///
    /// The single dispatch point plan 037 §3.9 calls for on the host
    /// side: C6/C7's mutation call sites route a host-owned intent here
    /// and a local one to `LocalClient` as today. A host that is not
    /// connected answers the intent rather than dropping it, so a caller
    /// awaiting the reply never waits forever.
    pub(crate) fn send(&self, host: &str, intent: queue::HostIntent) -> Result<(), HostOpError> {
        match self.ops(host) {
            Some(ops) => ops.send(intent),
            None => {
                intent.answer(Err(HostOpError::Unavailable));
                Err(HostOpError::Unavailable)
            }
        }
    }

    /// [`Self::send`] addressed by connection incarnation — the form the
    /// UI's own `TabKey`/`ProjectKey` already carry, so a call site
    /// acting on the selection never has to look the saved id back up.
    /// A stale incarnation answers the intent rather than dropping it,
    /// same contract as [`Self::send`].
    pub(crate) fn send_at(
        &self,
        incarnation: HostId,
        intent: queue::HostIntent,
    ) -> Result<(), HostOpError> {
        match self.owner_of(incarnation) {
            Some(host) => self.send(&host, intent),
            None => {
                intent.answer(Err(HostOpError::Unavailable));
                Err(HostOpError::Unavailable)
            }
        }
    }

    /// The op queue for whichever host owns this incarnation.
    pub(crate) fn ops_for(&self, incarnation: HostId) -> Option<&HostOps> {
        let host = self.owner_of(incarnation)?;
        self.ops(&host)
    }

    pub(crate) fn state(&self, host: &str) -> Option<&HostConnState> {
        self.entries
            .get(host)
            .and_then(|entry| Some(&entry.conn.as_ref()?.state))
    }

    /// Whether an ssh establish is still in flight for this host — the
    /// window where there is no `HostConn` yet but the attempt is very
    /// much under way. The band and the `host.connect` reply both read
    /// it as `connecting`; without it that window looks `disconnected`,
    /// which is a wrong answer to hand a caller who just asked to
    /// connect.
    pub(crate) fn establishing(&self, host: &str) -> bool {
        self.entries.get(host).is_some_and(|entry| {
            entry.conn.is_none()
                && entry
                    .ssh
                    .as_ref()
                    .is_some_and(|ssh| ssh.tunnel.is_none() && ssh.failure.is_none())
        })
    }

    /// The incarnation currently serving a saved host, if it is
    /// connected.
    pub(crate) fn incarnation(&self, host: &str) -> Option<HostId> {
        self.entries
            .get(host)
            .and_then(|entry| entry.conn.as_ref()?.incarnation)
    }

    /// The live mirror for an incarnation. C6/C7 read it through
    /// [`SharedMirror::read`] at draw time; there is no per-commit copy
    /// to hold on to.
    pub(crate) fn mirror(&self, incarnation: HostId) -> Option<&Arc<SharedMirror>> {
        self.mirrors.get(&incarnation)
    }

    /// What one saved host's sidebar section renders from.
    ///
    /// `None` only for a host that has never published anything — never
    /// connected, or removed — whose section renders as disconnected
    /// with no rows.
    ///
    /// The mirror deliberately outlives both a *drop* and an explicit
    /// *disconnect*: those shells are still running on the host, so the
    /// section keeps listing them dimmed until the connection is back.
    /// It does not outlive a *reconnect* — `Connecting { previous }`
    /// purges it and the fresh `tab.list` rebuilds, which is §3.2's
    /// purge-then-rebuild.
    pub(crate) fn section(&self, host: &str) -> Option<HostSectionView<'_>> {
        let entry = self.entries.get(host)?;
        if let Some(conn) = entry.conn.as_ref() {
            return Some(HostSectionView {
                label: conn.label.as_str(),
                state: &conn.state,
                incarnation: conn.incarnation,
                mirror: conn
                    .incarnation
                    .and_then(|incarnation| self.mirrors.get(&incarnation)),
            });
        }
        let retained = entry.retained.as_ref()?;
        Some(HostSectionView {
            label: retained.label.as_str(),
            state: &retained.state,
            incarnation: Some(retained.incarnation),
            mirror: Some(&retained.mirror),
        })
    }

    /// Connected hosts, as `(saved id, label, incarnation, mirror)`.
    /// The sidebar iterates saved hosts through [`Self::section`]
    /// instead — this is the connected-only view the palette verbs take.
    pub(crate) fn connected(
        &self,
    ) -> impl Iterator<Item = (&str, &str, HostId, &Arc<SharedMirror>)> {
        self.entries.iter().filter_map(|(host, entry)| {
            let conn = entry.conn.as_ref()?;
            let incarnation = conn.incarnation.filter(|_| conn.state.is_connected())?;
            let mirror = self.mirrors.get(&incarnation)?;
            Some((host.as_str(), conn.label.as_str(), incarnation, mirror))
        })
    }

    /// Where a saved host lives, and whether it is this machine's own.
    pub(crate) fn endpoint(&self, host: &str) -> Option<(&std::path::Path, bool)> {
        self.entries.get(host).and_then(|entry| {
            let conn = entry.conn.as_ref()?;
            Some((conn.socket.as_path(), conn.transport.is_localhost()))
        })
    }

    /// The socket a live incarnation's data connections dial — owned, so
    /// an attach task can carry it off the main thread. `None` for a
    /// stale incarnation, same contract as [`Self::ops_for`].
    pub(crate) fn endpoint_for(&self, incarnation: HostId) -> Option<std::path::PathBuf> {
        let host = self.owner_of(incarnation)?;
        self.endpoint(&host).map(|(socket, _)| socket.to_path_buf())
    }

    /// The theme changed. The slot every task re-reads on reconnect is
    /// updated first, then every connected host is told — so a session
    /// that reconnects during the round trip still gets the new colors.
    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        let colors = theme_colors(theme);
        {
            let mut slot = self
                .theme
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = colors.clone();
        }
        for conn in self
            .entries
            .values()
            .filter_map(|entry| entry.conn.as_ref())
        {
            // Lease-gated, and it rides the same queue as everything
            // else so it cannot interleave with an attach.
            let _ = conn.ops.send(
                queue::HostIntent::new(
                    ops::SESSION_SET_THEME,
                    serde_json::json!({ "osc_colors": colors }),
                )
                .with_lease(),
            );
        }
    }

    /// Ask one host to bring its agent hooks in line with this client's
    /// config (plan 046 §3.4).
    ///
    /// **Queued, not chained.** It rides the ordinary op queue — the same
    /// road `session.set_theme` and `session.set_focus` take — rather
    /// than joining the connect chain in `task::attempt`, and the two
    /// reasons are both about the attachment: an error there fails the
    /// whole attempt, and an ensure on a network-mounted `$HOME` would
    /// hold hydration up behind file I/O this client is not waiting on.
    /// So the reply comes back on the feed, where a failure costs a log
    /// line and a toast that does not appear.
    ///
    /// Sent on **every** connect with the client's own values, because
    /// the op is idempotent and a config edit since the last connect has
    /// no other way to reach the host.
    pub(crate) fn wire_agent_hooks(
        &self,
        host: &str,
        mode: roost_ipc::messages::AgentHooksMode,
        skip: &[String],
        client: &str,
    ) {
        let Some(conn) = self.entries.get(host).and_then(|entry| entry.conn.as_ref()) else {
            return;
        };
        let label = conn.label.clone();
        let reply = conn.ops.call(
            ops::SESSION_SET_AGENT_HOOKS,
            serde_json::json!({ "mode": mode, "skip": skip, "client": client }),
            true,
        );
        let feed = self.feed.clone();
        self.runtime.spawn(async move {
            let outcome = reply.await.and_then(|value| {
                serde_json::from_value(value).map_err(|error| {
                    // A reply this client cannot read is the host's
                    // answer all the same, so it is reported as a
                    // refusal rather than dropped.
                    HostOpError::Rejected {
                        code: roost_ipc::client::ServerCode::Internal,
                        message: format!(
                            "undecodable {} reply: {error}",
                            ops::SESSION_SET_AGENT_HOOKS
                        ),
                    }
                })
            });
            feed.send(crate::engine_feed::EngineFeed::HostAgentHooks(Box::new(
                crate::app::agent_hooks::HostAgentHooks { label, outcome },
            )));
        });
    }

    /// Tell every connected host which of its tabs this client is
    /// looking at — `claim` is the one host tab that is on screen in a
    /// focused window, if any, so at most one host hears a tab and every
    /// other hears null.
    ///
    /// Null is not silence: a session defaults to believing itself
    /// focused on its own restored tab, so a host that is never told
    /// keeps muting that tab's notifications. The dedup below is
    /// therefore only about the *repeat* — the first statement to each
    /// incarnation always goes out (`focus_sent` starts empty and is
    /// cleared with the incarnation).
    ///
    /// Fire-and-forget and quiet: a session one release older answers
    /// `unknown-op`, which the connection tolerates, and the task logs
    /// once per incarnation rather than warning per call.
    pub(crate) fn set_focus(&mut self, claim: Option<TabKey>) {
        for conn in self
            .entries
            .values_mut()
            .filter_map(|entry| entry.conn.as_mut())
        {
            let Some(incarnation) = conn.incarnation.filter(|_| conn.state.is_connected()) else {
                continue;
            };
            let focused = claim
                .filter(|tab| tab.host == incarnation)
                .map(|tab| tab.tab);
            if conn.focus_sent == Some(focused) {
                continue;
            }
            let sent = conn.ops.send(
                queue::HostIntent::new(
                    ops::SESSION_SET_FOCUS,
                    // A string id or JSON null — the field is required
                    // on the wire, so an absent one would be refused.
                    serde_json::json!({ "focused_tab_id": focused.map(|id| id.to_string()) }),
                )
                .with_lease()
                .quiet(),
            );
            // Recorded only once it is actually on the queue: an intent
            // refused at the enqueue never reaches the session, and
            // remembering it as sent would leave that host muted.
            if sent.is_ok() {
                conn.focus_sent = Some(focused);
            }
        }
    }

    /// Whether the session's active row moved away from what this client
    /// claimed. A lease-less third party (`tab.focus`, `tab.open`) can
    /// park the selection on a tab nobody watches — with the claim's
    /// `window_focused` still standing, that tab would be muted at the
    /// source until this client's next natural edge. A disagreement
    /// clears the dedup so the caller's re-push actually goes out; the
    /// echo of this client's own `set_focus` matches the claim and
    /// changes nothing.
    pub(crate) fn focus_claim_disagrees(&mut self, incarnation: HostId, tab_id: i64) -> bool {
        let Some(host) = self.owner_of(incarnation) else {
            return false;
        };
        let Some(conn) = self
            .entries
            .get_mut(&host)
            .and_then(|entry| entry.conn.as_mut())
        else {
            return false;
        };
        match conn.focus_sent {
            Some(claim) if claim != Some(tab_id) => {
                conn.focus_sent = None;
                true
            }
            _ => false,
        }
    }

    /// Drain one `EngineFeed::HostState`. Returns the saved host it
    /// belongs to, or `None` when the incarnation is stale — an item
    /// minted by a connection this set has since dropped.
    pub(crate) fn apply_state(
        &mut self,
        incarnation: HostId,
        mut next: HostConnState,
    ) -> Option<String> {
        // Attribution first: a `Connecting` from a replaced connection
        // must not purge the live one's mirror on its way to being
        // dropped.
        let host = self.owner_of(incarnation)?;
        // Stamped once and never cleared until the next `open_ssh`: from
        // here on, every drop for this attempt is a *session going away*
        // rather than a connect that never worked, and the bootstrap
        // offer turns on exactly that difference.
        if next.is_connected() {
            if let Some(ssh) = self
                .entries
                .get_mut(&host)
                .and_then(|entry| entry.ssh.as_mut())
            {
                ssh.reached_connected = true;
            }
        }
        match &next {
            // The outage is over, so the next one starts at the base
            // delay — and the lease this one carried has been superseded
            // by the one the fresh connection is about to publish.
            HostConnState::Connected
            // Terminal in the machine, and nothing is armed in any of
            // them: if the entry did not go here, nothing would ever
            // clean it up (§3.4).
            | HostConnState::TakenOver
            | HostConnState::Stopped
            | HostConnState::NeedsRestart(_) => self.clear_outage(&host),
            HostConnState::Connecting { .. } | HostConnState::Disconnected(_) => {}
        }
        if let HostConnState::Disconnected(disconnected) = &mut next {
            let folded = self.overlay_ssh_reason(&host, disconnected);
            let truncated = folded.as_ref().is_some_and(|(_, truncated)| *truncated);
            let family = folded.as_ref().map(|(family, _)| family);
            // Around the overlay, not inside it (§3.4), and before the
            // teardown below — which is what takes the tunnel the
            // decision clones for its fire-time re-check.
            self.schedule_reconnect(&host, DropInput::Session(family), truncated, disconnected);
            // A tunnel that has served a connection through to
            // Disconnected has nothing left to serve: an auto-reconnect
            // re-enters at `open_ssh` and opens a fresh one, so this
            // mux — which may be exactly what wedged — is retired here.
            self.shutdown_tunnel(&host);
        }
        // The reconnect contract: purge the dead incarnation the moment
        // the new attempt starts, so nothing keyed on it survives into
        // the rebuild that follows.
        if let HostConnState::Connecting {
            previous: Some(previous),
        } = &next
        {
            self.purge(*previous);
        }

        let conn = self.entries.get_mut(&host)?.conn.as_mut()?;
        // A different incarnation, or one that is no longer connected,
        // knows nothing about what it was told before: the queue behind
        // it was flushed, and a session that comes back is back on its
        // headless default. Clearing here is what makes a reconnect
        // re-assert the client's focus instead of deduping it away.
        if conn.incarnation != Some(incarnation) || !next.is_connected() {
            conn.focus_sent = None;
        }
        conn.incarnation = Some(incarnation);
        conn.state = next;
        Some(host)
    }

    /// Drain one `EngineFeed::HostWorkspace`.
    ///
    /// Only a reset touches this set: the task writes the mirror in
    /// place, so an applied batch is a wake plus the envelopes C5 routes
    /// off the feed item itself. Nothing here needs the batch, and
    /// nothing here may hold it — a per-commit copy is exactly the
    /// unbounded growth the shared mirror exists to avoid.
    pub(crate) fn apply_workspace(&mut self, incarnation: HostId, event: HostWorkspaceEvent) {
        if self.owner_of(incarnation).is_none() {
            return;
        }
        if let HostWorkspaceEvent::Reset(mirror) = event {
            self.mirrors.insert(incarnation, mirror);
        }
    }

    /// Whether an incarnation is still the live connection for its host
    /// — [`Self::owner_of`]'s question, asked by a caller that has to
    /// decide *before* touching anything.
    ///
    /// The mirror is attributed inside [`Self::apply_workspace`], but a
    /// batch also carries envelopes that reach surfaces no later purge
    /// can take back: a desktop banner, the clipboard, the notification
    /// inbox. Those are applied by the drain, so the drain needs the
    /// attribution first — a batch queued by a connection that has since
    /// been removed or replaced must fire nothing at all.
    pub(crate) fn owns(&self, incarnation: HostId) -> bool {
        self.owner_of(incarnation).is_some()
    }

    /// Which live connection an incarnation belongs to. `None` for one
    /// this set no longer holds — the stale-key drop pattern
    /// (`app.rs:2903`), applied to whole connections.
    ///
    /// "No longer holds" covers two cases, and the second is the subtle
    /// one: the host may be gone, or it may have been *replaced* while
    /// the previous task was still winding down. Both are stale, and
    /// both are dropped here rather than landing on the replacement.
    fn owner_of(&self, incarnation: HostId) -> Option<String> {
        let registration = self.minter.registration(incarnation)?;
        let Some(conn) = self
            .entries
            .get(&registration.host)
            .and_then(|entry| entry.conn.as_ref())
        else {
            tracing::debug!(
                incarnation = incarnation.raw(),
                host = %registration.host,
                "dropping an item from a host this app no longer holds"
            );
            return None;
        };
        if conn.generation != registration.generation {
            tracing::debug!(
                incarnation = incarnation.raw(),
                host = %registration.host,
                "dropping an item from a replaced connection"
            );
            return None;
        }
        Some(registration.host)
    }

    /// A fresh incarnation for a host's *current* connection, as its
    /// task would mint one.
    #[cfg(test)]
    fn mint_for(&self, host: &str) -> HostId {
        self.minter.mint(host, self.conn(host).generation)
    }

    /// The suite's reads into one host's entry, named for the maps they
    /// replace. Each panics exactly where the `HashMap` index it stands
    /// in for did — an assertion about a host with no entry is a broken
    /// test, not a `None`.
    #[cfg(test)]
    fn ssh(&self, host: &str) -> &SshState {
        self.entries[host]
            .ssh
            .as_ref()
            .expect("an ssh entry for the host")
    }

    #[cfg(test)]
    fn has_ssh(&self, host: &str) -> bool {
        self.entries
            .get(host)
            .is_some_and(|entry| entry.ssh.is_some())
    }

    #[cfg(test)]
    fn outage(&self, host: &str) -> &Outage {
        self.entries[host]
            .outage
            .as_ref()
            .expect("an outage for the host")
    }

    #[cfg(test)]
    fn outage_mut(&mut self, host: &str) -> &mut Outage {
        self.entries
            .get_mut(host)
            .expect("an entry for the host")
            .outage
            .as_mut()
            .expect("an outage")
    }

    #[cfg(test)]
    pub(crate) fn has_outage(&self, host: &str) -> bool {
        self.entries
            .get(host)
            .is_some_and(|entry| entry.outage.is_some())
    }

    #[cfg(test)]
    fn conn(&self, host: &str) -> &HostConn {
        self.entries[host]
            .conn
            .as_ref()
            .expect("a connection for the host")
    }

    #[cfg(test)]
    fn conn_mut(&mut self, host: &str) -> &mut HostConn {
        self.entries
            .get_mut(host)
            .expect("an entry for the host")
            .conn
            .as_mut()
            .expect("conn")
    }

    #[cfg(test)]
    pub(crate) fn has_conn(&self, host: &str) -> bool {
        self.entries
            .get(host)
            .is_some_and(|entry| entry.conn.is_some())
    }

    /// The same two reads, as values rather than references.
    ///
    /// [`SshState`] and [`Outage`] are this module's own types, so a
    /// suite outside it — `app::host_lifecycle`'s, driving the app's
    /// lifted edges against a real set — cannot spell
    /// `set.ssh(host).request`. It asks for the number instead.
    #[cfg(test)]
    pub(crate) fn ssh_request(&self, host: &str) -> u64 {
        self.ssh(host).request
    }

    #[cfg(test)]
    pub(crate) fn outage_armed(&self, host: &str) -> bool {
        self.outage(host).armed.is_some()
    }
}

/// A palette of nothing, for tests that only care about the plumbing.
#[cfg(test)]
pub(crate) fn blank_theme() -> OscColorsParams {
    OscColorsParams {
        foreground: "#000000".into(),
        background: "#000000".into(),
        cursor: "#000000".into(),
        palette: vec!["#000000".into(); 256],
    }
}

/// The suite's own scaffolding, shared with the sibling suites that
/// drive this set through the app's lifted edges
/// ([`crate::app::host_lifecycle`]). Promoted out of `mod tests`
/// unchanged — every body here is the one the inline cases were
/// written against.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// A set on the test's own runtime. The receiver comes back with it
    /// so the caller keeps the feed alive: a dropped receiver makes
    /// every task's first publish fail, which would end the connections
    /// these cases are driving by hand.
    pub(crate) fn a_set() -> (HostConnSet, crate::engine_feed::EngineFeedReceiver) {
        let (feed, rx) = crate::engine_feed::channel();
        let set = HostConnSet::new(
            tokio::runtime::Handle::current(),
            feed,
            &Theme::roost_dark(),
        );
        (set, rx)
    }

    /// Every ssh case below drives the *failed* half of an establish,
    /// which needs no `ssh` at all: the tunnel object only exists on the
    /// success path, and the process choreography behind it is pinned by
    /// `roost_ipc`'s own fake-ssh tests. What is C4's to prove is the
    /// handoff — who the answer belongs to, and what it leaves on the
    /// band.
    ///
    /// **The invariant that keeps that true**: `open_ssh` spawns a task
    /// that would exec the real `ssh` binary, and these cases rely on it
    /// never being polled. `#[tokio::test]` is single-threaded by
    /// default, so a spawned task only runs when the test awaits, and
    /// none of them awaits after an `open_ssh`. Converting any of them to
    /// `#[tokio::test(flavor = "multi_thread")]` — or adding an `.await`
    /// between the `open_ssh` and the `tunnel_ready` that answers it —
    /// would silently start dialing hosts from the unit suite.
    pub(crate) fn ssh_target(raw: &str) -> roost_ipc::ssh::SshTarget {
        match roost_ipc::ssh::classify(raw).expect("classify an ssh target") {
            roost_ipc::ssh::ResolvedTransport::Ssh(target) => target,
            other => panic!("{raw:?} is not an ssh target: {other:?}"),
        }
    }

    pub(crate) fn failed(host: &str, request: u64, reason: &str) -> HostTunnelReady {
        HostTunnelReady {
            host: host.to_string(),
            request,
            result: Err(ConnectFailure::unclassified(reason)),
        }
    }

    /// A classified establish failure, as the transport hands one back.
    pub(crate) fn refused(
        host: &str,
        request: u64,
        failure: roost_ipc::ssh::SshFailure,
    ) -> HostTunnelReady {
        HostTunnelReady {
            host: host.to_string(),
            request,
            result: Err(ConnectFailure::classified("workbox", failure)),
        }
    }

    pub(crate) fn dropped(reason: &str) -> HostConnState {
        HostConnState::Disconnected(state::Disconnected {
            reason: reason.into(),
            detail: None,
            retry_in: None,
        })
    }

    /// An ssh host walked to `Connected` by an explicit connect — the
    /// only state that mints a lease, and where each case below starts.
    /// The cause a case turns on is the one it passes *after* this.
    pub(crate) fn a_connected_ssh_host(set: &mut HostConnSet, socket: &str) -> HostId {
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        set.connect(
            "h1",
            "one",
            PathBuf::from(socket),
            HostTransport::Ssh,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let incarnation = set.mint_for("h1");
        set.apply_state(incarnation, HostConnState::Connected);
        incarnation
    }

    pub(crate) fn band_reason(set: &HostConnSet, host: &str) -> String {
        set.section_reason(host)
            .expect("a disconnected host has a reason")
            .to_string()
    }

    /// The budget this outage was built with, so a case reads `(2/N)`
    /// rather than pinning the shipped ten — the constant has a
    /// `ROOST_TEST_MODE` override, and a suite run under it must not
    /// start failing.
    pub(crate) fn budget(set: &HostConnSet) -> u32 {
        set.outage("h1").ladder.budget()
    }

    /// One turn of the ladder exactly as the app drives it: the armed
    /// timer comes due, and the re-entry
    /// `App::host_reconnect_requested` performs lands here as the same
    /// `open_ssh` that call would reach. Returns whether the due message
    /// authorized a dial.
    pub(crate) fn retry_once(set: &mut HostConnSet) -> bool {
        let request = set.ssh("h1").request;
        // The origin and the cause are the set's own answer, not the
        // test's: `App::host_reconnect_due` passes back exactly what it
        // is handed, and the origin is the load-bearing consent gate.
        let Some((origin, cause)) = set.reconnect_due("h1", request) else {
            return false;
        };
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            origin,
            cause,
        );
        true
    }

    /// A working ssh host whose link has just dropped: the ladder is at
    /// attempt one with a timer armed. Where every case below starts
    /// that is not itself about that first decision.
    pub(crate) fn a_dropped_ssh_host(set: &mut HostConnSet, socket: &str) -> HostId {
        let incarnation = a_connected_ssh_host(set, socket);
        set.apply_state(incarnation, dropped("the connection closed"));
        incarnation
    }

    /// A transport failure, which is the retryable establish shape.
    pub(crate) fn unreachable() -> SshFailure {
        SshFailure::Transport(Some("no route to host".into()))
    }

    /// Answer this host's in-flight establish with a classified failure,
    /// as the transport hands one back.
    pub(crate) fn refuse(set: &mut HostConnSet, failure: SshFailure) {
        let ready = refused("h1", set.ssh("h1").request, failure);
        set.tunnel_ready(ready);
    }

    /// A real `SshTunnel` with nothing behind it: `open` claims a
    /// scratch directory and writes a config, and binds and connects
    /// nothing. So a discard can be *measured* — the directory is gone
    /// afterwards — with no `ssh` anywhere near the suite.
    pub(crate) async fn an_unestablished_tunnel(parent: PathBuf) -> Arc<SshTunnel> {
        Arc::new(
            SshTunnel::open(
                "discardcase",
                &ssh_target("workbox"),
                roost_ipc::ssh::SshTunnelOptions {
                    config_paths: roost_ipc::ssh::SshConfigPaths {
                        user: None,
                        system: None,
                    },
                    // A macOS `$TMPDIR` can be too deep for a `sun_path`;
                    // the fallback is what `from_env` would pick too.
                    scratch_parents: vec![parent, PathBuf::from("/tmp")],
                    // Never spawned: the teardown's `-O exit` is skipped
                    // for a control socket that was never bound.
                    ssh_bin: PathBuf::from("/nonexistent/ssh"),
                    jail_fs_root: false,
                },
            )
            .await
            .expect("claim a scratch directory"),
        )
    }

    /// Nothing in this suite may dial a real host: stop every handshake
    /// an `open_ssh` spawned before anything is polled. A case that
    /// awaits after an `open_ssh` — and the runtime only polls when it
    /// does — calls this first.
    pub(crate) fn abort_establishes(set: &mut HostConnSet) {
        for entry in set.entries.values_mut().filter_map(|e| e.ssh.as_mut()) {
            if let Some(establish) = entry.establish.take() {
                establish.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn minted_incarnations_are_unique_and_never_local() {
        let minter = HostIdMinter::new();
        let ids: Vec<HostId> = (0..64).map(|_| minter.mint("h1", 1)).collect();
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            assert!(
                !id.is_local(),
                "a minted instance never aliases the local one"
            );
            assert!(seen.insert(id), "{id:?} was minted twice");
        }
    }

    /// The whole point of minting per connect: the same numeric tab id
    /// under two incarnations is two keys.
    #[test]
    fn two_incarnations_of_one_host_are_two_id_spaces() {
        let minter = HostIdMinter::new();
        let first = minter.mint("h1", 1);
        let second = minter.mint("h1", 2);
        assert_ne!(first, second);
        assert_ne!(
            roost_ui_model::keys::TabKey::new(first, 7),
            roost_ui_model::keys::TabKey::new(second, 7)
        );
    }

    /// The drain side resolves a feed item's host off the minter's
    /// registration, so two hosts connecting at once cannot have their
    /// states swapped — the failure a "whichever host is waiting"
    /// heuristic would produce.
    #[tokio::test]
    async fn two_hosts_connecting_at_once_keep_their_own_states() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-one.sock"),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        set.connect(
            "h2",
            "two",
            PathBuf::from("/nonexistent/roost-set-two.sock"),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );

        let first = set.mint_for("h1");
        let second = set.mint_for("h2");
        assert_eq!(
            set.apply_state(second, HostConnState::Connected).as_deref(),
            Some("h2")
        );
        assert_eq!(
            set.apply_state(first, HostConnState::TakenOver).as_deref(),
            Some("h1")
        );
        assert_eq!(set.state("h1"), Some(&HostConnState::TakenOver));
        assert_eq!(set.state("h2"), Some(&HostConnState::Connected));
        assert_eq!(set.incarnation("h2"), Some(second));
    }

    /// The staleness mechanism end to end: `Connecting` purges the
    /// incarnation it replaces, and anything still arriving under that
    /// id afterwards is dropped rather than applied to the new one.
    #[tokio::test]
    async fn a_reconnect_purges_the_old_incarnation_and_drops_its_stragglers() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-purge.sock"),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );

        let old = set.mint_for("h1");
        set.apply_state(old, HostConnState::Connected);
        set.apply_workspace(old, HostWorkspaceEvent::Reset(Arc::default()));
        assert!(set.mirror(old).is_some());

        let new = set.mint_for("h1");
        assert_eq!(
            set.apply_state(
                new,
                HostConnState::Connecting {
                    previous: Some(old)
                }
            )
            .as_deref(),
            Some("h1")
        );
        assert!(
            set.mirror(old).is_none(),
            "the dead incarnation's mirror is purged, not merged forward"
        );

        // A straggler from the old epoch — a batch already in flight when
        // the reconnect began.
        set.apply_workspace(old, HostWorkspaceEvent::Reset(Arc::default()));
        assert!(set.mirror(old).is_none());
        assert_eq!(set.apply_state(old, HostConnState::Connected), None);
        assert!(
            matches!(set.state("h1"), Some(HostConnState::Connecting { .. })),
            "a stale state must not resurrect a connection"
        );
    }

    /// The rapid disconnect + reconnect case. Winding a task down is not
    /// instantaneous, so the replaced one can still mint and publish
    /// after its replacement is live — and it must land nowhere. Keyed
    /// on the host name alone these would hit the replacement: a stale
    /// `Connecting` purging the fresh mirror, a stale `Connected`
    /// resurrecting a connection that is gone.
    #[tokio::test]
    async fn a_replaced_connection_cannot_publish_onto_its_replacement() {
        let (mut set, _feed) = a_set();
        let socket = PathBuf::from("/nonexistent/roost-set-replaced.sock");
        set.connect(
            "h1",
            "one",
            socket.clone(),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let outgoing = set.conn("h1").generation;

        // The user hits Connect again before the first task has wound
        // down.
        set.connect(
            "h1",
            "one",
            socket,
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let live = set.mint_for("h1");
        set.apply_state(live, HostConnState::Connected);
        set.apply_workspace(live, HostWorkspaceEvent::Reset(Arc::default()));

        // Only now does the outgoing task get around to its next
        // attempt, under the same saved host.
        let stale = set.minter.mint("h1", outgoing);
        assert_eq!(
            set.apply_state(
                stale,
                HostConnState::Connecting {
                    previous: Some(live)
                }
            ),
            None,
            "a replaced connection's state must not be attributed to its \
             replacement"
        );
        assert!(
            set.mirror(live).is_some(),
            "and it must not purge the live incarnation on the way out"
        );
        assert_eq!(set.state("h1"), Some(&HostConnState::Connected));
        assert_eq!(set.incarnation("h1"), Some(live));

        set.apply_workspace(stale, HostWorkspaceEvent::Reset(Arc::default()));
        assert!(set.mirror(stale).is_none());
        assert!(set.ops_for(stale).is_none());
        assert!(set.ops_for(live).is_some());
    }

    /// A destructive verb aimed at the selection carries an incarnation,
    /// not a saved id (plan 037 §3.9): `tab.close` on a host row is
    /// addressed by the row's own `TabKey`. It has to reach that host's
    /// queue — and a key minted by a connection that is gone must be
    /// answered rather than routed onto whatever replaced it, or a close
    /// lands on a same-numbered tab of a different session.
    #[tokio::test]
    async fn an_intent_addressed_by_incarnation_reaches_that_incarnation_or_nobody() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-send-at.sock"),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let live = set.mint_for("h1");
        set.apply_state(live, HostConnState::Connected);

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        assert!(set
            .send_at(
                live,
                HostIntent::new(ops::TAB_CLOSE, serde_json::json!({ "tab_id": "7" })).answering(tx)
            )
            .is_ok());
        assert!(
            rx.try_recv().is_err(),
            "the queued intent is the worker's to answer, not the enqueue's"
        );

        // A key from a connection that has been replaced, and one from a
        // host that is gone entirely.
        let stale = set.minter.mint("h1", set.conn("h1").generation - 1);
        for dead in [stale, HostId::new(9_999)] {
            let (tx, mut rx) = tokio::sync::oneshot::channel();
            assert!(matches!(
                set.send_at(
                    dead,
                    HostIntent::new(ops::TAB_CLOSE, serde_json::json!({})).answering(tx)
                ),
                Err(HostOpError::Unavailable)
            ));
            assert!(
                matches!(rx.try_recv(), Ok(Err(HostOpError::Unavailable))),
                "a refused intent is answered, never dropped on the floor"
            );
        }
    }

    /// A failed establish never enters `HostConn` — there is no socket to
    /// dial — so the reason it leaves on the entry is the only thing the
    /// band has to render, and only an attended attempt says it out loud.
    #[tokio::test]
    async fn a_failed_establish_leaves_a_reason_and_toasts_only_when_asked_for() {
        let (mut set, _feed) = a_set();

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        let request = set.ssh("h1").request;
        assert_eq!(
            set.tunnel_ready(failed("h1", request, "workbox refused authentication"))
                .as_deref(),
            Some("workbox refused authentication"),
            "a user who asked is told"
        );
        assert!(set.is_empty(), "and nothing was dialed");
        assert_eq!(
            set.section_reason("h1"),
            Some("workbox refused authentication")
        );

        set.open_ssh(
            "h2",
            "two",
            ssh_target("user@box"),
            ConnectMode::IfPresent,
            RequestOrigin::Ipc,
            AttemptCause::Explicit,
        );
        let request = set.ssh("h2").request;
        assert_eq!(
            set.tunnel_ready(failed("h2", request, "box is unreachable")),
            None,
            "an unattended failure is the band's business alone"
        );
        assert_eq!(set.section_reason("h2"), Some("box is unreachable"));
    }

    /// The distinction plan 039 §3.5 needs and `ConnectMode` cannot
    /// carry: `roostctl host connect` dials exactly as a click does, so
    /// "attended" is true for both — but only one of them has a human
    /// who could answer a dialog. Both facts are recorded, and the
    /// attended one is unchanged.
    #[tokio::test]
    async fn an_ipc_dial_is_attended_but_is_never_a_user() {
        let (mut set, _feed) = a_set();

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::Ipc,
            AttemptCause::Explicit,
        );
        assert_eq!(set.ssh_origin("h1"), Some(RequestOrigin::Ipc));
        assert_ne!(
            set.ssh_origin("h1"),
            Some(RequestOrigin::User),
            "a modal must never open to answer a machine"
        );
        assert!(
            set.ssh("h1").attended,
            "an IPC dial still owes its caller a reason, exactly as today"
        );
        let request = set.ssh("h1").request;
        assert_eq!(
            set.tunnel_ready(failed("h1", request, "workbox refused authentication"))
                .as_deref(),
            Some("workbox refused authentication"),
        );

        // And the same call from a person is recorded as one.
        set.open_ssh(
            "h2",
            "two",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        assert_eq!(set.ssh_origin("h2"), Some(RequestOrigin::User));
        assert_eq!(set.ssh_origin("nobody"), None);
    }

    /// The classified family survives the trip into the app layer.
    /// Routing a bootstrap offer off "the binary is not installed over
    /// there" must never mean substring-matching the sentence a user
    /// reads — that is the mistake `classify_ssh_failure` exists to
    /// prevent — so the family rides beside the copy rather than being
    /// rendered away.
    #[tokio::test]
    async fn a_failures_family_reaches_the_app_beside_its_copy() {
        use roost_ipc::ssh::SshFailure;
        let (mut set, _feed) = a_set();

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        let request = set.ssh("h1").request;
        let toast = set
            .tunnel_ready(refused("h1", request, SshFailure::NotFound))
            .expect("an attended failure is said out loud");

        assert_eq!(set.ssh_failure("h1"), Some(&SshFailure::NotFound));
        // The copy is the classifier's own, unchanged, and it is what
        // both the toast and the band show.
        assert_eq!(toast, SshFailure::NotFound.message("workbox"));
        assert_eq!(set.section_reason("h1"), Some(toast.as_str()));

        // This side's own failures have no family and no remedy over
        // there, and say so by carrying none.
        set.open_ssh(
            "h2",
            "two",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        let request = set.ssh("h2").request;
        set.tunnel_ready(failed(
            "h2",
            request,
            "could not create a scratch directory",
        ));
        assert_eq!(set.ssh_failure("h2"), None);
        assert_eq!(
            set.section_reason("h2"),
            Some("could not create a scratch directory")
        );
    }

    /// A bootstrap's own line sits in front of the failure that started
    /// it: while Roost is answering "roost-session isn't installed
    /// there", the band saying so as well would describe a question
    /// already being dealt with. And a fresh Connect clears it, because
    /// this attempt's own outcome is then the newer answer.
    #[tokio::test]
    async fn a_running_bootstrap_owns_the_band_until_the_next_connect() {
        use roost_ipc::ssh::SshFailure;
        let (mut set, _feed) = a_set();

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        let request = set.ssh("h1").request;
        set.tunnel_ready(refused("h1", request, SshFailure::NotFound));
        assert_eq!(
            set.section_reason("h1"),
            Some(SshFailure::NotFound.message("workbox").as_str())
        );

        set.set_bootstrap_note("h1", Some("checking one…".into()));
        assert_eq!(set.section_reason("h1"), Some("checking one…"));

        // A terminal failure travels the same slot: it is the most
        // recent true sentence, and it must not vanish the moment the
        // job's task ends.
        set.set_bootstrap_note("h1", Some("couldn't reach workbox".into()));
        assert_eq!(set.section_reason("h1"), Some("couldn't reach workbox"));

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        assert_eq!(
            set.section_reason("h1"),
            None,
            "a fresh attempt supersedes whatever the last bootstrap left"
        );

        // Clearing it explicitly falls back to whatever else is current.
        let request = set.ssh("h1").request;
        set.tunnel_ready(refused("h1", request, SshFailure::NoSession));
        set.set_bootstrap_note("h1", Some("setting up roost-session…".into()));
        set.set_bootstrap_note("h1", None);
        assert_eq!(
            set.section_reason("h1"),
            Some(SshFailure::NoSession.message("workbox").as_str())
        );
    }

    /// An answer has to prove it belongs to the request still waiting.
    /// Ask twice and the first establish's answer must land nowhere —
    /// otherwise a stale failure would overwrite the band while the
    /// second attempt is still in flight.
    #[tokio::test]
    async fn a_superseded_or_cancelled_establish_answers_nobody() {
        let (mut set, _feed) = a_set();
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        let first = set.ssh("h1").request;

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        let second = set.ssh("h1").request;
        assert_ne!(first, second, "a request id is never reused");

        assert_eq!(set.tunnel_ready(failed("h1", first, "stale")), None);
        assert_eq!(
            set.section_reason("h1"),
            None,
            "the superseded answer left nothing behind"
        );

        // And a disconnect while an establish is in flight cancels it:
        // the entry is gone, so the answer has nowhere to land.
        set.disconnect("h1");
        assert_eq!(set.tunnel_ready(failed("h1", second, "cancelled")), None);
        assert_eq!(set.section_reason("h1"), None);
    }

    /// The band prefers the live connection's own reason. An ssh failure
    /// recorded before this connection existed must not outrank what the
    /// connection is saying now.
    #[tokio::test]
    async fn a_live_connections_reason_outranks_a_previous_establish_failure() {
        let (mut set, _feed) = a_set();
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        let request = set.ssh("h1").request;
        set.tunnel_ready(failed("h1", request, "workbox refused authentication"));

        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-ssh.sock"),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let incarnation = set.mint_for("h1");
        set.apply_state(
            incarnation,
            HostConnState::Disconnected(state::Disconnected {
                reason: "the session closed".into(),
                detail: None,
                retry_in: None,
            }),
        );
        assert_eq!(set.section_reason("h1"), Some("the session closed"));
    }

    /// The one fact that separates a first connect from a session
    /// dropping under the user.
    ///
    /// Both surface as a `Disconnected` carrying `NotFound`/`NoSession`
    /// on a `User`-originated attempt — the origin is the *establish's*,
    /// and stays `User` for as long as the connection lives — so
    /// without this flag `roostctl session stop` on the far side would
    /// throw a consent card over an unrelated local tab.
    #[tokio::test]
    async fn a_connection_that_worked_is_marked_and_a_fresh_attempt_is_not() {
        let (mut set, _feed) = a_set();
        assert!(!set.ssh_reached_connected("h1"), "nothing asked for yet");

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-reached.sock"),
            HostTransport::Ssh,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let incarnation = set.mint_for("h1");

        // The first-connect shape: the establish came up, the
        // per-connection exec is what failed, and nothing ever served.
        set.apply_state(
            incarnation,
            HostConnState::Disconnected(state::Disconnected {
                reason: "roost-session isn't installed on workbox".into(),
                detail: None,
                retry_in: None,
            }),
        );
        assert!(
            !set.ssh_reached_connected("h1"),
            "a connect that never worked is still an offer"
        );

        set.apply_state(incarnation, HostConnState::Connected);
        assert!(set.ssh_reached_connected("h1"));
        set.apply_state(
            incarnation,
            HostConnState::Disconnected(state::Disconnected {
                reason: "the session closed".into(),
                detail: None,
                retry_in: None,
            }),
        );
        assert!(
            set.ssh_reached_connected("h1"),
            "a session going away does not un-happen"
        );

        // A fresh attempt is a fresh question.
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        assert!(!set.ssh_reached_connected("h1"));
    }

    /// The lease's whole life, walked rather than seeded.
    ///
    /// Every step of it is somewhere the previous store would have been
    /// wiped, which is why none of them may be short-circuited: it is
    /// published at `Connected`, when **no outage exists yet**; the
    /// outage that copies it out is not created until the drop; and the
    /// entry it was stamped on is replaced wholesale by the very
    /// `open_ssh` the ladder re-enters through. A test that seeded any
    /// one of those by hand would stay green while the guard was dead.
    #[tokio::test]
    async fn a_lease_published_at_connected_reaches_the_attempt_after_the_drop() {
        let (mut set, _feed) = a_set();
        let incarnation = a_connected_ssh_host(&mut set, "/nonexistent/roost-set-lease.sock");
        set.apply_lease(incarnation, "lease-1".into());
        assert_eq!(
            set.ssh("h1").lease.as_deref(),
            Some("lease-1"),
            "the lease lands on the entry the connection was opened under"
        );
        assert!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .is_none(),
            "and nowhere else yet — there is no outage to hold it"
        );

        set.apply_state(incarnation, dropped("the connection closed"));
        set.begin_outage("h1");
        assert_eq!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .as_deref(),
            Some("lease-1"),
            "the outage copies it out of the entry that is about to go"
        );

        // The re-entry: a fresh entry, and the lease still reaches the
        // dial it authorizes.
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::Ipc,
            AttemptCause::AutoReconnect,
        );
        assert_eq!(
            set.ssh("h1").lease,
            None,
            "the fresh entry knows nothing about the connection that died"
        );
        assert_eq!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .as_deref(),
            Some("lease-1"),
            "but the outage does, which is the whole reason it exists"
        );
    }

    /// A second drop inside one outage carries the *second* attempt's
    /// lease.
    ///
    /// The shape is the one `connect_loop` publishes a lease for without
    /// ever reaching `Connected`: `session.connect` granted a lease — so
    /// ownership moved on the wire and everything older is a tombstone —
    /// and the prologue then failed. Nothing clears the outage on that
    /// path (`Connected` is what does), so the entry survives to the next
    /// drop and [`HostConnSet::begin_outage`] has to refresh what it
    /// carries. Called from inside the entry-creation gate it never runs
    /// for that drop, and the ladder goes on presenting a lease the far
    /// side has already tombstoned: `taken-over`, terminal, with no other
    /// client involved (plan 040 §3.7).
    #[tokio::test]
    async fn a_second_drop_refreshes_the_lease_the_outage_carries() {
        let (mut set, _feed) = a_set();
        let socket = "/nonexistent/roost-set-refresh.sock";
        let first = a_connected_ssh_host(&mut set, socket);
        set.apply_lease(first, "lease-1".into());
        set.apply_state(first, dropped("the connection closed"));
        assert_eq!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .as_deref(),
            Some("lease-1"),
            "the drop that started the outage copied the lease out"
        );

        // The retry's tunnel comes up and its prologue is granted a lease
        // before failing — no `Connected`, so the outage is still the
        // same one.
        assert!(retry_once(&mut set));
        set.connect(
            "h1",
            "one",
            PathBuf::from(socket),
            HostTransport::Ssh,
            ConnectMode::Dial,
            AttemptCause::AutoReconnect,
        );
        let second = set.mint_for("h1");
        set.apply_lease(second, "lease-2".into());
        set.apply_state(second, dropped("and the prologue failed"));

        assert!(
            set.has_outage("h1"),
            "the ladder is still walking the outage it started"
        );
        assert_eq!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .as_deref(),
            Some("lease-2"),
            "and the next attempt presents the lease the last one was granted"
        );
    }

    /// What an attempt's cause decides, at both ends of the two-step ssh
    /// connect: `open_ssh` records it on the entry `tunnel_ready` reads
    /// to build the dial, and `connect` turns it into the lease the task
    /// starts holding.
    ///
    /// An explicit attempt clears the outage outright — the user's
    /// attempt supersedes the schedule, and a lease minted two
    /// connections ago must not be presented by the next ladder. A
    /// scheduled one leaves it alone: without that, every retry would
    /// zero its own attempt counter and the ladder would never end.
    #[tokio::test]
    async fn an_attempts_cause_decides_the_outage_and_the_lease_it_carries() {
        let (mut set, _feed) = a_set();
        let incarnation = a_connected_ssh_host(&mut set, "/nonexistent/roost-set-cause.sock");
        set.apply_lease(incarnation, "lease-1".into());
        set.apply_state(incarnation, dropped("the connection closed"));
        set.begin_outage("h1");

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::Ipc,
            AttemptCause::AutoReconnect,
        );
        assert_eq!(set.ssh("h1").cause, AttemptCause::AutoReconnect);
        assert!(
            set.has_outage("h1"),
            "a scheduled attempt leaves the ladder it belongs to alone"
        );
        assert_eq!(
            set.carried_lease("h1", set.ssh("h1").cause).as_deref(),
            Some("lease-1")
        );

        // The user clicks ↻ mid-ladder.
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        assert_eq!(set.ssh("h1").cause, AttemptCause::Explicit);
        assert!(
            !set.has_outage("h1"),
            "an explicit attempt supersedes the schedule outright"
        );
        assert_eq!(
            set.carried_lease("h1", set.ssh("h1").cause),
            None,
            "cleared, not merely not-passed: the lease went with the outage"
        );
        assert!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .is_none(),
            "so a ladder started after it cannot present the old lease either"
        );
    }

    /// A lease publication is attributed like every other feed item. One
    /// from a connection this set has since replaced must land nowhere:
    /// stored, it would become the lease the *next* outage presents —
    /// a lease two connections old, offered to a session that never
    /// issued it.
    #[tokio::test]
    async fn a_lease_from_a_replaced_connection_is_dropped() {
        let (mut set, _feed) = a_set();
        let live = a_connected_ssh_host(&mut set, "/nonexistent/roost-set-lease-stale.sock");
        set.apply_lease(live, "lease-live".into());

        let replaced = set.minter.mint("h1", set.conn("h1").generation - 1);
        set.apply_lease(replaced, "lease-from-the-connection-before".into());
        assert_eq!(
            set.ssh("h1").lease.as_deref(),
            Some("lease-live"),
            "a replaced connection cannot overwrite the live one's lease"
        );

        // And an incarnation this set never minted at all.
        set.apply_lease(HostId::new(9_999), "invented".into());
        assert_eq!(set.ssh("h1").lease.as_deref(), Some("lease-live"));

        // A disconnect ends the outage the lease would have been kept
        // for, so nothing survives to be presented.
        set.begin_outage("h1");
        set.disconnect("h1");
        assert!(set
            .carried_lease("h1", AttemptCause::AutoReconnect)
            .is_none());
    }

    /// A lease still in flight when an explicit connect installs a fresh
    /// attempt must not land on that attempt.
    ///
    /// The window is the ssh path's alone, and it is why "is this
    /// connection current?" cannot answer this on its own: `open_ssh`
    /// replaces [`SshState`] immediately, while the [`HostConn`] the
    /// lease was minted under stays in `conns` until the *next* tunnel
    /// comes up — so [`HostConnSet::owner_of`] accepts an item the
    /// explicit connect was supposed to have cleared. Stored, it would
    /// be copied onto the first outage of a connection that never
    /// issued it, undoing §3.7's "cleared, not merely not-passed".
    #[tokio::test]
    async fn a_lease_does_not_land_on_the_attempt_that_superseded_it() {
        let (mut set, _feed) = a_set();
        let live = a_connected_ssh_host(&mut set, "/nonexistent/roost-set-lease-superseded.sock");

        // The user clicks Connect. A fresh entry is in place at once;
        // the old connection is not forgotten until its replacement's
        // tunnel answers.
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        assert!(
            set.owner_of(live).is_some(),
            "the connection the lease belongs to is still held — that is the window"
        );

        set.apply_lease(live, "lease-1".into());
        assert_eq!(
            set.ssh("h1").lease,
            None,
            "but the attempt that replaced it never held that lease"
        );
        set.begin_outage("h1");
        assert!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .is_none(),
            "so no ladder off this attempt can present it"
        );
    }

    /// An outage carries the *freshest* lease, not the first one it ever
    /// saw.
    ///
    /// A reconnect that reaches `Connected` mints a new lease and
    /// tombstones the old one, so an outage still holding the old one
    /// would present a tombstone on the next drop — the far side answers
    /// `taken-over` and the host settles as somebody else's with nobody
    /// else involved. A retry that only failed its establish holds no
    /// lease at all, and there the outage's own is still the one to
    /// present.
    #[tokio::test]
    async fn an_outage_carries_the_lease_of_the_connection_that_just_died() {
        let (mut set, _feed) = a_set();
        let first = a_connected_ssh_host(&mut set, "/nonexistent/roost-set-lease-refresh.sock");
        set.apply_lease(first, "lease-1".into());
        set.apply_state(first, dropped("the connection closed"));
        set.begin_outage("h1");
        assert_eq!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .as_deref(),
            Some("lease-1")
        );

        // A retry that gets all the way to `Connected` again.
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::Ipc,
            AttemptCause::AutoReconnect,
        );
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-lease-refresh.sock"),
            HostTransport::Ssh,
            ConnectMode::Dial,
            AttemptCause::AutoReconnect,
        );
        let second = set.mint_for("h1");
        set.apply_state(second, HostConnState::Connected);
        set.apply_lease(second, "lease-2".into());
        set.apply_state(second, dropped("and it dropped again"));
        set.begin_outage("h1");
        assert_eq!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .as_deref(),
            Some("lease-2"),
            "the second drop presents the second connection's lease"
        );

        // A retry that never gets a connection at all: its entry has no
        // lease, and the outage keeps the one it has.
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::Ipc,
            AttemptCause::AutoReconnect,
        );
        set.begin_outage("h1");
        assert_eq!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .as_deref(),
            Some("lease-2"),
            "a failed establish has no lease of its own to supersede it with"
        );
    }

    // ── the retry ladder (plan 040 §3.4) ────────────────────────────
    //
    // What these cases can and cannot reach, stated once. The **session**
    // arm's family (`DropInput::Session(Some(_))`) comes from
    // `overlay_ssh_reason`, which reads a live `SshTunnel`'s
    // `last_error` — and `SshTunnel::record` is private to `roost-ipc`,
    // reachable only by that crate's own exec paths. So the cases below
    // drive families through the **establish** arm, which carries its
    // own `ConnectFailure`, and through `DeadTunnel::Recorded` for the
    // fire-time re-check. The per-family verdicts themselves are
    // `reconnect.rs`'s table tests.

    /// The whole `Disconnected` a host's band reads, whichever of the
    /// three writers put it there.
    fn band_state(set: &HostConnSet, host: &str) -> state::Disconnected {
        match set.state(host) {
            Some(HostConnState::Disconnected(disconnected)) => disconnected.clone(),
            other => panic!("{host} is {other:?}, not disconnected"),
        }
    }

    /// The headline case: a working ssh connection drops, and the host
    /// schedules its own way back without anybody clicking anything.
    ///
    /// A bare bridge EOF is the shape — no `last_error` on the tunnel at
    /// all — which is exactly the case `overlay_ssh_reason` early-returns
    /// on. A decision taken inside the overlay would never fire here.
    #[tokio::test]
    async fn a_retryable_drop_arms_a_retry_and_says_how_long() {
        let (mut set, _feed) = a_set();
        let incarnation = a_connected_ssh_host(&mut set, "/nonexistent/roost-set-arm.sock");
        set.apply_state(incarnation, dropped("the connection closed"));

        assert!(
            set.outage("h1").armed.is_some(),
            "a retryable drop leaves a timer waiting"
        );
        let band = band_state(&set, "h1");
        assert!(
            band.retry_in.is_some(),
            "and says so on the wire the UI reads"
        );
        assert_eq!(
            band.reason,
            format!("reconnecting in 1s (1/{})", budget(&set))
        );

        // The second `Dropped` a `HostConn::drop` can produce under the
        // same generation: no second timer, and the line the armed retry
        // wrote survives `apply_state` writing the raw reason back.
        set.apply_state(incarnation, dropped("the connection closed"));
        assert_eq!(set.outage("h1").ladder.attempts(), 1);
        assert_eq!(band_state(&set, "h1"), band);
    }

    /// `host.status`'s `retry` and the band's `(n/N)` are the same two
    /// numbers, read at two different moments: `arm_retry` takes the
    /// `attempt` off `Decision::Retry`, the accessor reads
    /// `ladder.attempts()` afterwards. `next` bumps the counter before
    /// it answers, so they agree — walked over two rungs so the case
    /// would fail if either side pinned a constant.
    #[tokio::test]
    async fn the_retry_schedule_carries_the_bands_own_numbers() {
        let (mut set, _feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-sched.sock");
        let budget = budget(&set);

        for attempt in 1..=2 {
            let schedule = set.retry_schedule("h1").expect("an armed retry");
            assert_eq!(schedule.attempt, Some(attempt));
            assert_eq!(schedule.budget, Some(budget));
            assert_eq!(
                schedule.delay_ms,
                band_state(&set, "h1").retry_in.expect("armed").as_millis() as u64,
                "the wire's delay is the one the timer was armed with"
            );
            assert_eq!(
                band_reason(&set, "h1"),
                format!(
                    "reconnecting in {}s ({attempt}/{budget})",
                    schedule.delay_ms.div_ceil(1_000).max(1)
                )
            );
            assert!(
                schedule.armed_at.is_some_and(|at| at.ends_with('Z')),
                "and it says when, so a caller can count down"
            );

            assert!(retry_once(&mut set), "the due retry dials");
            assert_eq!(
                set.retry_schedule("h1"),
                None,
                "once the timer has fired nothing is armed, even though the \
                 dead connection's own retry_in still mirrors the rung"
            );
            refuse(&mut set, unreachable());
        }
    }

    /// The armed rung's own `reason` (#399), read off the wire the same
    /// way `host.status` does.
    fn retry_reason(set: &HostConnSet) -> Option<String> {
        set.retry_schedule("h1").expect("an armed retry").reason
    }

    /// **#399.** While a rung is armed, `reason` has to read
    /// `reconnecting in 8s (3/10)` — the sidebar's rollup is derived
    /// from it — so *why* the ladder is retrying needs its own field or
    /// it is unreadable until the attempt settles.
    ///
    /// Three facts in one walk, because they are the same field seen at
    /// three moments: the **first** rung of an outage carries no family
    /// (the drop that starts one is the live connection dying, a bare
    /// bridge EOF the overlay classifies nothing from); the next rung
    /// does, through the establish arm; and a rung after *that* which was
    /// itself armed by a bare EOF carries none again, because the field
    /// names this rung's cause and the older copy is no longer true.
    #[tokio::test]
    async fn an_armed_rung_says_why_it_is_armed() {
        let (mut set, _feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-why.sock");
        assert_eq!(
            set.retry_schedule("h1").expect("armed").attempt,
            Some(1),
            "the drop that starts an outage"
        );
        assert_eq!(
            retry_reason(&set),
            None,
            "a bare bridge EOF has no family to name"
        );

        // Rung two: the retry dials, its establish fails, and *that*
        // failure is classified.
        assert!(retry_once(&mut set));
        refuse(&mut set, unreachable());
        assert_eq!(set.retry_schedule("h1").expect("armed").attempt, Some(2));
        assert_eq!(
            retry_reason(&set).as_deref(),
            Some(unreachable().message("workbox").as_str()),
            "and it is the family's own copy, the wording a give-up appends too"
        );
        let seconds = set
            .retry_schedule("h1")
            .expect("armed")
            .delay_ms
            .div_ceil(1_000)
            .max(1);
        assert_eq!(
            band_reason(&set, "h1"),
            format!("reconnecting in {seconds}s (2/{})", budget(&set)),
            "the band still shows the rung, not the family — the rollup is derived from it"
        );

        // Rung three, armed by a connection that came up and died with
        // nothing recorded: the honest answer is no family again.
        assert!(retry_once(&mut set));
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-why.sock"),
            HostTransport::Ssh,
            ConnectMode::Dial,
            AttemptCause::AutoReconnect,
        );
        let third = set.mint_for("h1");
        set.apply_state(third, dropped("and it dropped again"));
        assert_eq!(set.retry_schedule("h1").expect("armed").attempt, Some(3));
        assert_eq!(
            retry_reason(&set),
            None,
            "the previous rung's copy is stale by now, and stale is a lie"
        );
    }

    /// **The write-ordering trap (#399, plan 044 §3.3 d2).**
    ///
    /// `HostConn::drop` publishes its own `Dropped`, and a late one
    /// arrives under the *same* generation. `overlay_ssh_reason`
    /// early-returns `None` for it (the generation is already `seen`),
    /// so it reaches `schedule_reconnect` as `Session(None)` with no
    /// family at all — and it is caught by the already-armed guard,
    /// which re-emits the line without re-arming.
    ///
    /// Which is why the family is written where the `Decision::Retry` is
    /// taken and nowhere earlier: a write before that guard would erase
    /// the family the armed rung is actually for, and `host.status`
    /// would go quiet exactly when a user is watching it.
    #[tokio::test]
    async fn a_late_same_generation_drop_leaves_the_armed_rungs_reason_alone() {
        let (mut set, _feed) = a_set();
        let incarnation = a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-late.sock");
        assert!(retry_once(&mut set));
        refuse(&mut set, unreachable());
        let armed = set.retry_schedule("h1").expect("armed");
        assert_eq!(armed.attempt, Some(2));
        assert_eq!(
            armed.reason.as_deref(),
            Some(unreachable().message("workbox").as_str())
        );

        set.apply_state(incarnation, dropped("the connection closed"));

        assert_eq!(
            set.outage("h1").ladder.attempts(),
            2,
            "no second timer stacked on the first"
        );
        assert_eq!(
            set.retry_schedule("h1"),
            Some(armed),
            "and the whole schedule — the family included — is untouched"
        );
    }

    /// A family no retry may be spent on ends the outage, and the
    /// rung's `reason` goes with it: there is no armed rung left to
    /// explain, and the band already carries that family's own copy
    /// (#399). The give-up end of the same rule is pinned in
    /// `repeated_establish_failures_run_the_ladder_to_its_budget`.
    #[tokio::test]
    async fn a_non_retryable_family_takes_the_rungs_reason_with_the_outage() {
        let (mut set, _feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-nonretry.sock");
        assert!(retry_once(&mut set));
        refuse(&mut set, unreachable());
        assert!(retry_reason(&set).is_some(), "a family was armed on");

        assert!(retry_once(&mut set));
        refuse(&mut set, SshFailure::ChangedHostKey);
        assert!(!set.has_outage("h1"));
        assert_eq!(set.retry_schedule("h1"), None);
        assert_eq!(
            band_reason(&set, "h1"),
            SshFailure::ChangedHostKey.message("workbox"),
            "the band keeps the family that stopped the ladder"
        );
    }

    /// A localhost retry is the connection task's own backoff: the delay
    /// is published on the state, the counter never leaves the task.
    #[tokio::test]
    async fn a_localhost_retry_reports_a_delay_and_nothing_else() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-local.sock"),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let incarnation = set.mint_for("h1");
        assert_eq!(
            set.retry_schedule("h1"),
            None,
            "a connecting host has nothing armed"
        );

        set.apply_state(
            incarnation,
            HostConnState::Disconnected(state::Disconnected {
                reason: "the session closed".into(),
                detail: None,
                retry_in: Some(Duration::from_millis(250)),
            }),
        );
        assert_eq!(
            set.retry_schedule("h1"),
            Some(RetrySchedule {
                delay_ms: 250,
                ..RetrySchedule::default()
            })
        );

        set.apply_state(incarnation, dropped("and it settled"));
        assert_eq!(set.retry_schedule("h1"), None);
    }

    /// The generation is the host's, not the live connection's: a
    /// disconnected host must not read `0` again, or a poller waiting
    /// for the number to advance would count the next attempt twice.
    #[tokio::test]
    async fn the_generation_outlives_the_connection_and_dies_with_the_host() {
        let (mut set, _feed) = a_set();
        assert_eq!(set.generation("h1"), 0, "before any connect");

        let connect = |set: &mut HostConnSet| {
            set.connect(
                "h1",
                "one",
                PathBuf::from("/nonexistent/roost-set-gen.sock"),
                HostTransport::UnixSocket,
                ConnectMode::Dial,
                AttemptCause::Explicit,
            );
        };
        connect(&mut set);
        assert_eq!(set.generation("h1"), 1);

        set.disconnect("h1");
        assert_eq!(
            set.generation("h1"),
            1,
            "a disconnect ends the connection, not the host's history"
        );

        connect(&mut set);
        assert_eq!(set.generation("h1"), 2);

        set.remove("h1");
        assert_eq!(set.generation("h1"), 0, "forgetting the host forgets it");
    }

    /// The generation counts attempts *started*, not connections made —
    /// which for an ssh host is the whole difference. Its attempt
    /// begins at the handshake, and most of the ways one fails (no
    /// route, a refused port, a changed host key) never reach a
    /// `connect` at all, so numbering it there would leave a ten-rung
    /// ladder reporting one generation while the band counts `(1/10)`
    /// through `(10/10)` — and `host.status`'s one monotonic edge would
    /// be flat exactly where a caller needs it.
    #[tokio::test]
    async fn an_ssh_generation_advances_once_per_attempt_started() {
        let (mut set, _feed) = a_set();
        ask_ssh(&mut set, "h1");
        assert_eq!(
            set.generation("h1"),
            1,
            "the attempt is numbered at the handshake, before any tunnel"
        );

        // The connect a working tunnel reaches is the same attempt, so
        // it must not mint a second number.
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-sshgen.sock"),
            HostTransport::Ssh,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        assert_eq!(set.generation("h1"), 1, "one attempt is one generation");
        let incarnation = set.mint_for("h1");
        set.apply_state(incarnation, HostConnState::Connected);

        set.apply_state(incarnation, dropped("the connection closed"));
        assert_eq!(
            set.generation("h1"),
            1,
            "a drop is what starts a ladder, not an attempt of its own"
        );

        // Each rung dials, and counts whether or not it comes up.
        assert!(retry_once(&mut set));
        assert_eq!(set.generation("h1"), 2);
        refuse(&mut set, unreachable());
        assert_eq!(
            set.generation("h1"),
            2,
            "an establish that never came up is still that attempt"
        );
        assert!(retry_once(&mut set));
        assert_eq!(set.generation("h1"), 3, "and the next rung is the next");
    }

    /// Eligibility is the *outage*, and a host that never worked has no
    /// outage to inherit. A first connect that fails is a question for
    /// the person who asked, not a ladder.
    #[tokio::test]
    async fn a_connection_that_never_worked_schedules_nothing() {
        let (mut set, _feed) = a_set();
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        refuse(&mut set, unreachable());
        assert!(
            !set.has_outage("h1"),
            "nothing about this host has ever worked, so nothing is owed a retry"
        );
    }

    /// A changed host key is never retried, at either end of a ladder's
    /// life: not as the first drop, and not as the answer to a retry
    /// that was already scheduled — where it also *ends* the outage
    /// rather than leaving a dead entry behind.
    ///
    /// The band keeps the family's own copy. "Must not be tried" and
    /// "gave up trying" are different things to tell somebody about a
    /// possible machine-in-the-middle.
    #[tokio::test]
    async fn a_changed_host_key_arms_nothing_and_settles_a_ladder_that_had_started() {
        let (mut set, _feed) = a_set();

        // A first connect, before anything ever worked.
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        refuse(&mut set, SshFailure::ChangedHostKey);
        assert!(!set.has_outage("h1"));

        // And mid-ladder, on a host that had been working.
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-key.sock");
        assert!(set.has_outage("h1"), "the drop started a ladder");

        assert!(retry_once(&mut set), "and its first retry dialed");
        refuse(&mut set, SshFailure::ChangedHostKey);
        assert!(
            !set.has_outage("h1"),
            "a family no retry may be spent on ends the outage"
        );
        assert_eq!(
            band_reason(&set, "h1"),
            SshFailure::ChangedHostKey.message("workbox"),
            "and the band keeps the family's own copy, not a give-up line"
        );
    }

    /// **The §3.2 regression.** `open_ssh` installs a fresh `SshState`
    /// with `reached_connected: false` on every attempt, so an
    /// eligibility check re-read at the second failure finds `false` and
    /// refuses — the ladder stops dead after one retry, and every
    /// budget, give-up and copy rule below it becomes unreachable. The
    /// outage entry's *existence* is the record of eligibility, and this
    /// is what proves it survives the retry's own `open_ssh`.
    #[tokio::test]
    async fn a_ladder_survives_its_own_retrys_open_ssh() {
        let (mut set, _feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-survive.sock");
        assert_eq!(set.outage("h1").ladder.attempts(), 1);

        assert!(retry_once(&mut set));
        assert!(
            !set.ssh("h1").reached_connected,
            "the fresh entry knows nothing — which is the trap"
        );

        // The retry's tunnel comes up and its connection drops again.
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-survive.sock"),
            HostTransport::Ssh,
            ConnectMode::Dial,
            AttemptCause::AutoReconnect,
        );
        let second = set.mint_for("h1");
        set.apply_state(second, dropped("and it dropped again"));

        assert_eq!(
            set.outage("h1").ladder.attempts(),
            2,
            "the ladder continued instead of refusing a host that had worked"
        );
        assert!(set.outage("h1").armed.is_some());
    }

    /// **The K1.1 regression, and the give-up.** A retry whose
    /// *establish* fails is the common case — the network is usually
    /// still down — so `tunnel_ready`'s `Err` arm is the second schedule
    /// entry point, and the second failed retry has to re-arm exactly
    /// like the first. A ladder that stalls there never reaches its
    /// budget, and the give-up copy never renders.
    ///
    /// It also pins the two things that end an outage: the entry goes at
    /// `Exhausted`, lease included, since no timer is armed in that
    /// state and nothing else would ever clean it.
    #[tokio::test]
    async fn repeated_establish_failures_run_the_ladder_to_its_budget() {
        let (mut set, _feed) = a_set();
        let incarnation = a_connected_ssh_host(&mut set, "/nonexistent/roost-set-budget.sock");
        set.apply_lease(incarnation, "lease-1".into());
        set.apply_state(incarnation, dropped("the connection closed"));
        let budget = budget(&set);

        for attempt in 1..=budget {
            assert!(
                retry_once(&mut set),
                "attempt {attempt} of {budget} must be dialed"
            );
            refuse(&mut set, unreachable());
            if attempt < budget {
                assert_eq!(
                    set.outage("h1").ladder.attempts(),
                    attempt + 1,
                    "the failed retry re-arms rather than stalling"
                );
                assert!(set.outage("h1").armed.is_some());
                assert_eq!(
                    retry_reason(&set).as_deref(),
                    Some(unreachable().message("workbox").as_str()),
                    "and every rung after the first says why it is armed (#399)"
                );
            }
        }

        assert!(
            !set.has_outage("h1"),
            "the ladder settles when its budget is spent"
        );
        assert_eq!(
            band_reason(&set, "h1"),
            format!(
                "reconnect gave up after {budget} tries — {}",
                unreachable().message("workbox")
            ),
            "and says so through the band, not only in a field"
        );
        assert_eq!(band_state(&set, "h1").retry_in, None);
        assert_eq!(
            set.retry_schedule("h1"),
            None,
            "the rung's own reason went with the outage — nothing is armed to explain"
        );
        assert!(
            set.carried_lease("h1", AttemptCause::AutoReconnect)
                .is_none(),
            "the lease went with the outage it belonged to"
        );
    }

    /// The band has to *move*. `section_reason` prefers the live
    /// connection's own reason, and the dead conn from the first drop
    /// stays in `conns` until a successful establish reaches `connect` →
    /// `forget` — so a ladder that wrote only `SshState.failure` would
    /// show attempt one's line forever and neither `(2/N)` nor the
    /// give-up copy would ever appear.
    #[tokio::test]
    async fn the_band_reason_advances_with_the_attempt() {
        let (mut set, _feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-copy.sock");
        let budget = budget(&set);
        let mut seen = vec![band_reason(&set, "h1")];

        for _ in 0..2 {
            assert!(retry_once(&mut set));
            refuse(&mut set, unreachable());
            seen.push(band_reason(&set, "h1"));
        }

        assert!(seen[0].ends_with(&format!("(1/{budget})")), "{seen:?}");
        assert!(seen[1].ends_with(&format!("(2/{budget})")), "{seen:?}");
        assert!(seen[2].ends_with(&format!("(3/{budget})")), "{seen:?}");
    }

    /// The staleness stamp rides the timer's **own message**, read off
    /// the entry at arm time — and it has to still match when the timer
    /// fires, or the ladder stalls at its first failed retry. Every
    /// other case here recomputes the stamp; this is the one that lets
    /// the timer actually run and reads what the arm really put on the
    /// wire.
    #[tokio::test(start_paused = true)]
    async fn an_armed_timer_carries_a_stamp_the_entry_still_recognizes() {
        let (mut set, mut feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-stamp.sock");

        // Nothing in this suite may dial a real host, and this is the
        // one case that lets the runtime run: stop the handshake
        // `open_ssh` spawned before anything is polled.
        for entry in set.entries.values_mut().filter_map(|e| e.ssh.as_mut()) {
            if let Some(establish) = entry.establish.take() {
                establish.abort();
            }
        }
        let delay = set.outage("h1").armed.as_ref().expect("armed").delay;
        // The clock is paused, so this is instant.
        tokio::time::sleep(delay + Duration::from_secs(1)).await;

        let mut batch = crate::engine_feed::EngineBatch::default();
        let mut due = None;
        while let Some(item) = feed.try_next(&mut batch) {
            if let crate::engine_feed::EngineFeed::ReconnectDue { host, request } = item {
                due = Some((host, request));
            }
        }
        let (host, request) = due.expect("the armed timer put a due message on the feed");
        assert_eq!(host, "h1");
        assert!(
            set.reconnect_due(&host, request).is_some(),
            "the stamp the timer carried is the one the entry recognizes"
        );
    }

    /// A due message has to prove the host is still where it was when
    /// the timer was armed. Three ways it can fail to: the user
    /// disconnected (the entry is gone — which is why the test is
    /// `is_some_and` and not `is_none_or`, or a fired timer would
    /// resurrect the host), the user connected instead (the request
    /// moved on), or the connection came back on its own (the outage is
    /// over).
    #[tokio::test]
    async fn a_due_retry_that_lost_its_race_dials_nothing() {
        let (mut set, _feed) = a_set();

        // Disconnected in between.
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-race-a.sock");
        let armed = set.ssh("h1").request;
        set.disconnect("h1");
        assert!(
            set.reconnect_due("h1", armed).is_none(),
            "a fired timer must not resurrect a host the user just disconnected"
        );

        // An explicit connect in between.
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-race-b.sock");
        let armed = set.ssh("h1").request;
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        assert!(set.reconnect_due("h1", armed).is_none());

        // Connected again in between: the request is untouched, and the
        // outage is what is gone.
        let incarnation = a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-race-c.sock");
        let armed = set.ssh("h1").request;
        set.apply_state(incarnation, HostConnState::Connected);
        assert!(!set.has_outage("h1"), "a success ends the outage");
        assert!(set.reconnect_due("h1", armed).is_none());
    }

    /// §3.4's why-safe walk, pinned as a test rather than as a gate.
    ///
    /// An explicit reconnect of a still-*connected* host leaves the old
    /// task alive for a moment, and that task publishes `Disconnected`
    /// when its tunnel is torn down underneath it — under the same
    /// generation, which `owner_of` does not filter. Nothing re-arms,
    /// because the *creation* gate reads `reached_connected` off the
    /// fresh `SshState` the explicit connect just installed, and that is
    /// `false` by construction.
    #[tokio::test]
    async fn a_superseded_tasks_late_drop_does_not_arm_a_ladder() {
        let (mut set, _feed) = a_set();
        let live = a_connected_ssh_host(&mut set, "/nonexistent/roost-set-late.sock");

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        assert!(
            set.owner_of(live).is_some(),
            "the old connection is still held — that is the window"
        );

        set.apply_state(live, dropped("the connection closed"));
        assert!(
            !set.has_outage("h1"),
            "the user's own attempt is the schedule now"
        );
        assert_eq!(band_state(&set, "h1").retry_in, None);
    }

    /// The fire-time re-check (§3.4), which is the belt to C1's braces.
    ///
    /// One client runs several concurrent `ssh` execs, so a graver
    /// family can be recorded *after* the EOF the connection task saw.
    /// The armed timer re-reads the dead tunnel's slot before it dials:
    /// a family no retry may be spent on settles the outage on its own
    /// copy instead.
    #[tokio::test]
    async fn a_graver_family_found_at_fire_time_settles_instead_of_dialing() {
        let (mut set, _feed) = a_set();
        let incarnation = a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-late-family.sock");
        let armed = set.ssh("h1").request;

        // A retryable one still dials: the re-check refuses, it does not
        // simply distrust anything it finds.
        set.outage_mut("h1").dead_tunnel = Some(DeadTunnel::Recorded {
            generation: 9,
            failure: SshFailure::Transport(Some("broken pipe".into())),
            truncated: false,
            seen: 4,
        });
        assert!(set.reconnect_due("h1", armed).is_some());

        // Re-arm, then let a changed host key land in the same slot.
        set.apply_state(incarnation, dropped("the connection closed"));
        let armed = set.ssh("h1").request;
        set.outage_mut("h1").dead_tunnel = Some(DeadTunnel::Recorded {
            generation: 9,
            failure: SshFailure::ChangedHostKey,
            truncated: false,
            seen: 4,
        });
        assert!(
            set.reconnect_due("h1", armed).is_none(),
            "a possible machine-in-the-middle is not dialed again"
        );
        assert!(!set.has_outage("h1"));
        assert_eq!(
            band_reason(&set, "h1"),
            SshFailure::ChangedHostKey.message("workbox")
        );
        assert_eq!(
            set.ssh_failure("h1"),
            Some(&SshFailure::ChangedHostKey),
            "and the family is recorded, not only rendered"
        );
    }

    /// §3.5's suspend reset. A lid closed a second after the drop wakes
    /// with the timer firing at once, and attempts 2–10 would burn while
    /// the radio is still associating — settling the host exactly as the
    /// network comes back.
    ///
    /// It re-arms at the base delay and does **not** dial: a
    /// reset-then-dial spends a full `ConnectTimeout 15` establish
    /// against a radio that has no route yet, which is precisely the
    /// attempt the reset exists to protect.
    #[tokio::test]
    async fn a_suspend_shaped_skew_restarts_the_ladder_without_dialing() {
        let (mut set, _feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-suspend.sock");
        for _ in 0..2 {
            assert!(retry_once(&mut set));
            refuse(&mut set, unreachable());
        }
        assert_eq!(set.outage("h1").ladder.attempts(), 3, "deep enough to tell");
        let deep = set.outage("h1").armed.as_ref().expect("armed").delay;
        assert!(deep > Duration::from_secs(1), "{deep:?} is off the base");

        // The machine slept.
        let armed = set.outage_mut("h1").armed.as_mut().expect("armed");
        armed.at = SystemTime::now() - Duration::from_secs(3_600);
        let stamp = set.ssh("h1").request;

        assert!(
            set.reconnect_due("h1", stamp).is_none(),
            "a woken ladder gives the network its beat before dialing"
        );
        assert_eq!(set.outage("h1").ladder.attempts(), 1, "a new outage");
        let rearmed = set.outage("h1").armed.as_ref().expect("re-armed");
        assert!(
            rearmed.delay <= Duration::from_secs(1),
            "{:?} is not the base delay",
            rearmed.delay
        );
        assert_eq!(
            band_reason(&set, "h1"),
            format!("reconnecting in 1s (1/{})", budget(&set))
        );
        // A new *outage* only in the ladder's arithmetic — nothing
        // dropped here, so the last classified failure is still the
        // honest answer to why this host is retrying (#399).
        assert_eq!(
            retry_reason(&set).as_deref(),
            Some(unreachable().message("workbox").as_str()),
            "the reset carries the family across; it is the same outage"
        );
    }

    /// A terminal settle ends the outage, lease and all. Nothing is
    /// armed in `TakenOver`, so if the entry did not go here nothing
    /// would ever clean it — and the lease it holds belongs to a
    /// connection somebody else now owns.
    #[tokio::test]
    async fn a_terminal_settle_takes_the_outage_with_it() {
        let (mut set, _feed) = a_set();
        let incarnation = a_connected_ssh_host(&mut set, "/nonexistent/roost-set-terminal.sock");
        set.apply_lease(incarnation, "lease-1".into());
        set.apply_state(incarnation, dropped("the connection closed"));
        assert!(set.has_outage("h1"));

        set.apply_state(incarnation, HostConnState::TakenOver);
        assert!(!set.has_outage("h1"));
        assert!(set
            .carried_lease("h1", AttemptCause::AutoReconnect)
            .is_none());
    }

    /// The exit teardown, both windows (§3.4). An armed timer firing
    /// during the teardown would spawn a fresh `ssh` master; a quit
    /// timed into an establish already running would leave a
    /// just-daemonized `ControlPersist=60s` one behind. Neither is
    /// covered by the other.
    #[tokio::test]
    async fn the_exit_teardown_stops_both_a_waiting_retry_and_a_running_establish() {
        let (mut set, _feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-exit.sock");

        let timer = set
            .outage("h1")
            .armed
            .as_ref()
            .expect("a retry is waiting")
            .handle
            .clone();
        let establish = set
            .ssh("h1")
            .establish
            .clone()
            .expect("an establish is in flight");
        assert!(!timer.is_finished() && !establish.is_finished());

        set.abandon_reconnects();
        // Both are aborted before anything is polled, which is what
        // keeps this case from dialing a real host.
        tokio::task::yield_now().await;

        assert!(timer.is_finished(), "the armed retry never fires");
        assert!(
            establish.is_finished(),
            "and the handshake stops where it is"
        );
        assert!(set.entries.values().all(|e| e.outage.is_none()));
        assert!(set.ssh("h1").establish.is_none());
    }

    /// The half of the teardown the walk over `ssh` cannot see.
    ///
    /// Dropping an `AbortHandle` does not abort its task, and both
    /// places an `SshState` is displaced used to drop one: `open_ssh`
    /// replaces the entry, `disconnect` removes it. While the app runs
    /// that is the design — the establish lands and `tunnel_ready`
    /// discards it — but at exit nothing will ever drain its answer, so
    /// a just-daemonized `ControlPersist=60s` master would outlive the
    /// app with nothing left holding its handle.
    #[tokio::test]
    async fn the_exit_teardown_reaches_establishes_whose_entry_has_gone() {
        let (mut set, _feed) = a_set();

        // Displaced by a second attempt on the same host.
        ask_ssh(&mut set, "h1");
        let superseded = in_flight(&set, "h1");
        ask_ssh(&mut set, "h1");
        let current = in_flight(&set, "h1");

        // Displaced by an explicit disconnect.
        ask_ssh(&mut set, "h2");
        let disconnected = in_flight(&set, "h2");
        set.disconnect("h2");

        assert!(!superseded.is_finished() && !disconnected.is_finished());
        assert!(!set.has_ssh("h2"), "the entry went with the host");

        set.abandon_reconnects();
        // Every handle is aborted before anything is polled, which is
        // what keeps this case from dialing a real host.
        tokio::task::yield_now().await;

        assert!(
            superseded.is_finished(),
            "the establish a second Connect displaced is not left running"
        );
        assert!(
            disconnected.is_finished(),
            "and neither is the one whose host was disconnected"
        );
        assert!(current.is_finished(), "nor the one still on its entry");
        assert!(set.displaced.is_empty());
    }

    /// A displaced establish is parked, never aborted, while the app is
    /// running: aborting one mid-flight drops a half-built `SshTunnel`
    /// inside the aborted task, which runs its *blocking* `Drop` on a
    /// runtime worker — the very thing `discard_tunnel` exists to avoid.
    /// And the parking may not accumulate across a long session.
    #[tokio::test]
    async fn parking_a_displaced_establish_neither_aborts_it_nor_grows_forever() {
        let (mut set, _feed) = a_set();

        ask_ssh(&mut set, "h1");
        let superseded = in_flight(&set, "h1");
        ask_ssh(&mut set, "h1");
        ask_ssh(&mut set, "h2");
        let orphaned = in_flight(&set, "h2");
        set.disconnect("h2");
        assert!(
            !superseded.is_finished() && !orphaned.is_finished(),
            "a displaced establish is left to land and be discarded"
        );
        assert_eq!(set.displaced.len(), 2);

        // Both parked handles have answered now. Nothing sweeps them on
        // a timer — the next displacement does, so the list measures
        // what is in flight rather than every attempt this session made.
        superseded.abort();
        orphaned.abort();
        in_flight(&set, "h1").abort();
        tokio::task::yield_now().await;

        ask_ssh(&mut set, "h1");
        assert!(
            set.displaced.is_empty(),
            "finished handles are swept on the way in, not accumulated"
        );
        set.abandon_reconnects();
    }

    /// The window `abandon_reconnects` cannot close (§3.4).
    /// `EngineFeed::Quit` only latches the exit — the drain keeps
    /// running — so an establish that answers behind it still reaches
    /// the UI thread, where `tunnel_ready` would dial a session while
    /// the app tears down. The exit path routes it here instead, and a
    /// tunnel that *did* come up is retired rather than connected or
    /// leaked: its scratch directory goes with it.
    ///
    /// (The `exit_state` branch that chooses this path lives on `App`,
    /// which a unit test cannot build; C5's lane covers that leg.)
    #[tokio::test]
    async fn an_establish_answering_during_shutdown_is_discarded_rather_than_dialed() {
        let (mut set, _feed) = a_set();
        ask_ssh(&mut set, "h1");
        let request = set.ssh("h1").request;
        // Nothing in this suite may dial a real host, and this case has
        // to let the runtime run: stop the handshake `open_ssh` spawned
        // before anything is polled. The spent handle stays on the
        // entry, which is what a real answer finds.
        in_flight(&set, "h1").abort();

        let parent = tempfile::Builder::new()
            .prefix("roost-set-discard")
            .tempdir()
            .expect("a scratch parent");
        let tunnel = an_unestablished_tunnel(parent.path().to_path_buf()).await;
        let scratch = tunnel
            .bridge_socket()
            .parent()
            .expect("a tunnel's socket sits in its scratch directory")
            .to_path_buf();
        assert!(scratch.is_dir(), "{}", scratch.display());

        set.discard_ready(HostTunnelReady {
            host: "h1".to_string(),
            request,
            result: Ok(tunnel),
        });

        assert!(
            !set.has_conn("h1"),
            "a tunnel that lands during shutdown must not dial a session"
        );
        assert!(
            set.ssh("h1").establish.is_none(),
            "and the spent handle comes off the entry"
        );
        // Retired on the engine runtime rather than dropped here — the
        // `-O exit` and this removal are the difference between a
        // discard and a leak.
        tokio::time::timeout(Duration::from_secs(5), async {
            while scratch.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the discarded tunnel's scratch directory is removed");
    }

    /// One `open_ssh` as the app drives it, with nothing riding on the
    /// origin or the cause.
    fn ask_ssh(set: &mut HostConnSet, host: &str) {
        set.open_ssh(
            host,
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
    }

    fn in_flight(set: &HostConnSet, host: &str) -> AbortHandle {
        set.ssh(host)
            .establish
            .clone()
            .expect("an establish is in flight")
    }

    /// Attendedness is the cause, not the origin. `roostctl host
    /// connect` arrives as `Ipc` + `Dial` — exactly what an
    /// auto-reconnect looks like — and the status line raised here is
    /// its **only** failure surface, because `host.connect`'s reply
    /// carries no outcome. An origin rule would silence it; the cause
    /// rule keeps it while making every ladder attempt quiet.
    #[tokio::test]
    async fn a_ladder_is_quiet_while_roostctls_own_connect_still_speaks() {
        let (mut set, _feed) = a_set();

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::Ipc,
            AttemptCause::Explicit,
        );
        let request = set.ssh("h1").request;
        assert!(set.ssh("h1").attended);
        assert_eq!(
            set.tunnel_ready(failed("h1", request, "workbox is unreachable"))
                .as_deref(),
            Some("workbox is unreachable"),
            "roostctl host connect hears why it failed"
        );

        set.open_ssh(
            "h2",
            "two",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::Ipc,
            AttemptCause::AutoReconnect,
        );
        let request = set.ssh("h2").request;
        assert!(!set.ssh("h2").attended);
        assert_eq!(
            set.tunnel_ready(failed("h2", request, "workbox is unreachable")),
            None,
            "and a ladder says nothing, ten times over"
        );
    }

    /// **The consent property, over the values a real ladder leaves
    /// behind** — not `bootstrap_offer`'s decision on hand-made
    /// arguments, and not its accessor reads left unchecked. Every
    /// auto-reconnect stamps `RequestOrigin::Ipc`, and that is the one
    /// gate that holds on every attempt: `reached_connected` is `false`
    /// from the second attempt on (its own `open_ssh` cleared it), and
    /// attempt *k* can carry a family attempt one never had.
    #[tokio::test]
    async fn no_attempt_of_a_ladder_can_raise_a_consent_card() {
        use crate::app::host_lifecycle::bootstrap_offer;
        let (mut set, _feed) = a_set();
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-consent.sock");

        // An offer-able family on the *first* retry. It is a family
        // attempt zero never had — the drop was a bare EOF — which is
        // the case the withdrawn "offer-able families are not retryable"
        // proof missed.
        assert!(retry_once(&mut set));
        refuse(&mut set, SshFailure::NoSession);
        assert_eq!(set.ssh_failure("h1"), Some(&SshFailure::NoSession));
        assert_eq!(
            bootstrap_offer(&set, "h1"),
            None,
            "a machine asked for this attempt, so no card"
        );

        // And again at the bottom of a full ladder, where
        // `reached_connected` has been `false` for nine attempts and the
        // origin is the only gate still standing.
        a_dropped_ssh_host(&mut set, "/nonexistent/roost-set-consent.sock");
        let budget = budget(&set);
        while set.outage("h1").ladder.attempts() < budget - 1 {
            assert!(retry_once(&mut set));
            assert!(
                !set.ssh_reached_connected("h1"),
                "the flag `offer_for` refuses on is already false"
            );
            refuse(&mut set, unreachable());
        }
        assert!(retry_once(&mut set));
        refuse(&mut set, SshFailure::NoSession);
        assert_eq!(
            bootstrap_offer(&set, "h1"),
            None,
            "the last attempt cannot raise a card either"
        );
    }

    /// The establish window answers `connecting`, not `disconnected`:
    /// `host.connect` documents that reply, and the attempt is under way
    /// even though no `HostConn` exists yet.
    #[tokio::test]
    async fn an_establish_in_flight_reads_as_connecting() {
        let (mut set, _feed) = a_set();
        assert!(!set.establishing("h1"), "nothing asked for yet");
        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        let request = set.ssh("h1").request;
        assert!(set.establishing("h1"), "the establish is in flight");
        set.tunnel_ready(failed("h1", request, "workbox refused authentication"));
        assert!(
            !set.establishing("h1"),
            "a settled failure is disconnected, not connecting"
        );
        set.disconnect("h1");
        assert!(!set.establishing("h1"));
    }

    /// A third party moving the session's active row must clear the
    /// focus dedup — and only a genuine disagreement does; the echo of
    /// this client's own claim changes nothing.
    #[tokio::test]
    async fn an_active_move_away_from_the_claim_clears_the_dedup() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-focus.sock"),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let incarnation = set.mint_for("h1");
        set.apply_state(incarnation, HostConnState::Connected);
        set.conn_mut("h1").focus_sent = Some(Some(5));

        assert!(
            !set.focus_claim_disagrees(incarnation, 5),
            "the claim's own echo is not a disagreement"
        );
        assert_eq!(set.conn("h1").focus_sent, Some(Some(5)));

        assert!(set.focus_claim_disagrees(incarnation, 9));
        assert_eq!(
            set.conn("h1").focus_sent,
            None,
            "a disagreement clears the dedup so the re-push goes out"
        );
        assert!(
            !set.focus_claim_disagrees(incarnation, 9),
            "nothing claimed, nothing to disagree with"
        );
    }

    /// Disconnecting drops everything keyed on the host, so a late item
    /// from its task lands nowhere.
    #[tokio::test]
    async fn a_disconnected_host_stops_accepting_its_own_items() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-drop.sock"),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let incarnation = set.mint_for("h1");
        set.apply_state(incarnation, HostConnState::Connected);
        set.apply_workspace(incarnation, HostWorkspaceEvent::Reset(Arc::default()));

        set.disconnect("h1");
        assert!(set.is_empty());
        assert!(set.mirror(incarnation).is_none());
        assert_eq!(set.apply_state(incarnation, HostConnState::Connected), None);
        assert!(set.ops("h1").is_none());
        assert!(set.ops_for(incarnation).is_none());
    }

    /// The attribution the *drain* asks before it applies a batch's
    /// envelopes. Those reach surfaces no purge can take back — a
    /// desktop banner, the clipboard — so "is this incarnation still
    /// live?" has to be answerable without applying anything, and it has
    /// to answer the same way `apply_workspace` does for all three
    /// staleness cases.
    #[tokio::test]
    async fn ownership_is_answerable_before_anything_is_applied() {
        let (mut set, _feed) = a_set();
        let socket = PathBuf::from("/nonexistent/roost-set-owns.sock");
        set.connect(
            "h1",
            "one",
            socket.clone(),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let live = set.mint_for("h1");
        set.apply_state(live, HostConnState::Connected);
        assert!(set.owns(live));

        // Never minted by this set at all.
        assert!(!set.owns(HostId::new(9_999)));

        // Replaced: the outgoing task can still publish for a while.
        let replaced = set.minter.mint("h1", set.conn("h1").generation - 1);
        assert!(!set.owns(replaced));

        // Removed: the host is gone, and so is every id it minted.
        assert_eq!(set.remove("h1"), Some(live));
        assert!(!set.owns(live));
    }

    /// Zero hosts is the zero-change baseline: nothing is spawned, and
    /// every accessor answers empty rather than inventing a host.
    #[tokio::test]
    async fn a_set_with_no_hosts_is_inert() {
        let (set, _feed) = a_set();
        assert!(set.is_empty());
        assert_eq!(set.connected().count(), 0);
        assert!(set.state("anything").is_none());
        assert!(set.endpoint("anything").is_none());
        assert!(set.mirror(HostId::new(1)).is_none());
        assert!(set.section("anything").is_none());
    }

    /// What the dimmed section is built on: a dropped connection keeps
    /// the rows it published, because the shells they name are still
    /// running on the host. A *reconnect* is the one thing that clears
    /// them — the fresh `tab.list` is authoritative, so the rebuild is
    /// purge-then-rebuild rather than a merge.
    #[tokio::test]
    async fn a_dropped_connection_keeps_its_rows_for_the_dimmed_section() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "pop-os",
            PathBuf::from("/nonexistent/roost-set-section.sock"),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let incarnation = set.mint_for("h1");
        set.apply_state(incarnation, HostConnState::Connected);
        set.apply_workspace(incarnation, HostWorkspaceEvent::Reset(Arc::default()));

        set.apply_state(
            incarnation,
            HostConnState::Disconnected(state::Disconnected {
                reason: "connection refused".into(),
                detail: None,
                retry_in: None,
            }),
        );
        let section = set.section("h1").expect("a saved host keeps its section");
        assert_eq!(section.label, "pop-os");
        assert!(!section.state.is_connected());
        assert_eq!(section.incarnation, Some(incarnation));
        assert!(
            section.mirror.is_some(),
            "the rows outlive the connection that published them"
        );
        assert_eq!(
            set.connected().count(),
            0,
            "but the host contributes nothing that can be acted on"
        );

        let fresh = set.mint_for("h1");
        set.apply_state(
            fresh,
            HostConnState::Connecting {
                previous: Some(incarnation),
            },
        );
        let section = set.section("h1").expect("still saved");
        assert!(
            section.mirror.is_none(),
            "a reconnect purges rather than merging (plan 037 §3.2)"
        );
    }

    /// An **explicit** disconnect follows the same rule as a dropped
    /// connection: the rows stay listed (dimmed), because the session
    /// still holds those shells (plan 037 §3.1). It is the path that
    /// removes the `HostConn` the rows normally hang off, so it is the
    /// one that can silently empty the section — the regression this
    /// pins. Reconnect still replaces them; remove still clears them.
    #[tokio::test]
    async fn an_explicit_disconnect_keeps_the_rows_a_reconnect_replaces_and_remove_clears() {
        let (mut set, _feed) = a_set();
        use roost_ui_model::host_sidebar::SectionState;
        let socket = PathBuf::from("/nonexistent/roost-set-disconnect-rows.sock");

        set.connect(
            "h1",
            "pop-os",
            socket.clone(),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let incarnation = set.mint_for("h1");
        set.apply_state(incarnation, HostConnState::Connected);
        set.apply_workspace(incarnation, HostWorkspaceEvent::Reset(Arc::default()));

        assert_eq!(set.disconnect("h1"), Some(incarnation));
        let section = set
            .section("h1")
            .expect("a disconnected host still has a section to dim");
        assert_eq!(section.label, "pop-os");
        assert_eq!(section.state.section_state(), SectionState::Disconnected);
        assert_eq!(section.incarnation, Some(incarnation));
        assert!(
            section.mirror.is_some(),
            "the rows outlive an explicit disconnect, exactly as they \
             outlive a dropped connection"
        );
        // Retained means *rendered*, never *actionable*: nothing about a
        // dimmed section may still accept ops or be resurrected by a
        // straggler from the connection that published it.
        assert!(set.is_empty());
        assert!(set.ops("h1").is_none());
        assert!(set.ops_for(incarnation).is_none());
        assert_eq!(set.apply_state(incarnation, HostConnState::Connected), None);
        assert_eq!(set.connected().count(), 0);

        // Reconnect: purge-then-rebuild, so the retained rows go with the
        // incarnation that published them.
        set.connect(
            "h1",
            "pop-os",
            socket.clone(),
            HostTransport::UnixSocket,
            ConnectMode::Dial,
            AttemptCause::Explicit,
        );
        let section = set.section("h1").expect("connecting hosts have sections");
        assert!(
            section.mirror.is_none(),
            "a reconnect rebuilds from a fresh tab.list (plan 037 §3.2)"
        );

        // Remove: the rows go with the host.
        let live = set.mint_for("h1");
        set.apply_state(live, HostConnState::Connected);
        set.apply_workspace(live, HostWorkspaceEvent::Reset(Arc::default()));
        assert_eq!(set.remove("h1"), Some(live));
        assert!(
            set.section("h1").is_none(),
            "a forgotten host has no section to keep rows in"
        );
    }

    /// The representation invariant the six parallel maps could not
    /// state: an entry appears only where something is *inserted*, and
    /// goes only at `remove`.
    ///
    /// Each half is a real hazard of the one-entry shape. Routing a
    /// clearing write through the entry API would leave a phantom entry
    /// for a host nothing ever touched; pruning an entry once its
    /// fields are gone would take the generation with it, and a poller
    /// watching that number across a disconnect would see it fall back
    /// to `0` and count the next attempt twice; and reading `is_empty`
    /// off the map rather than off the live connections would call a
    /// host with an establish in flight "connected".
    #[tokio::test]
    async fn an_entry_is_created_only_by_an_insert_and_removed_only_by_remove() {
        let (mut set, _feed) = a_set();

        // Clearing what a host does not have is a no-op, not a create.
        set.set_bootstrap_note("h9", None);
        set.clear_outage("h9");
        assert!(
            !set.entries.contains_key("h9"),
            "clearing a note or an outage on an unknown host conjured an entry"
        );

        set.open_ssh(
            "h1",
            "one",
            ssh_target("workbox"),
            ConnectMode::Dial,
            RequestOrigin::User,
            AttemptCause::Explicit,
        );
        // Nothing in this suite may dial a real host: stop the handshake
        // `open_ssh` spawned before anything is polled.
        for entry in set.entries.values_mut().filter_map(|e| e.ssh.as_mut()) {
            if let Some(establish) = entry.establish.take() {
                establish.abort();
            }
        }

        // An ssh-only entry is not a connection.
        assert!(
            set.is_empty(),
            "a host with an establish in flight and no connection reads as connected"
        );
        let minted = set.generation("h1");
        assert!(minted > 0, "the attempt was never numbered");

        set.disconnect("h1");
        assert!(
            set.entries.contains_key("h1"),
            "disconnect took the entry the generation has to outlive"
        );
        assert_eq!(
            set.generation("h1"),
            minted,
            "a disconnect reset the number a poller counts edges on"
        );

        set.remove("h1");
        assert!(
            !set.entries.contains_key("h1"),
            "remove left the host's entry behind"
        );
        assert_eq!(
            set.generation("h1"),
            0,
            "a removed host still remembers an attempt"
        );
    }

    #[test]
    fn a_theme_renders_as_the_wires_hex_spelling() {
        let theme = Theme::roost_dark();
        let colors = theme_colors(&theme);
        assert_eq!(
            colors.palette.len(),
            256,
            "a short palette is invalid-param"
        );
        for value in std::iter::once(&colors.foreground)
            .chain([&colors.background, &colors.cursor])
            .chain(colors.palette.iter())
        {
            assert_eq!(value.len(), 7, "{value}");
            assert!(value.starts_with('#'), "{value}");
            assert!(value[1..].chars().all(|c| c.is_ascii_hexdigit()), "{value}");
        }
        assert_eq!(
            colors.background,
            format!(
                "#{:02x}{:02x}{:02x}",
                theme.background.r, theme.background.g, theme.background.b
            )
        );
    }
}
