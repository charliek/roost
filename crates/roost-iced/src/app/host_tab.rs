//! The focused host tab's attach: queue permit → data dial → hydration
//! → live, with resume across refocus (host-sessions plan 037 §3.4).
//!
//! Ownership is split the way the threading rules demand (CLAUDE.md):
//! background tasks only move bytes — the dial task takes the permit and
//! runs the handshake, a reader task turns data-plane frames into
//! [`HostTabFrame`]s on the engine feed, a writer task drains the tab's
//! input queue onto the wire — while the [`Hydrator`] and both terminals
//! (the old one still rendering, the new one hydrating) live in
//! [`HostAttach`] on the main thread and are driven from the feed drain.
//!
//! Which hydration a payload gets is the *server's* answer, not this
//! client's preference: the data connection's handshake names the
//! negotiated [`PayloadKind`], and a session whose libghostty build
//! disagrees with ours answers `vt` — bytes to replay — where a matching
//! one answers a snapshot to decode.
//!
//! Every frame carries the `attempt` that produced it. A re-attach
//! aborts the previous attempt's tasks, but an abort is asynchronous —
//! frames already on the feed from the dead attempt must land somewhere
//! harmless, and the attempt check is that somewhere. The same shape at
//! one level up: a whole [`TabKey`] from a dead connection incarnation
//! misses the app's attach map entirely (the `HostId` staleness
//! contract), so neither a stale attempt nor a stale incarnation can
//! touch a live terminal.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use roost_ipc::client::{ClientError, DataConnection, ServerCode, ServerFrame};
use roost_ipc::messages::{
    AttachHandshake, AttachHandshakeTerms, AttachPayloadKind, SizeOwner, SizeTraceAction,
};
use roost_ui_model::keys::TabKey;
use roost_vt::{HistoryStep, ReadyState, SnapshotDecodeOptions, SnapshotDecoder, Terminal};
use tokio::sync::{mpsc, watch};

use super::tab_backend::HostDataMsg;
use crate::engine_feed::{EngineFeed, EngineFeedSender};
use crate::host_conn::queue::{AttachPermit, HostOpError, HostOps};
use crate::host_conn::state::CLIENT_PAYLOAD_KINDS;

/// History records (pages or image payloads) stepped per feed-drain
/// pass. Bounds main-thread work so a large-scrollback attach never
/// stalls a frame; the drain re-arms itself with
/// [`HostTabFrame::StepDecoder`] while records remain.
const PAGES_PER_PASS: usize = 8;

/// The pre-swap resize withhold (architecture §5: a resize mid-snapshot
/// forfeits the remaining history pages, so a withheld one is sent when
/// the hydration completes — but never held longer than this).
const WITHHOLD_DEADLINE: Duration = Duration::from_secs(2);
/// How long a detach leaves the data connection's writer to put `DETACH`
/// on the wire before the attempt is aborted (plan 084 §4.B).
pub(super) const DETACH_DRAIN: Duration = Duration::from_millis(200);

/// Give every detach queued so far its [`DETACH_DRAIN`] on `runtime`,
/// from a thread outside it — the window's teardown, after which the
/// runtime goes. The sleep is built inside the runtime: a timer made
/// outside one panics.
pub(super) fn wait_out_detach_drains(runtime: &tokio::runtime::Runtime) {
    runtime.block_on(async { tokio::time::sleep(DETACH_DRAIN).await });
}

/// How long a local scroll of another client's tab must hold still
/// before it takes the tab's size (plan 084 §4.B).
const SCROLL_SETTLE: Duration = Duration::from_millis(300);

/// [`SCROLL_SETTLE`], or `ROOST_TEST_SCROLL_SETTLE_MS` in test mode.
/// Read once, at startup.
pub(super) fn scroll_settle() -> Duration {
    use crate::host_conn::reconnect::{parse_millis, test_override};
    parse_millis(
        test_override("ROOST_TEST_SCROLL_SETTLE_MS").as_deref(),
        SCROLL_SETTLE,
    )
}

/// Attempt numbers, unique across every attach this process makes: a frame
/// a draining connection of one attach still delivers must never match
/// the attach that replaced it on the same tab.
static NEXT_ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_attempt() -> u64 {
    NEXT_ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Why a host tab is being attached (plan 084 D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachIntent {
    /// The user switched to it: the attach takes the tab's size.
    Explicit,
    /// The window attached on its own — a restore, a fallback, a
    /// re-attach: it takes only a tab nobody sizes.
    Automatic,
}

/// A host tab's size-ownership actions, in order, for `host.size_trace`.
/// Kept by the app across attaches; only in test mode.
pub(crate) type SizeTrace = Arc<std::sync::Mutex<Vec<SizeTraceAction>>>;

/// Re-attach backoff: base doubling, capped. Deterministic (no jitter):
/// one client re-attaching one focused tab is not a thundering herd.
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// The first rung of the ladder whose delay is [`BACKOFF_CAP`].
const BACKOFF_CAP_STEP: u32 = {
    let mut step = 0;
    while BACKOFF_BASE.as_nanos() << step < BACKOFF_CAP.as_nanos() {
        step += 1;
    }
    step
};

/// How many times an attach refused by a full op queue is retried before
/// it detaches: as many times as the ladder has rungs, the capped one
/// included.
///
/// Bounded where every other retryable failure is not, because what
/// failed is not this attach's own: a queue still full once the ladder
/// has been climbed is the host connection's to recover, not this tab's
/// to keep asking at the cap.
const FULL_QUEUE_RETRIES: u32 = BACKOFF_CAP_STEP + 1;

/// The grid + pixel geometry an attach negotiates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Geometry {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) cell_w: u32,
    pub(crate) cell_h: u32,
}

/// Which of [`CLIENT_PAYLOAD_KINDS`] an attach was accepted as, and so
/// which hydration its payload gets.
///
/// The wire's `AttachPayloadKind` is an open string on purpose (a client
/// must be able to read a newer session's list); this is the closed set
/// of the two this client can actually decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PayloadKind {
    /// libghostty's own snapshot, decoded by [`SnapshotDecoder`].
    GhosttySnapshot,
    /// A VT byte stream replayed into a terminal of this client's own.
    Vt,
}

impl PayloadKind {
    fn from_wire(kind: &AttachPayloadKind) -> Option<Self> {
        match kind.as_str() {
            AttachPayloadKind::GHOSTTY_SNAPSHOT => Some(Self::GhosttySnapshot),
            AttachPayloadKind::VT => Some(Self::Vt),
            _ => None,
        }
    }

    /// The wire spelling, as `host.status` reports it.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::GhosttySnapshot => AttachPayloadKind::GHOSTTY_SNAPSHOT,
            Self::Vt => AttachPayloadKind::VT,
        }
    }
}

/// Where a detached tab can pick its stream back up: the stream identity
/// the last accepted handshake reported plus the next seq this client
/// has not applied. Kept per tab across detach — refocus hands it back
/// and the wire answers `mode: "resume"` when the ring still covers it,
/// or falls back to a fresh snapshot in the same reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResumePoint {
    pub(crate) server_epoch: u64,
    pub(crate) tab_generation: u64,
    pub(crate) next_seq: u64,
}

/// Whether a resume point still holds for a terminal re-gridded from
/// `before` to `after`.
///
/// A resume replays the records the session wrote after the point, and
/// it wrote them for the grid the session last heard from this client —
/// the one the terminal was at. Replayed into a terminal at another grid,
/// every absolute cursor move and every wrapped line lands somewhere the
/// session's terminal did not put it, and nothing heals the primary
/// screen or the scrollback afterwards (#564). Cell pixels move no cell,
/// so only the grid counts.
pub(super) fn resume_survives_regrid(before: (u16, u16), after: (u16, u16)) -> bool {
    before == after
}

/// Drop `key`'s resume point when its terminal was re-gridded from
/// `before` to `after`, so the next attach is a snapshot. A detached tab
/// is the only one with a point in `resumes`: an attach holds its own.
pub(super) fn forget_resume_on_regrid(
    resumes: &mut HashMap<TabKey, ResumePoint>,
    key: TabKey,
    before: (u16, u16),
    after: (u16, u16),
) {
    if !resume_survives_regrid(before, after) {
        resumes.remove(&key);
    }
}

/// One item of an attached host tab's traffic, riding the engine feed.
pub(crate) enum HostTabFrame {
    /// The handshake was accepted: the negotiated kind, the stream
    /// identity and the fence.
    Accepted {
        attempt: u64,
        resumed: bool,
        /// What the payload that follows is, taken from the data
        /// connection's own reply (see [`negotiated_kind`]).
        kind: PayloadKind,
        fence: u64,
        server_epoch: u64,
        tab_generation: u64,
        /// The tab's grid as the payload was composed — the PTY's, which
        /// is this attach's own only when it took the tab.
        grid: (u16, u16),
        /// Who sizes the tab, as of the same moment.
        owner: Option<SizeOwner>,
    },
    /// The attach op or the dial failed before any frame flowed.
    Failed {
        attempt: u64,
        reason: FailReason,
    },
    /// The queue permit was refused, before the grant or by the fence
    /// after it. Carried typed rather than classified on the task: whether
    /// a full queue is still worth a retry depends on how many times it
    /// has refused this attach, which only the main thread counts.
    PermitRefused {
        attempt: u64,
        error: HostOpError,
    },
    Snap {
        attempt: u64,
        bytes: Vec<u8>,
    },
    Pty {
        attempt: u64,
        seq: u64,
        bytes: Vec<u8>,
    },
    Exit {
        attempt: u64,
        final_seq: u64,
        code: i32,
    },
    /// A `GRID` frame: the tab's grid or size owner changed, after PTY
    /// frame `seq` and before the next one.
    Grid {
        attempt: u64,
        seq: u64,
        grid: (u16, u16),
        owner: Option<SizeOwner>,
    },
    /// An `ERROR` frame: the connection closes after it.
    Error {
        attempt: u64,
        code: String,
        message: String,
    },
    /// EOF or a transport error on the data connection.
    Closed {
        attempt: u64,
    },
    /// The re-attach backoff timer fired.
    ReattachDue {
        attempt: u64,
    },
    /// The 2 s resize-withhold deadline fired.
    WithholdDeadline {
        attempt: u64,
    },
    /// Self-wake: history pages remain and the per-pass budget was hit.
    StepDecoder {
        attempt: u64,
    },
    /// A local scroll held still for [`scroll_settle`]: `generation`
    /// names the scroll that armed it.
    ScrollSettled {
        attempt: u64,
        generation: u64,
    },
}

/// Why an attach attempt died before its stream started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FailReason {
    /// `build-mismatch` — retrying cannot help; the host needs a restart
    /// (the C8 dialog drives that; the tab just stops).
    BuildMismatch(String),
    /// The session is going away — the host connection owns the
    /// recovery; the tab detaches passively.
    HostGone(String),
    /// Anything transient: transport errors, `snapshot-failed`,
    /// `not-found` after a respawn race. Re-attach with backoff.
    Retryable(String),
}

/// What the drain should do after a frame was applied.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum AttachStep {
    None,
    /// Terminal state moved — refresh the tab's published snapshot.
    Refresh,
    /// Drop this attempt's tasks and schedule a re-attach after `delay`.
    Reattach {
        delay: Duration,
    },
    /// Detach passively and stay detached: another window took the tab
    /// (`superseded`), the session is going, or the build mismatches. The
    /// host-level state (banner, NeedsRestart) is the connection's to
    /// publish, not this tab's.
    Detach,
    /// EXIT validated — the tab is over; the mirror's `tab.closed`
    /// drives the row out of the sidebar.
    Closed {
        code: i32,
    },
}

/// How a payload becomes the terminal that will be swapped in — one
/// arm per [`PayloadKind`].
enum Hydrator {
    /// GHOSTSNP: libghostty's decoder, fed frame by frame, its history
    /// stepped after READY and finished by the payload's own FINISH
    /// record.
    Snapshot(SnapshotDecoder),
    /// `vt`: a terminal of this client's own, at the attach geometry and
    /// wearing this client's theme, that the payload's bytes are written
    /// into as they arrive. The stream carries no marks of its own — the
    /// server ends it with one zero-length SNAP frame, and that
    /// terminator is the only signal the payload is whole.
    Vt(Terminal),
}

impl Hydrator {
    /// Drop a hydration in flight, **on this thread**. Which is the
    /// whole reason it lives here and not on the task that reads frames:
    /// an abort must never be what drops a decoder mid-`feed`.
    fn abandon(self) {
        match self {
            Self::Snapshot(decoder) => drop(decoder.abandon()),
            Self::Vt(terminal) => drop(terminal),
        }
    }
}

/// Hydration-phase state: the hydrator plus the deferral and rendering
/// it needs. Lives on the main thread only.
struct Hydration {
    hydrator: Hydrator,
    /// The stream identity + fence progress of THIS hydration. Promoted
    /// to [`HostAttach::resume`] only at the swap: until then, the
    /// rendered terminal is still the old one, and advertising the new
    /// fence early would let an aborted hydration "resume" onto a
    /// terminal that never took the snapshot — permanent divergence.
    identity: ResumePoint,
    /// PTY and GRID frames that arrived before the payload could take
    /// them, replayed in order once it can. The server holds both on its
    /// side too, so this is belt-and-braces for the tiny window the two
    /// rules can miss.
    deferred: VecDeque<Deferred>,
    /// Bytes queued in `deferred` — bounded, because a server violating
    /// its own hold rule must not grow client memory without limit.
    deferred_bytes: usize,
    /// The snapshot decoder passed READY: there is a screen to render
    /// from, history left to step, and a resize could be mirrored.
    ///
    /// Never true of a [`Hydrator::Vt`], which has no such prefix — a
    /// half-replayed VT stream is not a drawable screen, so a `vt`
    /// hydration defers PTY and withholds a resize for the whole payload
    /// and is finished by the terminator instead.
    ready: bool,
    /// Bounded stepping left pages behind; a `StepDecoder` self-wake is
    /// in flight.
    stepping: bool,
    /// The cols/rows the terminal being built here is at: the
    /// snapshot's own (`AttachAccepted.snapshot_cols/rows`), then each
    /// `GRID` the hydration applied. What [`HostAttach::finish_hydration`]
    /// installs the terminal at.
    built_at: (u16, u16),
}

/// One frame [`Hydration::deferred`] holds.
enum Deferred {
    Pty(Vec<u8>),
    Grid((u16, u16)),
}

impl Hydration {
    /// The decoder past READY — the only state with history to step or a
    /// screen a mid-stream resize could be mirrored onto.
    ///
    /// A `vt` hydration answers `None` at every moment of its life: the
    /// server composed those bytes for the attach geometry and they
    /// cannot be re-cut, which is why a resize that will not wait
    /// re-attaches there rather than being mirrored.
    fn live_decoder(&mut self) -> Option<&mut SnapshotDecoder> {
        if !self.ready {
            return None;
        }
        match &mut self.hydrator {
            Hydrator::Snapshot(decoder) => Some(decoder),
            Hydrator::Vt(_) => None,
        }
    }
}

/// The deferral bound: the server's own queued-PTY budget. More than
/// this before READY means the peer is not honoring hold-until-READY,
/// and the stream is rebuilt rather than buffered without limit.
const MAX_DEFERRED_BYTES: usize = 8 * 1024 * 1024;

enum Phase {
    /// Permit + dial in flight on the attempt's task.
    Requesting,
    /// Boxed for the variant-size lint: a `Phase` lives in every attach
    /// entry, and only hydration carries the decoder's bulk.
    Hydrating(Box<Hydration>),
    Live,
    /// Detached-for-good from this tab's perspective (superseded, build
    /// mismatch, exit). The attach state is dropped right after.
    Ended,
}

/// The focused host tab's attach state — at most one per host under the
/// attach-on-focus policy, held in the app's map on the main thread.
pub(super) struct HostAttach {
    key: TabKey,
    attempt: u64,
    phase: Phase,
    /// The stream identity + fence progress. `None` until the first
    /// accepted handshake.
    resume: Option<ResumePoint>,
    /// What the last accepted attach on this tab negotiated. `None`
    /// until one has been accepted; read by `host.status`.
    kind: Option<PayloadKind>,
    /// Whether the next handshake takes the tab's size: an explicit
    /// switch's, until the first handshake the session accepts.
    take_pending: bool,
    /// Whether the session has accepted any handshake of this attach.
    accepted: bool,
    /// The latest user resize withheld while the stream is not live
    /// (latest-wins), with the window focus it happened under.
    withheld: Option<(Geometry, bool)>,
    /// This window's grid and cells — what the handshake and every
    /// RESIZE carry. Never the mirror's: the tab's session sizes that.
    advisory: Geometry,
    /// The grid the session last said the rendered terminal is at: an
    /// accepted resume's, a finished hydration's, or a live `GRID`'s.
    /// `None` until this attach has heard one.
    pty_grid: Option<(u16, u16)>,
    /// Who sizes the tab, from the accepted handshake and every `GRID`
    /// since: the only source the marker reads.
    owner: Option<SizeOwner>,
    /// The client this window attaches as — what tells "me" from
    /// another client in [`Self::owner`].
    client_id: String,
    /// The persistent input queue: keystrokes survive re-attach windows
    /// here. The sender side also lives in the tab's `TabHandle`.
    input_tx: mpsc::UnboundedSender<HostDataMsg>,
    input_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<HostDataMsg>>>,
    /// The current attempt's tasks (dial/reader/writer/timers), aborted
    /// wholesale on detach or re-attach.
    tasks: Vec<tokio::task::AbortHandle>,
    /// The current attempt's data-connection task, kept apart from
    /// `tasks`: the one a detach lets finish writing `DETACH`.
    data_task: Option<tokio::task::AbortHandle>,
    /// Set once this attach is detaching: a dial still waiting for its
    /// reply writes `DETACH` behind its handshake, and the data task
    /// stops feeding the frames it still reads, which nothing will apply.
    detaching: watch::Sender<bool>,
    /// The settle timer of the last local scroll, and which scroll it
    /// was armed for.
    settle: Option<tokio::task::AbortHandle>,
    settle_generation: u64,
    trace: Option<SizeTrace>,
    backoff_step: u32,
    /// Permits refused by a full queue since the stream was last Live —
    /// see [`FULL_QUEUE_RETRIES`].
    full_refusals: u32,
}

impl HostAttach {
    pub(super) fn new(key: TabKey, advisory: Geometry, intent: AttachIntent) -> Self {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        Self {
            key,
            attempt: 0,
            phase: Phase::Requesting,
            resume: None,
            kind: None,
            take_pending: intent == AttachIntent::Explicit,
            accepted: false,
            withheld: None,
            advisory,
            pty_grid: None,
            owner: None,
            client_id: String::new(),
            input_tx,
            input_rx: Arc::new(tokio::sync::Mutex::new(input_rx)),
            tasks: Vec::new(),
            data_task: None,
            detaching: watch::Sender::new(false),
            settle: None,
            settle_generation: 0,
            trace: None,
            backoff_step: 0,
            full_refusals: 0,
        }
    }

    /// Restore the resume point a previous attach of this tab left
    /// behind (refocus). Must be set before [`Self::begin`].
    pub(super) fn with_resume(mut self, resume: Option<ResumePoint>) -> Self {
        self.resume = resume;
        self
    }

    /// Record this attach's size-ownership actions into `trace`.
    pub(super) fn with_trace(mut self, trace: Option<SizeTrace>) -> Self {
        self.trace = trace;
        self
    }

    /// The sender the tab's `TabHandle` queues input on.
    pub(super) fn input_tx(&self) -> mpsc::UnboundedSender<HostDataMsg> {
        self.input_tx.clone()
    }

    /// What the last accepted attach negotiated — what the host's
    /// `payload_kind` reports.
    pub(super) fn payload_kind(&self) -> Option<PayloadKind> {
        self.kind
    }

    /// Who sizes the tab, as this attach last heard it.
    pub(super) fn owner(&self) -> Option<&SizeOwner> {
        self.owner.as_ref()
    }

    pub(super) fn is_me(&self, owner: &SizeOwner) -> bool {
        owner.client_id == self.client_id
    }

    /// The client sizing the tab, when it is not this window's.
    pub(super) fn other_owner(&self) -> Option<&SizeOwner> {
        self.owner.as_ref().filter(|owner| !self.is_me(owner))
    }

    pub(super) fn owned_by_another(&self) -> bool {
        self.other_owner().is_some()
    }

    fn owned_by_me(&self) -> bool {
        self.owner.as_ref().is_some_and(|owner| self.is_me(owner))
    }

    fn trace(&self, action: SizeTraceAction) {
        record(self.trace.as_ref(), action);
    }

    /// Whether input queued here still has a reader.
    ///
    /// True for every phase but [`Phase::Ended`]. `Requesting` and
    /// `Hydrating` queue rather than drop — the input queue outlives an
    /// attempt, which is the whole reason it lives on the attach and not
    /// on the attempt's task.
    pub(super) fn live(&self) -> bool {
        !matches!(self.phase, Phase::Ended)
    }

    /// Whether the keys typed while this tab opened can go (plan 072
    /// §D2). They are encoded against the terminal's modes (DECCKM,
    /// kitty), and an accepted snapshot attach is still rendering the
    /// stand-in, so only the stream live on the terminal the session
    /// hydrated will do. An attempt that ends first leaves them in the
    /// pending buffer — not in the input queue — for its retry.
    pub(super) fn takes_typed_ahead(&self) -> bool {
        self.streaming()
    }

    /// Whether the stream is actually up — the handshake accepted and
    /// the snapshot hydrated.
    ///
    /// Narrower than [`Self::live`] on purpose, and the two are not
    /// interchangeable: `live` means "keep queuing input for this tab",
    /// which is true of an attempt that has not landed yet. This means
    /// "there is something on screen", which is what plan 063 §D8 phase
    /// 6's fence has to wait for — a selection starts an attach, and an
    /// attach can still be refused.
    pub(super) fn streaming(&self) -> bool {
        matches!(self.phase, Phase::Live)
    }

    /// Start (or restart) an attach attempt. Must be called inside the
    /// app runtime (`Runtime::enter`) — every task binds to the ambient
    /// runtime. `ops` is the host's op queue, which the attempt takes a
    /// permit from so the dial cannot overtake `session.set_theme`;
    /// `session_id` is the session the handshake binds itself to and
    /// `socket` the host's endpoint.
    ///
    /// `session_id` is `None` only in the window between a connection
    /// publishing `Connected` and publishing its facts, which is a
    /// "not yet", not a refusal — the attempt fails retryably and the
    /// ordinary backoff picks it up.
    #[allow(clippy::too_many_arguments)] // One attempt's whole context, as the app holds it.
    pub(super) fn begin(
        &mut self,
        ops: &HostOps,
        session_id: Option<&str>,
        socket: std::path::PathBuf,
        libghostty_build: &str,
        client_id: &str,
        feed: &EngineFeedSender,
    ) {
        self.abort_tasks();
        self.attempt = next_attempt();
        self.phase = Phase::Requesting;
        self.client_id = client_id.to_string();
        let attempt = self.attempt;
        let key = self.key;
        let advisory = self.advisory;
        let resume = self.resume;
        let Some(session_id) = session_id else {
            feed.send(EngineFeed::HostTab(
                key,
                HostTabFrame::Failed {
                    attempt,
                    reason: FailReason::Retryable(
                        "the host connection has not reported its session id yet".into(),
                    ),
                },
            ));
            return;
        };
        let terms = AttachHandshakeTerms {
            session_id: session_id.to_string(),
            kinds: CLIENT_PAYLOAD_KINDS.map(AttachPayloadKind::from).to_vec(),
            cols: advisory.cols,
            rows: advisory.rows,
            cell_w_px: cell_px(advisory.cell_w),
            cell_h_px: cell_px(advisory.cell_h),
            libghostty_build: libghostty_build.to_string(),
            take: self.take_pending,
            client_id: client_id.to_string(),
        };
        self.trace(SizeTraceAction::AttachSent { take: terms.take });
        let dial = Dial {
            socket,
            handshake: choose_handshake(key.tab, terms, resume),
        };
        // Fenced at this tab's own incarnation, in the queue and again
        // for the dial it releases: an attach must not reach whatever
        // replaced the connection it was asked of, nor the connection
        // itself once this client has lost it.
        let permit = ops.attach_permit(key.host);
        let input_rx = Arc::clone(&self.input_rx);
        let task = tokio::spawn(run_attempt(
            key,
            attempt,
            permit,
            dial,
            input_rx,
            self.detaching.subscribe(),
            feed.clone(),
        ));
        self.data_task = Some(task.abort_handle());
    }

    /// An explicit switch to a tab this attach is already for. An attach
    /// the window made on its own that no handshake of has been accepted
    /// yet becomes the switch's — it will take the size — and reports so,
    /// for the caller to dial again with that intent; one the session has
    /// accepted is an established attachment, which re-selecting leaves
    /// alone (Take size is for that).
    pub(super) fn make_explicit(&mut self) -> bool {
        if self.accepted || self.take_pending {
            return false;
        }
        self.take_pending = true;
        true
    }

    /// Arm the re-attach backoff timer: one `ReattachDue` for the
    /// current attempt after `delay`. Must be called inside the app
    /// runtime.
    pub(super) fn arm_reattach(&mut self, delay: Duration, feed: &EngineFeedSender) {
        let attempt = self.attempt;
        self.arm_timer(delay, feed, HostTabFrame::ReattachDue { attempt });
    }

    /// Put `frame` on the feed after `delay`, tracked with the attempt's
    /// other tasks so a re-attach or a detach cancels it. Must be called
    /// inside the app runtime.
    fn arm_timer(&mut self, delay: Duration, feed: &EngineFeedSender, frame: HostTabFrame) {
        // A withhold deadline re-arms itself for as long as a hydration
        // runs, so the fired ones are dropped here rather than kept to
        // the end of the attempt: there is nothing left to abort in a
        // timer that already sent.
        self.tasks.retain(|task| !task.is_finished());
        let timer = spawn_after(delay, self.key, feed, frame);
        self.tasks.push(timer);
    }

    /// Adopt `geometry` and queue its RESIZE behind whatever input is
    /// already waiting, so the wire sees the two in submission order.
    fn queue_geometry(&mut self, geometry: Geometry, focused: bool) {
        self.advisory = geometry;
        self.trace(SizeTraceAction::ResizeSent { focused });
        let _ = self.input_tx.send(HostDataMsg::Resize {
            cols: geometry.cols,
            rows: geometry.rows,
            cell_w: geometry.cell_w,
            cell_h: geometry.cell_h,
            focused,
        });
    }

    /// A window resize while attached, and whether the window had focus
    /// when it happened. Live: queued onto the wire in order with input.
    /// Requesting or hydrating: withheld (latest-wins) — sent once the
    /// stream is live or at the 2 s deadline, because a resize that
    /// moves the tab mid-snapshot forfeits the remaining history pages.
    ///
    /// A focused resize of a tab this window does not size is a deliberate
    /// take even at the grid already advertised — a font change that
    /// lands on the same cells, a drag that comes back to where it began —
    /// so only an unfocused or already-owned one at that grid is dropped.
    pub(super) fn note_resize(&mut self, geometry: Geometry, focused: bool) {
        let news = geometry != self.advisory || (focused && !self.owned_by_me());
        match &self.phase {
            Phase::Live => {
                if news {
                    self.queue_geometry(geometry, focused);
                }
            }
            Phase::Requesting | Phase::Hydrating(_) => {
                self.withheld = news.then_some((geometry, focused));
            }
            Phase::Ended => {}
        }
    }

    /// Ask the session for the tab's size at this window's grid.
    pub(super) fn take(&mut self) {
        self.trace(SizeTraceAction::TakeSent);
        let _ = self.input_tx.send(HostDataMsg::Take);
    }

    /// A copy captured its text off this tab's mirror. Another client's
    /// tab is taken after the capture, so the copy holds the text as it
    /// was laid out when the user chose it (plan 084 D6).
    pub(super) fn copied(&mut self) {
        self.trace(SizeTraceAction::CopyCaptured);
        if !self.owned_by_me() {
            self.take();
        }
    }

    /// The viewport moved locally. On another client's tab, a scroll
    /// that then holds still for `settle` takes the size; each further
    /// scroll starts the wait over.
    pub(super) fn scrolled(&mut self, settle: Duration, feed: &EngineFeedSender) {
        if self.owned_by_me() {
            return;
        }
        if let Some(previous) = self.settle.take() {
            previous.abort();
        }
        self.settle_generation += 1;
        let frame = HostTabFrame::ScrollSettled {
            attempt: self.attempt,
            generation: self.settle_generation,
        };
        self.settle = Some(spawn_after(settle, self.key, feed, frame));
    }

    /// Drive one feed frame through the machine. `tab` is this tab's
    /// rendering state; the decoder and hydrating terminal live in
    /// `self`. Frames from a previous attempt are dropped here — their
    /// tasks were aborted, but frames already queued outlive the abort.
    pub(super) fn on_frame(
        &mut self,
        frame: HostTabFrame,
        tab: &mut super::terminal_tab::TerminalTab,
        feed: &EngineFeedSender,
    ) -> AttachStep {
        if frame_attempt(&frame) != self.attempt {
            tracing::debug!(key = %self.key, "dropping a frame from a dead attach attempt");
            return AttachStep::None;
        }
        match frame {
            HostTabFrame::Accepted {
                resumed,
                kind,
                fence,
                server_epoch,
                tab_generation,
                grid,
                owner,
                ..
            } => {
                // The session committed the explicit switch this attach was
                // made for: every attach after this one is the window's own,
                // however this one ends.
                self.take_pending = false;
                self.accepted = true;
                self.trace(SizeTraceAction::AttachAccepted { resumed });
                if resumed && !resume_survives_regrid(tab.grid(), grid) {
                    return self.discard_resume();
                }
                self.kind = Some(kind);
                self.owner = owner;
                let identity = ResumePoint {
                    server_epoch,
                    tab_generation,
                    next_seq: fence + 1,
                };
                if resumed {
                    // No SNAP at all: the ring replays as ordinary PTY
                    // frames ahead of the live ones. The old terminal is
                    // the right base — that is what resume means, and
                    // why the fence installs immediately here but only
                    // at FINISH in snapshot mode.
                    self.resume = Some(identity);
                    self.pty_grid = Some(grid);
                    // Reaching Live is what resets the backoff ladder —
                    // a mere accept from a server that then dies would
                    // otherwise hot-loop at the base delay.
                    self.reset_ladder();
                    self.phase = Phase::Live;
                    self.flush_withheld();
                } else {
                    // Built at the payload's own grid, which is the
                    // tab's: replaying a payload composed at another
                    // width into a terminal of this width wraps its
                    // lines and misplaces its absolute cursor moves.
                    let built_at = grid;
                    let hydrator = match kind {
                        PayloadKind::GhosttySnapshot => Hydrator::Snapshot(SnapshotDecoder::new(
                            SnapshotDecodeOptions::default(),
                        )),
                        // Built here rather than by the decoder: a `vt`
                        // payload carries only what the *program*
                        // changed, so the terminal it lands in is this
                        // client's — its scrollback, its theme.
                        PayloadKind::Vt => match tab.hydration_terminal(built_at.0, built_at.1) {
                            Ok(terminal) => Hydrator::Vt(terminal),
                            Err(error) => {
                                tracing::warn!(key = %self.key, %error, "vt hydration terminal build failed; re-attaching");
                                return self.schedule_reattach();
                            }
                        },
                    };
                    // A fresh payload supersedes anything the old stream
                    // had applied; the fence restarts the count — inside
                    // the hydration, not in `self.resume`, which keeps
                    // describing the terminal actually rendered.
                    self.phase = Phase::Hydrating(Box::new(Hydration {
                        hydrator,
                        identity,
                        deferred: VecDeque::new(),
                        deferred_bytes: 0,
                        ready: false,
                        stepping: false,
                        built_at,
                    }));
                    // The withhold deadline covers only a hydration; a
                    // resume has no completion to wait for.
                    let attempt = self.attempt;
                    self.arm_timer(
                        WITHHOLD_DEADLINE,
                        feed,
                        HostTabFrame::WithholdDeadline { attempt },
                    );
                }
                AttachStep::Refresh
            }
            HostTabFrame::Failed { reason, .. } => self.apply_failure(reason),
            HostTabFrame::PermitRefused { error, .. } => {
                let reason = self.count_permit_refusal(&error);
                self.apply_failure(reason)
            }
            HostTabFrame::Snap { bytes, .. } => self.apply_snap(&bytes, tab, feed),
            HostTabFrame::Pty { seq, bytes, .. } => self.apply_pty(seq, bytes, tab),
            HostTabFrame::Grid {
                seq, grid, owner, ..
            } => self.apply_grid(seq, grid, owner, tab),
            HostTabFrame::Exit {
                final_seq, code, ..
            } => {
                let expected = self.expected_next_seq().unwrap_or(0);
                if final_seq != expected {
                    // The exit consumed an ordinal we never saw bytes
                    // for — something was lost. There is nothing to
                    // re-attach to (the tab is over); render the exit.
                    tracing::debug!(
                        key = %self.key, final_seq, expected,
                        "EXIT with unapplied bytes outstanding"
                    );
                }
                self.phase = Phase::Ended;
                AttachStep::Closed { code }
            }
            HostTabFrame::Error { code, message, .. } => {
                let mapped = ServerCode::from_wire(&code);
                match mapped {
                    ServerCode::ShuttingDown => {
                        // Host-level: the events connection sees the
                        // same fate and the connection state machine
                        // owns the banner. The tab detaches passively.
                        self.phase = Phase::Ended;
                        AttachStep::Detach
                    }
                    _ => {
                        // desync / overflow / protocol-error: the stream
                        // cannot be trusted; re-attach rebuilds it.
                        tracing::debug!(key = %self.key, %code, %message, "data stream error; re-attaching");
                        self.schedule_reattach()
                    }
                }
            }
            HostTabFrame::Closed { .. } => match self.phase {
                Phase::Ended => AttachStep::None,
                _ => self.schedule_reattach(),
            },
            HostTabFrame::ReattachDue { .. } => AttachStep::Reattach {
                delay: Duration::ZERO,
            },
            HostTabFrame::WithholdDeadline { .. } => {
                if !matches!(self.phase, Phase::Hydrating(_)) {
                    return AttachStep::None;
                }
                // Bound the hold, not the hydration: the resize goes to
                // the session now, and any `GRID` it brings back applies
                // to the payload at its place in the stream — forfeiting
                // the history pages still to come, which is the price of
                // not holding the user's geometry hostage. The next
                // deadline covers whatever is withheld from here on.
                self.flush_withheld();
                let attempt = self.attempt;
                self.arm_timer(
                    WITHHOLD_DEADLINE,
                    feed,
                    HostTabFrame::WithholdDeadline { attempt },
                );
                AttachStep::None
            }
            HostTabFrame::StepDecoder { .. } => {
                if let Phase::Hydrating(hydration) = &mut self.phase {
                    hydration.stepping = false;
                }
                self.drive_decoder(tab, feed)
            }
            HostTabFrame::ScrollSettled { generation, .. } => {
                if generation == self.settle_generation && self.owned_by_another() {
                    self.take();
                }
                AttachStep::None
            }
        }
    }

    /// Stop a pending scroll settle, including one whose frame is already
    /// on the feed: an attempt that ended takes nothing on the next one's
    /// behalf.
    fn cancel_settle(&mut self) {
        if let Some(settle) = self.settle.take() {
            settle.abort();
        }
        self.settle_generation += 1;
    }

    /// Send the withheld resize, if any.
    fn flush_withheld(&mut self) {
        if let Some((geometry, focused)) = self.withheld.take() {
            self.queue_geometry(geometry, focused);
        }
    }

    /// Detach: abort this attempt's tasks and abandon a mid-flight
    /// decoder on this thread — never dropping it mid-`feed` from an
    /// aborted task, which is why the decoder lives here and not there.
    ///
    /// The resume point survives in the return value only while `kept`,
    /// the grid of the terminal the tab keeps, is the one the session
    /// last said it is at: the records past the point were written for
    /// that grid.
    pub(super) fn detach(mut self, kept: Option<(u16, u16)>) -> Option<ResumePoint> {
        if matches!(self.phase, Phase::Ended) {
            self.abort_tasks();
        } else {
            self.detach_on_the_wire();
        }
        if let Phase::Hydrating(hydration) = std::mem::replace(&mut self.phase, Phase::Ended) {
            hydration.hydrator.abandon();
        }
        let given = self.pty_grid;
        self.resume.filter(|_| {
            kept.is_some_and(|kept| given.is_none_or(|given| resume_survives_regrid(given, kept)))
        })
    }

    /// Refuse an accepted resume written for a grid the terminal is not
    /// at: the attempt ends, and a fresh one goes with no resume point —
    /// a snapshot.
    fn discard_resume(&mut self) -> AttachStep {
        tracing::debug!(key = %self.key, "a resume for a grid the terminal is not at; attaching fresh");
        self.abort_tasks();
        self.resume = None;
        AttachStep::Reattach {
            delay: Duration::ZERO,
        }
    }

    fn abort_tasks(&mut self) {
        if let Some(task) = self.data_task.take() {
            task.abort();
        }
        self.cancel_settle();
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }

    /// Tell the session this is a deliberate detach, so it hands the
    /// tab's size on at once instead of holding it for this client's
    /// grace: `DETACH` is queued behind whatever input is waiting (or,
    /// for a dial still waiting on its reply, written right behind its
    /// handshake), the data task gets [`DETACH_DRAIN`] to write it, and
    /// everything else goes now. Outside a runtime there is nothing to
    /// wait on, and the detach is the disconnect it always was.
    fn detach_on_the_wire(&mut self) {
        self.trace(SizeTraceAction::DetachSent);
        self.detaching.send_replace(true);
        let _ = self.input_tx.send(HostDataMsg::Detach);
        if let Some(settle) = self.settle.take() {
            settle.abort();
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
        let Some(data_task) = self.data_task.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    tokio::time::sleep(DETACH_DRAIN).await;
                    data_task.abort();
                });
            }
            Err(_) => data_task.abort(),
        }
    }

    /// The seq the next PTY frame must carry — tracked by the hydration
    /// while one is in flight (its fence describes the terminal being
    /// built, not the one rendered), by the resume point once live.
    fn expected_next_seq(&self) -> Option<u64> {
        match &self.phase {
            Phase::Hydrating(hydration) => Some(hydration.identity.next_seq),
            Phase::Live => self.resume.map(|resume| resume.next_seq),
            Phase::Requesting | Phase::Ended => None,
        }
    }

    /// Test-only: what the machine queued toward the wire, drained.
    #[cfg(test)]
    fn test_drain_input(&self) -> Vec<HostDataMsg> {
        let mut rx = self.input_rx.try_lock().expect("no writer task in tests");
        let mut drained = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            drained.push(msg);
        }
        drained
    }

    fn apply_failure(&mut self, reason: FailReason) -> AttachStep {
        match reason {
            FailReason::BuildMismatch(message) => {
                tracing::warn!(key = %self.key, %message, "attach refused: build mismatch");
                self.phase = Phase::Ended;
                AttachStep::Detach
            }
            FailReason::HostGone(message) => {
                tracing::debug!(key = %self.key, %message, "attach refused: host connection gone");
                self.phase = Phase::Ended;
                AttachStep::Detach
            }
            FailReason::Retryable(message) => {
                tracing::debug!(key = %self.key, %message, "attach attempt failed; backing off");
                self.schedule_reattach()
            }
        }
    }

    /// [`classify_permit_failure`], with a full queue retried at most
    /// [`FULL_QUEUE_RETRIES`] times.
    fn count_permit_refusal(&mut self, error: &HostOpError) -> FailReason {
        if *error == HostOpError::QueueFull {
            self.full_refusals += 1;
            if self.full_refusals > FULL_QUEUE_RETRIES {
                return FailReason::HostGone(format!(
                    "{error}, still after {FULL_QUEUE_RETRIES} retries"
                ));
            }
        }
        classify_permit_failure(error)
    }

    fn reset_ladder(&mut self) {
        self.backoff_step = 0;
        self.full_refusals = 0;
    }

    fn schedule_reattach(&mut self) -> AttachStep {
        self.abort_tasks();
        if let Phase::Hydrating(hydration) = std::mem::replace(&mut self.phase, Phase::Requesting) {
            hydration.hydrator.abandon();
        }
        let delay = BACKOFF_BASE
            .saturating_mul(1u32 << self.backoff_step.min(BACKOFF_CAP_STEP))
            .min(BACKOFF_CAP);
        self.backoff_step = self.backoff_step.saturating_add(1);
        AttachStep::Reattach { delay }
    }

    fn apply_snap(
        &mut self,
        bytes: &[u8],
        tab: &mut super::terminal_tab::TerminalTab,
        feed: &EngineFeedSender,
    ) -> AttachStep {
        let cells = self.advisory;
        let Phase::Hydrating(hydration) = &mut self.phase else {
            // SNAP outside hydration — including a non-empty one after a
            // `vt` terminator: the stream is confused. Rebuild.
            tracing::debug!(key = %self.key, "SNAP outside hydration; re-attaching");
            return self.schedule_reattach();
        };
        match &mut hydration.hydrator {
            Hydrator::Vt(terminal) => {
                if !bytes.is_empty() {
                    terminal.vt_write(bytes);
                    return AttachStep::None;
                }
                // The terminator: the payload is whole, so what the
                // server held behind it goes in now, in arrival order —
                // the same replay READY does on the other arm.
                while let Some(deferred) = hydration.deferred.pop_front() {
                    match deferred {
                        Deferred::Pty(bytes) => terminal.vt_write(&bytes),
                        Deferred::Grid(grid) => {
                            if let Err(error) = terminal.resize(
                                grid.0,
                                grid.1,
                                cells.cell_w.max(1),
                                cells.cell_h.max(1),
                            ) {
                                tracing::debug!(key = %self.key, %error, "deferred GRID failed; re-attaching");
                                return self.schedule_reattach();
                            }
                            hydration.built_at = grid;
                            trace_grid(self.trace.as_ref(), grid);
                        }
                    }
                }
                self.finish_hydration(tab)
            }
            Hydrator::Snapshot(decoder) => {
                if let Err(error) = decoder.feed(bytes) {
                    tracing::debug!(key = %self.key, %error, "snapshot decode failed; re-attaching");
                    return self.schedule_reattach();
                }
                if !hydration.ready {
                    match decoder.try_ready() {
                        Ok(ReadyState::NeedMoreBytes) => return AttachStep::None,
                        Ok(ReadyState::Ready) => {
                            hydration.ready = true;
                            // Replay the deferral in arrival order —
                            // these are live frames from after the
                            // snapshot's fence, and the decoder
                            // interleaves them correctly from READY.
                            while let Some(deferred) = hydration.deferred.pop_front() {
                                let applied = match deferred {
                                    Deferred::Pty(bytes) => decoder.vt_write(&bytes),
                                    Deferred::Grid(grid) => {
                                        hydration.built_at = grid;
                                        trace_grid(self.trace.as_ref(), grid);
                                        decoder.resize(grid.0, grid.1, cells.cell_w, cells.cell_h)
                                    }
                                };
                                if let Err(error) = applied {
                                    tracing::debug!(key = %self.key, %error, "deferred replay failed; re-attaching");
                                    return self.schedule_reattach();
                                }
                            }
                        }
                        Err(error) => {
                            tracing::debug!(key = %self.key, %error, "snapshot READY failed; re-attaching");
                            return self.schedule_reattach();
                        }
                    }
                }
                self.drive_decoder(tab, feed)
            }
        }
    }

    /// Check a frame's place in the stream: `seq` must be the one the
    /// next PTY frame carries (or, for a `GRID`, the one before it).
    fn in_sequence(&mut self, seq: u64, what: &str) -> Result<(), AttachStep> {
        let Some(expected) = self.expected_next_seq() else {
            tracing::debug!(key = %self.key, "{what} before an accepted handshake; re-attaching");
            return Err(self.schedule_reattach());
        };
        if seq != expected {
            // A gap or a duplicate: the terminal would silently diverge
            // and could never tell. Fatal by contract; re-attach.
            tracing::debug!(
                key = %self.key, seq, expected,
                "{what} seq discontinuity; re-attaching"
            );
            return Err(self.schedule_reattach());
        }
        Ok(())
    }

    fn apply_pty(
        &mut self,
        seq: u64,
        bytes: Vec<u8>,
        tab: &mut super::terminal_tab::TerminalTab,
    ) -> AttachStep {
        if let Err(step) = self.in_sequence(seq, "PTY") {
            return step;
        }
        match &mut self.phase {
            Phase::Hydrating(hydration) => {
                hydration.identity.next_seq += 1;
                match hydration.live_decoder() {
                    // Past READY the decoder interleaves live bytes with
                    // the history it is still stepping.
                    Some(decoder) => match decoder.vt_write(&bytes) {
                        Ok(()) => AttachStep::Refresh,
                        Err(error) => {
                            tracing::debug!(key = %self.key, %error, "hydration vt_write failed; re-attaching");
                            self.schedule_reattach()
                        }
                    },
                    // Nothing can take them yet: a snapshot short of
                    // READY, or a `vt` payload, which is only a screen
                    // once it is whole. Hold them in arrival order.
                    None => {
                        hydration.deferred_bytes += bytes.len();
                        if hydration.deferred_bytes > MAX_DEFERRED_BYTES {
                            tracing::debug!(
                                key = %self.key,
                                "peer streamed PTY past the hold budget; re-attaching"
                            );
                            return self.schedule_reattach();
                        }
                        hydration.deferred.push_back(Deferred::Pty(bytes));
                        AttachStep::None
                    }
                }
            }
            Phase::Live => {
                let Some(resume) = &mut self.resume else {
                    unreachable!("Live always has a resume point");
                };
                resume.next_seq += 1;
                tab.write_vt(&bytes);
                AttachStep::Refresh
            }
            Phase::Requesting | Phase::Ended => AttachStep::None,
        }
    }

    /// Follow a `GRID`: the mirror takes the tab's grid at the frame's
    /// place in the stream (plan 084 D6) — the hydrating terminal while
    /// a payload is in flight, the rendered one once live. Never echoed:
    /// the window's own grid stays what it is, and no RESIZE goes out.
    fn apply_grid(
        &mut self,
        seq: u64,
        grid: (u16, u16),
        owner: Option<SizeOwner>,
        tab: &mut super::terminal_tab::TerminalTab,
    ) -> AttachStep {
        if let Err(step) = self.in_sequence(seq.saturating_add(1), "GRID") {
            return step;
        }
        self.owner = owner;
        let cells = self.advisory;
        match &mut self.phase {
            Phase::Hydrating(hydration) => {
                let Some(decoder) = hydration.live_decoder() else {
                    hydration.deferred.push_back(Deferred::Grid(grid));
                    return AttachStep::Refresh;
                };
                // A resize mid-history forfeits the pages still to come,
                // which the decoder reports as zero-row pages.
                if let Err(error) = decoder.resize(grid.0, grid.1, cells.cell_w, cells.cell_h) {
                    tracing::debug!(key = %self.key, %error, "decoder GRID failed; re-attaching");
                    return self.schedule_reattach();
                }
                hydration.built_at = grid;
                trace_grid(self.trace.as_ref(), grid);
                AttachStep::Refresh
            }
            Phase::Live => {
                if let Err(error) = tab.resize_for_host(grid.0, grid.1, cells.cell_w, cells.cell_h)
                {
                    tracing::warn!(key = %self.key, %error, "mirror GRID failed; re-attaching");
                    return self.schedule_reattach();
                }
                self.pty_grid = Some(grid);
                trace_grid(self.trace.as_ref(), grid);
                AttachStep::Refresh
            }
            Phase::Requesting | Phase::Ended => AttachStep::None,
        }
    }

    /// Step queued history pages within the per-pass budget; on FINISH,
    /// swap the hydrated terminal into the tab and flush the withheld
    /// resize. Re-arms itself through the feed while pages remain, so a
    /// deep scrollback hydrates across passes instead of inside one.
    fn drive_decoder(
        &mut self,
        tab: &mut super::terminal_tab::TerminalTab,
        feed: &EngineFeedSender,
    ) -> AttachStep {
        let Phase::Hydrating(hydration) = &mut self.phase else {
            return AttachStep::None;
        };
        let Some(decoder) = hydration.live_decoder() else {
            return AttachStep::None;
        };
        for _ in 0..PAGES_PER_PASS {
            match decoder.try_next() {
                Ok(HistoryStep::NeedMoreBytes) => return AttachStep::None,
                Ok(HistoryStep::Page { .. } | HistoryStep::Image { .. }) => {}
                Ok(HistoryStep::Finished) => return self.finish_hydration(tab),
                Err(error) => {
                    tracing::debug!(key = %self.key, %error, "history decode failed; re-attaching");
                    return self.schedule_reattach();
                }
            }
        }
        if !hydration.stepping {
            hydration.stepping = true;
            feed.send(EngineFeed::HostTab(
                self.key,
                HostTabFrame::StepDecoder {
                    attempt: self.attempt,
                },
            ));
        }
        AttachStep::None
    }

    fn finish_hydration(&mut self, tab: &mut super::terminal_tab::TerminalTab) -> AttachStep {
        let Phase::Hydrating(hydration) = std::mem::replace(&mut self.phase, Phase::Live) else {
            unreachable!("finish_hydration is only called from the hydrating arm");
        };
        let identity = hydration.identity;
        let built_at = hydration.built_at;
        let terminal = match hydration.hydrator {
            Hydrator::Snapshot(decoder) => match decoder.finish() {
                Ok(decoded) => decoded.terminal,
                Err(error) => {
                    tracing::debug!(key = %self.key, %error, "snapshot finish failed; re-attaching");
                    return self.schedule_reattach();
                }
            },
            // The terminator already said the payload was whole, and its
            // bytes are in this terminal.
            Hydrator::Vt(terminal) => terminal,
        };
        // At the grid the payload ended at — its own, or the last GRID
        // the hydration applied. The mirror is the tab's grid; the
        // window draws it at whatever offset that takes.
        if let Err(error) = tab.swap_terminal(terminal, built_at.0, built_at.1) {
            tracing::warn!(key = %self.key, %error, "hydrated terminal swap failed; re-attaching");
            return self.schedule_reattach();
        }
        // Only now is the hydration's fence true of the rendered
        // terminal — this is what makes an aborted hydration resume from
        // the OLD point instead of claiming bytes it never applied.
        self.resume = Some(identity);
        self.pty_grid = Some(built_at);
        // The stream proved itself end to end; the next failure starts
        // the ladder from the bottom.
        self.reset_ladder();
        // Held through hydration; send it now, in order behind any
        // buffered input.
        self.flush_withheld();
        AttachStep::Refresh
    }
}

/// Append `action` to `trace`, when the trace is on.
fn record(trace: Option<&SizeTrace>, action: SizeTraceAction) {
    if let Some(Ok(mut trace)) = trace.map(|trace| trace.lock()) {
        trace.push(action);
    }
}

fn trace_grid(trace: Option<&SizeTrace>, (cols, rows): (u16, u16)) {
    record(trace, SizeTraceAction::GridApplied { cols, rows });
}

/// Put `frame` on the feed after `delay`. Must be called inside the app
/// runtime.
fn spawn_after(
    delay: Duration,
    key: TabKey,
    feed: &EngineFeedSender,
    frame: HostTabFrame,
) -> tokio::task::AbortHandle {
    let feed = feed.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        feed.send(EngineFeed::HostTab(key, frame));
    })
    .abort_handle()
}

/// Whether a `tab.dump` the engine flagged `defer_unless_streaming` is
/// the session's to answer (#515): a slot tab keeps its terminal, frozen,
/// after its stream stops, and before the stream is up it has not caught
/// up yet — only a streaming one is current.
pub(super) fn dump_defers_to_session(
    defer_unless_streaming: bool,
    attach: Option<&HostAttach>,
) -> bool {
    defer_unless_streaming && !attach.is_some_and(HostAttach::streaming)
}

fn frame_attempt(frame: &HostTabFrame) -> u64 {
    match frame {
        HostTabFrame::Accepted { attempt, .. }
        | HostTabFrame::Failed { attempt, .. }
        | HostTabFrame::PermitRefused { attempt, .. }
        | HostTabFrame::Snap { attempt, .. }
        | HostTabFrame::Pty { attempt, .. }
        | HostTabFrame::Exit { attempt, .. }
        | HostTabFrame::Grid { attempt, .. }
        | HostTabFrame::Error { attempt, .. }
        | HostTabFrame::Closed { attempt }
        | HostTabFrame::ReattachDue { attempt }
        | HostTabFrame::WithholdDeadline { attempt }
        | HostTabFrame::StepDecoder { attempt }
        | HostTabFrame::ScrollSettled { attempt, .. } => *attempt,
    }
}

/// One measured cell metric, narrowed to what the handshake carries.
///
/// The UI measures in `u32` and nothing upstream bounds it: a configured
/// font size is deliberately not capped at the interactive zoom limit,
/// and the renderer accepts any finite cell of at least one pixel. So
/// the narrowing is real, and a bare `as` is the one thing it must not
/// be — 65536 truncates to `0`, which the session reads as "this client
/// has no pixel metrics", and anything above wraps to a small, plausible
/// lie.
///
/// Clamped rather than refused: the number is advisory. It is what the
/// child reports for mode 2048 and nothing is rendered from it, so a
/// font or zoom that somehow measured a cell wider than 65535 px should
/// cost a wrong pixel report, not a tab that cannot attach at all. Zero
/// passes through unchanged — that is the wire's own "unknown".
fn cell_px(measured: u32) -> u16 {
    u16::try_from(measured).unwrap_or(u16::MAX)
}

/// The line this attempt opens with: the tab, the terms, and the resume
/// identity when there is one to hand back.
///
/// A resume point is asked for whenever one survives. Nothing here
/// pre-checks it: there is no control leg left to check it against, and
/// a miss on the epoch or the generation is answered with
/// `mode: "snapshot"` in the accepted line rather than an error. A point
/// from a *different* session cannot reach this at all — it is keyed by
/// connection incarnation and purged with it.
fn choose_handshake(
    tab_id: i64,
    terms: AttachHandshakeTerms,
    resume: Option<ResumePoint>,
) -> AttachHandshake {
    match resume {
        Some(r) => {
            AttachHandshake::resume(tab_id, terms, r.next_seq, r.server_epoch, r.tab_generation)
        }
        None => AttachHandshake::snapshot(tab_id, terms),
    }
}

/// What the payload that is about to arrive will be decoded as.
///
/// The accepted line is the authority: the server picks from the kinds
/// the handshake offered, and the bytes behind it are what that server
/// composed. It can still name something this client cannot decode —
/// `AttachPayloadKind` is an open string so a newer session's list is
/// readable — and decoding by guess is how a corrupt screen ships.
/// Re-attaching is the recovery, as it is for every other stream this
/// client cannot trust.
fn negotiated_kind(accepted: &AttachPayloadKind) -> Result<PayloadKind, String> {
    PayloadKind::from_wire(accepted)
        .ok_or_else(|| format!("the session accepted {accepted}, which this client never offered"))
}

/// Plan §3.4's ERROR mapping: which refusals are terminal, which mean
/// the host connection owns the recovery, and which are worth a retry.
/// The one table both refusal paths below classify against.
fn reason_for(code: Option<&ServerCode>, message: String) -> FailReason {
    match code {
        Some(ServerCode::BuildMismatch) => FailReason::BuildMismatch(message),
        Some(ServerCode::ShuttingDown) => FailReason::HostGone(message),
        _ => FailReason::Retryable(message),
    }
}

/// What a refused queue permit means for the attach.
///
/// Three rows, because a permit is refused in exactly three ways. Only a
/// full queue is worth a retry; the other two say the host connection
/// owns this, not the tab. A permit refused *after* the grant, by the
/// fence it was granted under ([`AttachPermit::guarding`]), is the
/// `Disconnected` row and no new one — the connection went away, which
/// is what `Disconnected` means.
fn classify_permit_failure(error: &HostOpError) -> FailReason {
    match error {
        HostOpError::QueueFull => FailReason::Retryable(error.to_string()),
        // The queue was flushed before the permit came up, or the
        // connection it was granted for ended before the dial.
        HostOpError::Disconnected
        // Nothing is left to serve the queue at all.
        | HostOpError::WorkerGone
        // Nothing else can reach a barrier — it is answered by the
        // worker itself and never put on the wire, so there is no
        // session refusal, no dead wire and no upload error to sort. Not
        // given arms of their own, because the answer would be the same:
        // whatever refused the permit, this attach is not the thing that
        // recovers from it.
        | HostOpError::Rejected { .. }
        | HostOpError::Transport(_)
        | HostOpError::Local(_) => FailReason::HostGone(error.to_string()),
    }
}

/// Classify an attach-op or dial refusal, applied one step earlier in
/// the lifecycle.
fn classify_failure(error: &ClientError) -> FailReason {
    reason_for(error.server_code().as_ref(), error.to_string())
}

/// Where an attempt's data connection goes and the line it opens with.
struct Dial {
    socket: std::path::PathBuf,
    handshake: AttachHandshake,
}

/// The background half of one attempt: take the queue permit, dial the
/// data connection, then split into a reader loop (frames → feed) and a
/// writer loop (input queue → wire). Every await lives out here; the
/// main thread only ever sees feed items.
///
/// The permit is the *only* thing this takes from the op queue — see
/// [`HostOps::attach_permit`]. Everything after it happens on a socket
/// of its own, so a slow session delays this tab and nothing else.
async fn run_attempt(
    key: TabKey,
    attempt: u64,
    permit: impl std::future::Future<Output = Result<AttachPermit, HostOpError>>,
    dial: Dial,
    input_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<HostDataMsg>>>,
    detaching: watch::Receiver<bool>,
    feed: EngineFeedSender,
) {
    let Dial { socket, handshake } = dial;
    // Once the attach is detaching nothing it says applies to anything:
    // the tab may already be on a new attach that this attempt's frames
    // would corrupt.
    let publish = |frame: HostTabFrame| {
        if !*detaching.borrow() {
            feed.send(EngineFeed::HostTab(key, frame));
        }
    };
    let fail = |reason: FailReason| publish(HostTabFrame::Failed { attempt, reason });
    let refused = |error: HostOpError| publish(HostTabFrame::PermitRefused { attempt, error });
    let permit = match permit.await {
        Ok(permit) => permit,
        Err(error) => return refused(error),
    };
    // Detached while it queued: nothing has reached the session to
    // cancel, and nothing should.
    if *detaching.borrow() {
        return;
    }
    // Bounded, on the same budget a control leg gets. `DataConnection::dial`
    // has no timeout of its own, and the socket it dials is not always a
    // local one: over the ssh transport it is a bridge whose accept is a
    // remote `ssh` exec, so an unreachable host would otherwise park this
    // attempt forever with the tab showing "attaching…" and no retry. A
    // timeout is a failed dial like any other — retryable, and the
    // re-attach backoff decides what happens next.
    let budget = crate::host_conn::leg_budget();
    // The accept is published the moment its line is parsed: the session
    // committed the attach when it wrote it, so the explicit intent is
    // spent even if the preamble behind it never comes.
    let decodable = std::sync::atomic::AtomicBool::new(false);
    let announce =
        |accepted: &roost_ipc::messages::AttachAccepted| match negotiated_kind(&accepted.kind) {
            Ok(kind) => {
                decodable.store(true, std::sync::atomic::Ordering::Relaxed);
                publish(HostTabFrame::Accepted {
                    attempt,
                    resumed: accepted.mode == roost_ipc::messages::AttachMode::Resume,
                    kind,
                    fence: accepted.seq,
                    server_epoch: accepted.server_epoch,
                    tab_generation: accepted.tab_generation,
                    grid: (accepted.snapshot_cols, accepted.snapshot_rows),
                    owner: accepted.owner.clone(),
                });
            }
            Err(message) => fail(FailReason::Retryable(message)),
        };
    // Through the permit, never beside it: the grant said where the
    // queue stood, and the connection it was granted for can end before
    // this opens anything ([`AttachPermit::guarding`]).
    let dialed = permit
        .guarding(tokio::time::timeout(
            budget,
            DataConnection::dial_unless(
                &socket,
                &handshake,
                until_detached(detaching.clone()),
                announce,
            ),
        ))
        .await;
    let conn = match dialed {
        Ok(Ok(Ok(Some((_, conn))))) => conn,
        // Detached on the way: `DETACH` went out behind the handshake
        // line, and nothing will apply what the session answers.
        Ok(Ok(Ok(None))) => return,
        Ok(Ok(Err(error))) => return fail(classify_failure(&error)),
        Ok(Err(_elapsed)) => {
            return fail(FailReason::Retryable(format!(
                "attaching to {} timed out after {}s",
                socket.display(),
                budget.as_secs().max(1)
            )))
        }
        // Refused by the fence: the connection this attach was admitted
        // for is gone.
        Err(error) => return refused(error),
    };
    if !decodable.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut rx = input_rx.lock().await;
    stream_attempt(key, attempt, conn, &mut rx, &detaching, &feed).await;
}

/// Resolves once the attach is detaching — never, for one whose state was
/// dropped without a detach.
async fn until_detached(mut detaching: watch::Receiver<bool>) {
    if detaching.wait_for(|detaching| *detaching).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// One accepted attempt's connection, until its reader ends it — or, once
/// the attach is detaching, until `DETACH` is written.
async fn stream_attempt(
    key: TabKey,
    attempt: u64,
    conn: DataConnection,
    rx: &mut mpsc::UnboundedReceiver<HostDataMsg>,
    detaching: &watch::Receiver<bool>,
    feed: &EngineFeedSender,
) {
    let (mut reader, mut writer) = conn.into_split();
    // Both halves are futures of THIS task rather than spawned children,
    // so aborting the attempt takes the writer with it and releases the
    // shared input queue's lock synchronously. A spawned writer would
    // merely detach on abort and stay parked in `recv()` still holding
    // that lock, and the next attempt's writer would block behind the
    // corpse — swallowing the first keystroke typed after a re-attach.
    // The lock serializes attempts over the persistent queue: a
    // previous attempt's writer holds it until dropped, and this one
    // takes over draining the same keystrokes.
    //
    // ONE loop over both halves, and only the READER decides when the
    // attempt is over. A write error just parks the writer branch: the
    // server labels its closes (`superseded`, `ERROR`
    // desync…), and cancelling the reader on a write failure would lose
    // the label already queued behind it — turning a passive detach
    // into a re-attach loop. The queue keeps buffering for the retry.
    let mut writer_dead = false;
    loop {
        tokio::select! {
            read = read_server_frame(&mut reader) => match read {
                // Draining for its `DETACH` only: whatever it reads, its own
                // end included, is no news for a tab whose next attach may
                // already be on this attempt number.
                Ok(Some(_)) if *detaching.borrow() => {}
                _ if *detaching.borrow() => return,
                Ok(Some(frame)) => {
                    let done = matches!(frame, ServerFrame::Exit { .. } | ServerFrame::Error(_));
                    feed.send(EngineFeed::HostTab(key, lift_frame(attempt, frame)));
                    if done {
                        return;
                    }
                }
                Ok(None) => {
                    feed.send(EngineFeed::HostTab(key, HostTabFrame::Closed { attempt }));
                    return;
                }
                Err(error) => {
                    tracing::debug!(%key, %error, "data connection read failed");
                    feed.send(EngineFeed::HostTab(key, HostTabFrame::Closed { attempt }));
                    return;
                }
            },
            msg = rx.recv(), if !writer_dead => {
                let Some(msg) = msg else {
                    // The queue's senders are gone: the tab is being
                    // dropped; the reader half winds the attempt down.
                    writer_dead = true;
                    continue;
                };
                let outcome = match msg {
                    HostDataMsg::Input { bytes, take } => writer.send_input(&bytes, take).await,
                    HostDataMsg::Resize {
                        cols,
                        rows,
                        cell_w,
                        cell_h,
                        focused,
                    } => {
                        writer
                            .send_resize(cols, rows, cell_px(cell_w), cell_px(cell_h), focused)
                            .await
                    }
                    HostDataMsg::Take => writer.send_take().await,
                    // Terminal on the wire: nothing more is written, and
                    // the attempt is over.
                    HostDataMsg::Detach => {
                        let _ = writer.send_detach().await;
                        return;
                    }
                };
                if outcome.is_err() {
                    writer_dead = true;
                }
            },
        }
    }
}

async fn read_server_frame(
    reader: &mut roost_ipc::dataframe::DataFrameReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<Option<ServerFrame>, roost_ipc::Error> {
    match reader.next_frame().await? {
        Some(frame) => Ok(Some(ServerFrame::decode(frame)?)),
        None => Ok(None),
    }
}

fn lift_frame(attempt: u64, frame: ServerFrame) -> HostTabFrame {
    match frame {
        ServerFrame::Snap(bytes) => HostTabFrame::Snap { attempt, bytes },
        ServerFrame::Pty { seq, bytes } => HostTabFrame::Pty {
            attempt,
            seq,
            bytes,
        },
        ServerFrame::Exit { final_seq, code } => HostTabFrame::Exit {
            attempt,
            final_seq,
            code,
        },
        ServerFrame::Error(error) => HostTabFrame::Error {
            attempt,
            code: error.code,
            message: error.message,
        },
        ServerFrame::Grid(grid) => HostTabFrame::Grid {
            attempt,
            seq: grid.seq,
            grid: (grid.cols, grid.rows),
            owner: grid.owner,
        },
    }
}

#[cfg(test)]
mod tests {
    use roost_ui_model::keys::HostId;
    use roost_vt::{Terminal, TerminalOptions};

    use super::*;
    use crate::app::terminal_tab::TerminalTab;
    use crate::engine_feed::{self, EngineFeedReceiver};
    use crate::host_conn::queue::HostIntent;

    const GEOMETRY: Geometry = Geometry {
        cols: 80,
        rows: 24,
        cell_w: 9,
        cell_h: 18,
    };

    impl Geometry {
        fn grid(&self) -> (u16, u16) {
            (self.cols, self.rows)
        }
    }

    fn key() -> TabKey {
        TabKey::new(HostId::new(3), 7)
    }

    /// This window's client id, and another client's.
    const ME: &str = "iced-test";
    const DESK: &str = "desk-1";

    fn owner(client_id: &str) -> Option<SizeOwner> {
        Some(SizeOwner {
            client_id: client_id.into(),
            label: format!("{client_id}-label"),
        })
    }

    /// The trace, without the accepts — the cases that count those use
    /// [`accepts_of`].
    fn trace_of(attach: &HostAttach) -> Vec<SizeTraceAction> {
        full_trace(attach)
            .into_iter()
            .filter(|action| !matches!(action, SizeTraceAction::AttachAccepted { .. }))
            .collect()
    }

    fn full_trace(attach: &HostAttach) -> Vec<SizeTraceAction> {
        attach
            .trace
            .as_ref()
            .expect("a traced attach")
            .lock()
            .expect("trace lock")
            .clone()
    }

    /// The terms [`HostAttach::begin`] assembles, for the cases that
    /// read the handshake rather than build one.
    fn terms() -> AttachHandshakeTerms {
        AttachHandshakeTerms {
            session_id: "sess-1".into(),
            kinds: CLIENT_PAYLOAD_KINDS.map(AttachPayloadKind::from).to_vec(),
            cols: GEOMETRY.cols,
            rows: GEOMETRY.rows,
            cell_w_px: cell_px(GEOMETRY.cell_w),
            cell_h_px: cell_px(GEOMETRY.cell_h),
            libghostty_build: "gb".into(),
            take: true,
            client_id: "iced-test".into(),
        }
    }

    /// A machine plus the tab it drives and the feed its timers write to.
    fn rig() -> (
        HostAttach,
        TerminalTab,
        EngineFeedSender,
        EngineFeedReceiver,
    ) {
        let (feed_tx, feed_rx) = engine_feed::channel();
        let mut attach = HostAttach::new(key(), GEOMETRY, AttachIntent::Explicit)
            .with_trace(Some(SizeTrace::default()));
        // The frames below are hand-fed, so the machine must be on the
        // attempt they carry, and it attaches as this client.
        attach.attempt = 1;
        attach.client_id = ME.into();
        let (tab, _capture) = crate::app::terminal_tab::attach_test_host_terminal(
            GEOMETRY.cols,
            GEOMETRY.rows,
            attach.input_tx(),
        );
        (attach, tab, feed_tx, feed_rx)
    }

    fn accepted(resumed: bool, fence: u64) -> HostTabFrame {
        accepted_as(PayloadKind::GhosttySnapshot, resumed, fence)
    }

    fn accepted_as(kind: PayloadKind, resumed: bool, fence: u64) -> HostTabFrame {
        accepted_at(kind, resumed, fence, GEOMETRY.grid(), owner(ME))
    }

    /// The accepted handshake with the tab's own grid and owner, which
    /// are this window's only when it took the tab.
    fn accepted_at(
        kind: PayloadKind,
        resumed: bool,
        fence: u64,
        grid: (u16, u16),
        owner: Option<SizeOwner>,
    ) -> HostTabFrame {
        HostTabFrame::Accepted {
            attempt: 1,
            resumed,
            kind,
            fence,
            server_epoch: 11,
            tab_generation: 2,
            grid,
            owner,
        }
    }

    fn grid(seq: u64, grid: (u16, u16), owner: Option<SizeOwner>) -> HostTabFrame {
        HostTabFrame::Grid {
            attempt: 1,
            seq,
            grid,
            owner,
        }
    }

    fn snap(bytes: &[u8]) -> HostTabFrame {
        HostTabFrame::Snap {
            attempt: 1,
            bytes: bytes.to_vec(),
        }
    }

    fn screen(tab: &mut TerminalTab) -> String {
        tab.refresh_snapshot().expect("refresh");
        tab.dump(0).expect("dump").rows_text.join("\n")
    }

    fn pty(seq: u64, bytes: &[u8]) -> HostTabFrame {
        HostTabFrame::Pty {
            attempt: 1,
            seq,
            bytes: bytes.to_vec(),
        }
    }

    /// A real encoded snapshot (READY through FINISH) with `marker`
    /// visible on screen — what the wire's SNAP frames carry.
    fn snapshot_with(marker: &str) -> Vec<u8> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: GEOMETRY.cols,
            rows: GEOMETRY.rows,
            max_scrollback: 200,
            continuation_max_bytes: 0,
        })
        .expect("terminal");
        terminal.vt_write(marker.as_bytes());
        terminal.snapshot().expect("encode snapshot")
    }

    /// Drive a machine through a whole snapshot, servicing its
    /// `StepDecoder` self-wakes off the feed like the drain would.
    fn hydrate_fully(
        attach: &mut HostAttach,
        tab: &mut TerminalTab,
        feed_tx: &EngineFeedSender,
        feed_rx: &mut EngineFeedReceiver,
        bytes: Vec<u8>,
    ) -> AttachStep {
        let attempt = attach.attempt;
        let mut step = attach.on_frame(HostTabFrame::Snap { attempt, bytes }, tab, feed_tx);
        loop {
            let mut batch = crate::engine_feed::EngineBatch::default();
            let Some(item) = feed_rx.try_next(&mut batch) else {
                return step;
            };
            if let EngineFeed::HostTab(_, frame @ HostTabFrame::StepDecoder { .. }) = item {
                step = attach.on_frame(frame, tab, feed_tx);
            }
        }
    }

    /// `mode: "resume"` skips hydration entirely: the surviving terminal
    /// is the base and PTY applies straight to it from the fence.
    #[tokio::test]
    async fn resume_hit_goes_straight_to_live() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        assert_eq!(
            attach.on_frame(accepted(true, 41), &mut tab, &feed_tx),
            AttachStep::Refresh,
            "the owner it names is news for the tab band"
        );
        assert!(matches!(attach.phase, Phase::Live));
        assert_eq!(
            attach.on_frame(pty(42, b"after-resume"), &mut tab, &feed_tx),
            AttachStep::Refresh
        );
        tab.refresh_snapshot().expect("refresh");
        assert!(
            tab.dump(0)
                .expect("dump")
                .rows_text
                .join("\n")
                .contains("after-resume"),
            "resumed PTY applies to the surviving terminal"
        );
    }

    const POINT: ResumePoint = ResumePoint {
        server_epoch: 11,
        tab_generation: 2,
        next_seq: 42,
    };

    const WIDER: Geometry = Geometry {
        cols: 120,
        ..GEOMETRY
    };

    /// The window resizing under the attach the way `App::resize` does
    /// it: the attach hears the new geometry, and the mirror keeps the
    /// tab's grid.
    fn widen(attach: &mut HostAttach) {
        attach.note_resize(WIDER, true);
    }

    #[test]
    fn a_resume_survives_only_a_regrid_that_moves_no_cell() {
        assert!(resume_survives_regrid((80, 24), (80, 24)));
        assert!(!resume_survives_regrid((80, 24), (120, 24)), "wider");
        assert!(!resume_survives_regrid((80, 24), (80, 30)), "taller");

        let moved = key();
        let other = TabKey::new(HostId::new(3), 8);
        let mut resumes = HashMap::from([(moved, POINT), (other, POINT)]);
        forget_resume_on_regrid(&mut resumes, moved, (80, 24), (80, 24));
        assert_eq!(resumes.len(), 2, "no cell moved");
        forget_resume_on_regrid(&mut resumes, moved, (80, 24), (120, 24));
        assert_eq!(
            resumes,
            HashMap::from([(other, POINT)]),
            "only the re-gridded tab forgets its point"
        );
    }

    /// Plan 072 D4a, the in-flight half (#564), now keyed off the
    /// session's grid: a resume the session cut at a grid the mirror is
    /// not at is refused, and a fresh attach goes at once.
    #[tokio::test]
    async fn an_accepted_resume_at_a_grid_the_mirror_is_not_at_is_discarded() {
        let (attach, mut tab, feed_tx, _feed_rx) = rig();
        let mut attach = attach.with_resume(Some(POINT));
        assert_eq!(
            attach.on_frame(
                accepted_at(
                    PayloadKind::GhosttySnapshot,
                    true,
                    41,
                    WIDER.grid(),
                    owner(ME)
                ),
                &mut tab,
                &feed_tx
            ),
            AttachStep::Reattach {
                delay: Duration::ZERO
            },
            "the resumed stream is refused and a fresh attach goes at once"
        );
        assert_eq!(attach.resume, None, "the fresh attach asks for a snapshot");
        assert!(matches!(attach.phase, Phase::Requesting));
    }

    /// Plan 072 D4a, the detach guard (#564): the point survives only
    /// while the kept terminal is at the grid the session last said it
    /// is at.
    #[tokio::test]
    async fn a_detach_keeps_the_resume_point_only_at_the_grid_the_session_said() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        assert_eq!(
            attach.detach(Some(tab.grid())),
            Some(POINT),
            "the kept terminal is at the session's grid"
        );

        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        tab.resize_for_host(WIDER.cols, WIDER.rows, WIDER.cell_w, WIDER.cell_h)
            .expect("re-grid the kept terminal");
        assert_eq!(
            attach.detach(Some(tab.grid())),
            None,
            "a kept terminal at a grid the session never named"
        );

        let (attach, mut tab, _feed_tx, _feed_rx) = rig();
        let attach = attach.with_resume(Some(POINT));
        assert_eq!(
            attach.detach(Some(tab.grid())),
            Some(POINT),
            "an attach that never heard a grid hands the point back untouched"
        );
        let _ = &mut tab;
    }

    /// A window resize never moves the mirror, so it costs the resume
    /// point nothing: the session's own `Regridded` refusal covers a
    /// resize it applied before this client heard of it. A `GRID` moves
    /// the mirror and the point together.
    #[tokio::test]
    async fn a_window_resize_keeps_the_resume_point_and_a_grid_moves_it_with_the_mirror() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        widen(&mut attach);
        assert_eq!(
            tab.grid(),
            GEOMETRY.grid(),
            "the mirror keeps the tab's grid"
        );
        assert_eq!(
            attach.detach(Some(tab.grid())),
            Some(POINT),
            "resized live: the refocus still resumes"
        );

        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        attach.on_frame(pty(42, b"at the old grid"), &mut tab, &feed_tx);
        assert_eq!(
            attach.on_frame(grid(42, WIDER.grid(), owner(ME)), &mut tab, &feed_tx),
            AttachStep::Refresh
        );
        assert_eq!(tab.grid(), WIDER.grid());
        assert_eq!(
            attach.detach(Some(tab.grid())),
            Some(ResumePoint {
                next_seq: 43,
                ..POINT
            }),
            "the GRID moved the mirror and the point it resumes from"
        );

        let (ops, _worker) = HostOps::channel();
        let nowhere = std::path::PathBuf::from("/nonexistent/roost-resize-resume.sock");
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        widen(&mut attach);
        assert!(matches!(
            attach.on_frame(HostTabFrame::Closed { attempt: 1 }, &mut tab, &feed_tx),
            AttachStep::Reattach { .. }
        ));
        attach.begin(&ops, None, nowhere, "gb", ME, &feed_tx);
        assert_eq!(
            attach.resume,
            Some(POINT),
            "a re-attach after a resize resumes"
        );
    }

    /// `mode: "snapshot"` (the resume-miss fallback rides the same
    /// reply) hydrates: the old terminal keeps rendering until FINISH
    /// swaps the decoded one in.
    #[tokio::test]
    async fn resume_miss_hydrates_and_swaps_at_finish() {
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        tab.write_vt(b"old-screen");
        tab.refresh_snapshot().expect("refresh");
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        assert!(matches!(attach.phase, Phase::Hydrating(_)));
        assert!(
            tab.dump(0)
                .expect("dump")
                .rows_text
                .join("\n")
                .contains("old-screen"),
            "the old terminal renders through hydration — never blank"
        );
        let step = hydrate_fully(
            &mut attach,
            &mut tab,
            &feed_tx,
            &mut feed_rx,
            snapshot_with("fresh-host-screen"),
        );
        assert_eq!(step, AttachStep::Refresh);
        assert!(matches!(attach.phase, Phase::Live));
        assert_eq!(
            attach.payload_kind(),
            Some(PayloadKind::GhosttySnapshot),
            "what host.status reports is what was accepted"
        );
        tab.refresh_snapshot().expect("refresh");
        let text = tab.dump(0).expect("dump").rows_text.join("\n");
        assert!(
            text.contains("fresh-host-screen"),
            "FINISH swaps the hydrated terminal in: {text:?}"
        );
        assert!(!text.contains("old-screen"));
    }

    /// PTY frames that beat READY are deferred and replayed in order —
    /// pre-READY output is never lost.
    #[tokio::test]
    async fn pre_ready_pty_is_deferred_and_replayed() {
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        assert_eq!(
            attach.on_frame(pty(101, b"early-bytes"), &mut tab, &feed_tx),
            AttachStep::None,
            "pre-READY PTY defers"
        );
        hydrate_fully(
            &mut attach,
            &mut tab,
            &feed_tx,
            &mut feed_rx,
            snapshot_with("base"),
        );
        tab.refresh_snapshot().expect("refresh");
        let text = tab.dump(0).expect("dump").rows_text.join("\n");
        assert!(
            text.contains("early-bytes"),
            "the deferral replays into the hydrated terminal: {text:?}"
        );
    }

    /// A gap is fatal: the terminal would silently diverge. Re-attach.
    #[tokio::test]
    async fn a_seq_gap_forces_a_reattach() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        attach.on_frame(pty(42, b"ok"), &mut tab, &feed_tx);
        assert!(matches!(
            attach.on_frame(pty(44, b"skipped 43"), &mut tab, &feed_tx),
            AttachStep::Reattach { .. }
        ));
    }

    /// A duplicate is the same fatality as a gap.
    #[tokio::test]
    async fn a_duplicate_seq_forces_a_reattach() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        attach.on_frame(pty(42, b"ok"), &mut tab, &feed_tx);
        assert!(matches!(
            attach.on_frame(pty(42, b"again"), &mut tab, &feed_tx),
            AttachStep::Reattach { .. }
        ));
    }

    /// The line a data connection opens with: the tab, every negotiated
    /// term beside it, and the resume identity only when this tab has
    /// one to hand back.
    #[test]
    fn the_handshake_states_the_tab_the_terms_and_any_resume_point() {
        let fresh = choose_handshake(7, terms(), None);
        assert_eq!(fresh.attach, "7", "the tab id, as a string_int64");
        assert_eq!(fresh.resume_from_seq, None);
        assert_eq!(fresh.server_epoch, None);
        assert_eq!(fresh.tab_generation, None);
        let stated = &fresh.terms;
        assert_eq!(stated.session_id, "sess-1");
        assert_eq!(
            stated.kinds,
            CLIENT_PAYLOAD_KINDS.map(AttachPayloadKind::from),
            "in this client's preference order"
        );
        assert_eq!((stated.cols, stated.rows), (80, 24));
        assert!(stated.take, "this client only attaches what it shows");
        assert_eq!(stated.client_id, "iced-test");

        let resumed = choose_handshake(
            7,
            terms(),
            Some(ResumePoint {
                server_epoch: 11,
                tab_generation: 2,
                next_seq: 42,
            }),
        );
        assert_eq!(resumed.resume_from_seq, Some(42));
        assert_eq!(resumed.server_epoch, Some(11));
        assert_eq!(resumed.tab_generation, Some(2));
        assert_eq!(
            resumed.terms.session_id, "sess-1",
            "a resume states the same terms as a snapshot"
        );
    }

    /// EOF mid-hydration (before FINISH) abandons the decoder and
    /// re-attaches — never a half-hydrated swap.
    #[tokio::test]
    async fn eof_before_finish_reattaches() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        assert!(matches!(
            attach.on_frame(HostTabFrame::Closed { attempt: 1 }, &mut tab, &feed_tx),
            AttachStep::Reattach { .. }
        ));
        assert!(matches!(attach.phase, Phase::Requesting));
    }

    /// The ERROR-code table: `shutting-down` detaches passively (the
    /// host connection owns the recovery), `overflow`/`desync` rebuild.
    #[tokio::test]
    async fn error_codes_map_to_their_recoveries() {
        for (code, detaches) in [
            ("shutting-down", true),
            ("overflow", false),
            ("desync", false),
        ] {
            let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
            attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
            let step = attach.on_frame(
                HostTabFrame::Error {
                    attempt: 1,
                    code: code.into(),
                    message: String::new(),
                },
                &mut tab,
                &feed_tx,
            );
            if detaches {
                assert_eq!(step, AttachStep::Detach, "{code}");
            } else {
                assert!(matches!(step, AttachStep::Reattach { .. }), "{code}");
            }
        }
    }

    /// A build-mismatch refusal stops the tab (the host's NeedsRestart
    /// state owns the recovery); a transient failure retries.
    #[tokio::test]
    async fn attach_refusals_split_terminal_from_transient() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        assert_eq!(
            attach.on_frame(
                HostTabFrame::Failed {
                    attempt: 1,
                    reason: FailReason::BuildMismatch("pin moved".into()),
                },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::Detach
        );
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        assert!(matches!(
            attach.on_frame(
                HostTabFrame::Failed {
                    attempt: 1,
                    reason: FailReason::Retryable("connection refused".into()),
                },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::Reattach { .. }
        ));
    }

    /// The re-attach delay doubles and caps under consecutive failures;
    /// only *reaching Live* resets it — an accept from a server that
    /// then dies immediately must not hot-loop at the base delay.
    #[tokio::test]
    async fn backoff_grows_caps_and_resets() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        let mut last = Duration::ZERO;
        for round in 0..8 {
            let AttachStep::Reattach { delay } =
                attach.on_frame(HostTabFrame::Closed { attempt: 1 }, &mut tab, &feed_tx)
            else {
                panic!("round {round} did not re-attach");
            };
            assert!(delay >= last, "round {round}: {delay:?} < {last:?}");
            assert!(delay <= BACKOFF_CAP);
            last = delay;
        }
        assert_eq!(last, BACKOFF_CAP, "the ladder reaches the cap");
        // A resume accept reaches Live directly: the ladder resets.
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        let AttachStep::Reattach { delay } =
            attach.on_frame(HostTabFrame::Closed { attempt: 1 }, &mut tab, &feed_tx)
        else {
            panic!("no re-attach after reset");
        };
        assert_eq!(delay, BACKOFF_BASE);
    }

    /// A resize during hydration is withheld (latest-wins, with the focus
    /// it happened under) and fires once at FINISH.
    #[tokio::test]
    async fn a_withheld_resize_fires_at_finish() {
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        attach.note_resize(
            Geometry {
                cols: 100,
                rows: 30,
                ..GEOMETRY
            },
            true,
        );
        attach.note_resize(
            Geometry {
                cols: 120,
                rows: 40,
                ..GEOMETRY
            },
            false,
        );
        assert!(
            attach.test_drain_input().is_empty(),
            "nothing reaches the wire pre-FINISH"
        );
        hydrate_fully(
            &mut attach,
            &mut tab,
            &feed_tx,
            &mut feed_rx,
            snapshot_with("x"),
        );
        assert_eq!(
            attach.test_drain_input(),
            vec![HostDataMsg::Resize {
                cols: 120,
                rows: 40,
                cell_w: GEOMETRY.cell_w,
                cell_h: GEOMETRY.cell_h,
                focused: false,
            }],
            "latest-wins: one RESIZE, the latest, with the focus it was made under"
        );
        assert_eq!(
            tab.grid(),
            GEOMETRY.grid(),
            "a RESIZE never moves the mirror"
        );
    }

    /// The withhold ends at the 2 s deadline: the RESIZE goes to the
    /// session while the payload is still hydrating, and nothing here
    /// resizes the decoder to this window's grid — a `GRID` back is what
    /// would.
    #[tokio::test]
    async fn a_withheld_resize_is_sent_at_the_deadline_and_moves_nothing_here() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        // Feed exactly the prefix through READY — hydrating, not done.
        let encoded = snapshot_with("mid-hydration");
        let boundary = roost_vt::ready_boundary(&encoded).expect("READY boundary");
        attach.on_frame(
            HostTabFrame::Snap {
                attempt: 1,
                bytes: encoded[..boundary].to_vec(),
            },
            &mut tab,
            &feed_tx,
        );
        attach.note_resize(
            Geometry {
                cols: 132,
                rows: 50,
                ..GEOMETRY
            },
            true,
        );
        assert_eq!(
            attach.on_frame(
                HostTabFrame::WithholdDeadline { attempt: 1 },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::None
        );
        let sent = attach.test_drain_input();
        assert!(matches!(
            sent.as_slice(),
            [HostDataMsg::Resize {
                cols: 132,
                rows: 50,
                focused: true,
                ..
            }]
        ));
        let Phase::Hydrating(hydration) = &attach.phase else {
            panic!("still hydrating");
        };
        assert_eq!(
            hydration.built_at,
            GEOMETRY.grid(),
            "the decoder is untouched"
        );
    }

    /// The same before READY: the RESIZE goes, the stream stays — there
    /// is nothing to re-attach for when the session sizes the mirror.
    #[tokio::test]
    async fn a_deadline_before_ready_sends_the_resize_and_keeps_the_stream() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        attach.note_resize(
            Geometry {
                cols: 132,
                rows: 50,
                ..GEOMETRY
            },
            true,
        );
        assert_eq!(
            attach.on_frame(
                HostTabFrame::WithholdDeadline { attempt: 1 },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::None
        );
        assert!(matches!(attach.phase, Phase::Hydrating(_)));
        assert!(matches!(
            attach.test_drain_input().as_slice(),
            [HostDataMsg::Resize { cols: 132, .. }]
        ));
    }

    /// While live, a resize goes straight out — in order behind input,
    /// with the window's focus as it happened.
    #[tokio::test]
    async fn a_live_resize_queues_behind_input() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        tab.session
            .send_input(b"typed".to_vec(), roost_ipc::dataframe::InputTake::Before);
        attach.note_resize(
            Geometry {
                cols: 90,
                rows: 28,
                ..GEOMETRY
            },
            false,
        );
        let sent = attach.test_drain_input();
        assert!(matches!(
            &sent[0],
            HostDataMsg::Input { bytes, take: roost_ipc::dataframe::InputTake::Before }
                if bytes == b"typed"
        ));
        assert!(matches!(
            sent[1],
            HostDataMsg::Resize {
                cols: 90,
                focused: false,
                ..
            }
        ));
        assert_eq!(
            trace_of(&attach),
            vec![SizeTraceAction::ResizeSent { focused: false }]
        );
    }

    /// Keys typed while a new host tab opens reach it only once its
    /// attach is live on the terminal the session hydrated (plan 072
    /// §D2), and are encoded in that terminal's modes. An attempt that
    /// ends first keeps them in the pending buffer — not in its queue —
    /// for the retry, which then carries them.
    #[tokio::test]
    async fn typed_ahead_waits_for_the_hydrated_terminal_across_a_failed_attach() {
        use super::super::pending_input::{deliver, Facts, PendingInput, PendingKeyboard, Step};

        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        let target = key();
        let now = std::time::Instant::now();
        let mut keyboard = PendingKeyboard::default();
        keyboard.arm(Some((5, target.host)), 0, now);
        for key in ["e", "c", "h", "o", "ArrowUp"] {
            let press = crate::input::synthetic_press(key, None, &[]).expect("a key");
            keyboard.push(PendingInput::Key(press, crate::input::OptionKey::default()));
        }
        keyboard.answered(5, Ok(target), now);
        let step = |keyboard: &PendingKeyboard, attach: &HostAttach| {
            keyboard.step(&Facts {
                now,
                focus_generation: 0,
                connected: true,
                active: target,
                ready: attach.takes_typed_ahead(),
            })
        };
        assert_eq!(step(&keyboard, &attach), Some(Step::Wait), "requesting");

        let failed = attach.on_frame(
            HostTabFrame::Failed {
                attempt: 1,
                reason: FailReason::Retryable("dial refused".into()),
            },
            &mut tab,
            &feed_tx,
        );
        assert!(matches!(failed, AttachStep::Reattach { .. }));
        assert_eq!(
            step(&keyboard, &attach),
            Some(Step::Wait),
            "an attach that ended before it was live"
        );
        assert!(attach.test_drain_input().is_empty());

        attach.attempt = 2;
        attach.on_frame(
            HostTabFrame::Accepted {
                attempt: 2,
                resumed: false,
                kind: PayloadKind::GhosttySnapshot,
                fence: 0,
                server_epoch: 11,
                tab_generation: 2,
                grid: GEOMETRY.grid(),
                owner: owner(ME),
            },
            &mut tab,
            &feed_tx,
        );
        assert!(matches!(attach.phase, Phase::Hydrating(_)));
        assert_eq!(
            step(&keyboard, &attach),
            Some(Step::Wait),
            "accepted, but the tab still renders the stand-in"
        );
        assert!(attach.test_drain_input().is_empty());

        // The session's shell has application cursor keys (DECCKM) on.
        let hydrated = hydrate_fully(
            &mut attach,
            &mut tab,
            &feed_tx,
            &mut feed_rx,
            snapshot_with("\x1b[?1h$ "),
        );
        assert_eq!(hydrated, AttachStep::Refresh);
        assert_eq!(step(&keyboard, &attach), Some(Step::Flush(target)));
        let (_, entries) = keyboard.take().expect("armed");
        deliver(&mut tab, target.tab, entries);
        let sent: Vec<u8> = attach
            .test_drain_input()
            .into_iter()
            .flat_map(|msg| match msg {
                HostDataMsg::Input { bytes, .. } => bytes,
                HostDataMsg::Resize { .. } | HostDataMsg::Take | HostDataMsg::Detach => Vec::new(),
            })
            .collect();
        assert_eq!(
            sent, b"echo\x1bOA",
            "the retry carries the kept keys, the arrow in the hydrated cursor-key mode"
        );
    }

    /// EXIT renders the close; its ordinal is one past the last PTY seq.
    #[tokio::test]
    async fn exit_closes_the_tab() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        attach.on_frame(pty(42, b"last words"), &mut tab, &feed_tx);
        assert_eq!(
            attach.on_frame(
                HostTabFrame::Exit {
                    attempt: 1,
                    final_seq: 43,
                    code: 0,
                },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::Closed { code: 0 }
        );
    }

    /// A frame stamped with a dead attempt is dropped whole — the
    /// stale-message contract at the attempt level (the `HostId` level
    /// is the app map's miss).
    #[tokio::test]
    async fn a_stale_attempts_frame_is_dropped() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        attach.attempt = 2; // a re-attach superseded attempt 1
        assert_eq!(
            attach.on_frame(pty(42, b"from the dead"), &mut tab, &feed_tx),
            AttachStep::None
        );
        tab.refresh_snapshot().expect("refresh");
        assert!(
            !tab.dump(0)
                .expect("dump")
                .rows_text
                .join("\n")
                .contains("from the dead"),
            "a dead attempt's bytes never touch the terminal"
        );
    }

    /// Detach mid-hydration abandons the decoder (on this thread) and
    /// does NOT advertise the unfinished snapshot's fence: the rendered
    /// terminal never took it, so resuming from it would silently skip
    /// every byte the abandoned hydration absorbed.
    #[tokio::test]
    async fn detach_mid_hydration_does_not_claim_the_unfinished_fence() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        attach.on_frame(pty(101, b"progress"), &mut tab, &feed_tx);
        assert_eq!(
            attach.detach(Some(tab.grid())),
            None,
            "no fence was ever true of the rendered terminal"
        );
    }

    /// The same rule across a failed hydration mid-session: the OLD
    /// resume point (true of the still-rendered terminal) survives; the
    /// dead hydration's progress does not overwrite it.
    #[tokio::test]
    async fn an_aborted_hydration_keeps_the_old_resume_point() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        // Establish a live stream: fence 41, one applied frame → 43.
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        attach.on_frame(pty(42, b"live"), &mut tab, &feed_tx);
        // A snapshot re-attach begins (new fence far ahead) and dies
        // before FINISH.
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        attach.on_frame(pty(101, b"absorbed then lost"), &mut tab, &feed_tx);
        assert!(matches!(
            attach.on_frame(HostTabFrame::Closed { attempt: 1 }, &mut tab, &feed_tx),
            AttachStep::Reattach { .. }
        ));
        assert_eq!(
            attach.detach(Some(tab.grid())),
            Some(ResumePoint {
                server_epoch: 11,
                tab_generation: 2,
                next_seq: 43,
            }),
            "the old point still describes what is rendered"
        );
    }

    /// A `vt` payload is a bare byte stream with no marks of its own: it
    /// is not a screen until it is whole, and the zero-length SNAP
    /// terminator is the only thing that says it is.
    #[tokio::test]
    async fn a_vt_terminator_swaps_the_replayed_terminal_in() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        tab.write_vt(b"old-screen");
        attach.on_frame(accepted_as(PayloadKind::Vt, false, 100), &mut tab, &feed_tx);
        assert_eq!(
            attach.payload_kind(),
            Some(PayloadKind::Vt),
            "what host.status reports is what was accepted"
        );
        assert_eq!(
            attach.on_frame(snap(b"fresh-host-screen"), &mut tab, &feed_tx),
            AttachStep::None
        );
        assert!(
            screen(&mut tab).contains("old-screen"),
            "the old terminal renders through hydration — never half a payload"
        );

        assert_eq!(
            attach.on_frame(snap(b""), &mut tab, &feed_tx),
            AttachStep::Refresh
        );
        assert!(matches!(attach.phase, Phase::Live));
        let text = screen(&mut tab);
        assert!(text.contains("fresh-host-screen"), "{text:?}");
        assert!(!text.contains("old-screen"));
        assert_eq!(
            attach.detach(Some(tab.grid())),
            Some(ResumePoint {
                server_epoch: 11,
                tab_generation: 2,
                next_seq: 101,
            }),
            "the terminator promotes the fence, exactly as FINISH does"
        );
    }

    /// A payload is composed at the tab's grid, which is another
    /// client's when this attach did not take it (plan 084 D6): the
    /// terminal is built at that grid and **stays** there — the mirror is
    /// the PTY's grid, and the window draws it at whatever offset that
    /// takes. Replaying a `vt` payload into a terminal of another width
    /// would wrap its lines and misplace its absolute cursor moves.
    #[tokio::test]
    async fn a_vt_payload_hydrates_at_the_tabs_grid_and_stays_there() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(
            accepted_at(PayloadKind::Vt, false, 100, (100, 30), owner(DESK)),
            &mut tab,
            &feed_tx,
        );
        let Phase::Hydrating(hydration) = &attach.phase else {
            panic!("a fresh vt payload hydrates");
        };
        assert_eq!(
            hydration.built_at,
            (100, 30),
            "the terminal the payload replays into is the payload's size"
        );

        attach.on_frame(
            snap(b"\x1b[30;1Hbottom-of-the-servers-screen"),
            &mut tab,
            &feed_tx,
        );
        assert_eq!(
            attach.on_frame(snap(b""), &mut tab, &feed_tx),
            AttachStep::Refresh
        );
        assert_eq!(tab.grid(), (100, 30), "the mirror keeps the tab's grid");
        assert!(screen(&mut tab).contains("bottom-of-the-servers-screen"));
        assert!(
            attach.test_drain_input().is_empty(),
            "and asks the session for nothing"
        );
        assert!(attach.owned_by_another());
    }

    /// The other side of the same rule: an attach that took the tab gets
    /// a payload at its own grid, and hydrates there.
    #[tokio::test]
    async fn a_taking_attach_hydrates_at_its_own_grid() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted_as(PayloadKind::Vt, false, 100), &mut tab, &feed_tx);
        let Phase::Hydrating(hydration) = &attach.phase else {
            panic!("a fresh vt payload hydrates");
        };
        assert_eq!(hydration.built_at, (GEOMETRY.cols, GEOMETRY.rows));

        attach.on_frame(snap(b"taken"), &mut tab, &feed_tx);
        attach.on_frame(snap(b""), &mut tab, &feed_tx);
        tab.refresh_snapshot().expect("refresh");
        let dump = tab.dump(0).expect("dump");
        assert_eq!(dump.rows_text.len(), usize::from(GEOMETRY.rows));
        assert!(dump.rows_text.join("\n").contains("taken"));
        assert!(!attach.owned_by_another());
    }

    /// Input queued at an attach has a reader in every phase but the
    /// last: `Requesting` and `Hydrating` queue (the input queue outlives
    /// an attempt), `Live` writes, and only `Ended` — the entry about to
    /// be dropped — has nothing to drain it. The keyboard route and the
    /// paste gate both read this, so a tab that takes keys cannot refuse
    /// a paste.
    #[tokio::test]
    async fn input_has_a_reader_in_every_phase_but_ended() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        assert!(matches!(attach.phase, Phase::Requesting));
        assert!(attach.live(), "a dial in flight queues rather than drops");

        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        assert!(matches!(attach.phase, Phase::Hydrating(_)));
        assert!(attach.live(), "so does a hydration");

        attach.phase = Phase::Live;
        assert!(attach.live());

        attach.phase = Phase::Ended;
        assert!(
            !attach.live(),
            "nothing will drain this queue; the route must go elsewhere"
        );
    }

    /// #515: flagged, only a streaming tab reads its kept terminal — a
    /// dial or a hydration in flight does not. Unflagged (in-process, an
    /// ssh host, the slot down), every tab does.
    #[tokio::test]
    async fn a_flagged_dump_defers_to_the_session_unless_the_tab_is_streaming() {
        let defers = |attach: Option<&HostAttach>| {
            [true, false].map(|flagged| dump_defers_to_session(flagged, attach))
        };
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        assert_eq!(defers(None), [true, false], "detached");

        assert!(matches!(attach.phase, Phase::Requesting));
        assert_eq!(defers(Some(&attach)), [true, false], "requesting");

        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        assert!(matches!(attach.phase, Phase::Hydrating(_)));
        assert_eq!(defers(Some(&attach)), [true, false], "hydrating");

        attach.phase = Phase::Live;
        assert_eq!(defers(Some(&attach)), [false, false], "streaming");

        attach.phase = Phase::Ended;
        assert_eq!(defers(Some(&attach)), [true, false], "ended");
    }

    /// A `vt` hydration has no READY to interleave live bytes from, so
    /// PTY defers for the whole payload and replays in arrival order
    /// once the terminator lands.
    #[tokio::test]
    async fn a_vt_hydration_holds_pty_until_the_terminator() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted_as(PayloadKind::Vt, false, 100), &mut tab, &feed_tx);
        attach.on_frame(snap(b"base"), &mut tab, &feed_tx);
        assert_eq!(
            attach.on_frame(pty(101, b"-live"), &mut tab, &feed_tx),
            AttachStep::None,
            "there is no screen to apply it to yet"
        );
        attach.on_frame(snap(b""), &mut tab, &feed_tx);
        let text = screen(&mut tab);
        assert!(text.contains("base-live"), "{text:?}");
    }

    /// The decoder follows the accepted kind and nothing else: the same
    /// bytes are a screen under `vt` and are not a snapshot under
    /// `ghostty-snapshot`. A client that picked by its own preference
    /// instead would build a terminal out of a stream it never parsed.
    #[tokio::test]
    async fn the_accepted_kind_chooses_the_decoder() {
        let payload = b"plain-vt-bytes";

        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted_as(PayloadKind::Vt, false, 100), &mut tab, &feed_tx);
        attach.on_frame(snap(payload), &mut tab, &feed_tx);
        assert_eq!(
            attach.on_frame(snap(b""), &mut tab, &feed_tx),
            AttachStep::Refresh
        );
        assert!(screen(&mut tab).contains("plain-vt-bytes"));

        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        tab.write_vt(b"old-screen");
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        attach.on_frame(snap(payload), &mut tab, &feed_tx);
        attach.on_frame(snap(b""), &mut tab, &feed_tx);
        assert!(
            screen(&mut tab).contains("old-screen"),
            "the snapshot decoder never took those bytes for a screen"
        );
    }

    /// A measured cell the wire's `u16` cannot hold clamps; it never
    /// wraps, and it never becomes the `0` the session reads as "this
    /// client has no pixel metrics at all".
    ///
    /// Nothing upstream bounds the measurement: a configured font size
    /// is deliberately not capped at the interactive zoom limit, and the
    /// renderer accepts any finite cell of at least one pixel.
    #[test]
    fn a_cell_too_wide_for_the_wire_clamps_rather_than_wrapping() {
        assert_eq!(cell_px(0), 0, "the wire's own 'unknown' passes through");
        assert_eq!(cell_px(9), 9);
        assert_eq!(cell_px(u16::MAX as u32), u16::MAX);
        assert_eq!(cell_px(u16::MAX as u32 + 1), u16::MAX, "not 0");
        assert_eq!(cell_px(70_000), u16::MAX, "not 4464");
        assert_eq!(cell_px(u32::MAX), u16::MAX);
    }

    /// The accepted kind decides the decoder, and a kind this client
    /// never offered is refused rather than guessed at.
    #[test]
    fn an_accepted_kind_this_client_cannot_decode_is_refused() {
        let snapshot = AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT);
        let vt = AttachPayloadKind::from(AttachPayloadKind::VT);
        assert_eq!(negotiated_kind(&snapshot), Ok(PayloadKind::GhosttySnapshot));
        assert_eq!(negotiated_kind(&vt), Ok(PayloadKind::Vt));

        let unoffered = AttachPayloadKind::from("sixel-mosaic");
        let refused = negotiated_kind(&unoffered).unwrap_err();
        assert!(
            refused.contains("sixel-mosaic"),
            "the refusal names what it could not decode: {refused}"
        );
    }

    /// The client's half of the permit, with the test standing in for
    /// the connection task at both ends of it: it takes the barrier off
    /// the queue, and it holds the serving fence that decides whether a
    /// granted permit may still dial.
    ///
    /// What the *worker* does with a barrier — grants it in line, never
    /// on the wire, and serves through it — is pinned against the
    /// production drain loop in
    /// [`crate::host_conn::task`], not here.
    struct StandIn {
        _dir: tempfile::TempDir,
        socket: std::path::PathBuf,
        ops: HostOps,
        ops_rx: mpsc::Receiver<HostIntent>,
        dialed: tokio::sync::oneshot::Receiver<()>,
        data_plane: tokio::task::JoinHandle<()>,
    }

    impl StandIn {
        /// A data plane that accepts the dial, says so, and then says
        /// nothing at all — an attach parked mid-handshake.
        fn new() -> StandIn {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket = dir.path().join("roost.sock");
            let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
            let (dialed_tx, dialed) = tokio::sync::oneshot::channel();
            let data_plane = tokio::spawn(async move {
                let (held, _) = listener.accept().await.expect("accept");
                let _ = dialed_tx.send(());
                std::future::pending::<()>().await;
                drop(held);
            });
            let (ops, ops_rx) = HostOps::channel();
            StandIn {
                _dir: dir,
                socket,
                ops,
                ops_rx,
                dialed,
                data_plane,
            }
        }

        /// The barrier [`HostAttach::begin`] enqueued, checked for the
        /// two things the queue promises about it.
        async fn barrier(&mut self) -> HostIntent {
            let intent = tokio::time::timeout(Duration::from_secs(5), self.ops_rx.recv())
                .await
                .expect("the attach enqueued its permit")
                .expect("on an open queue");
            assert!(intent.barrier, "it is a barrier, not a wire op");
            assert_eq!(
                intent.fence,
                Some(key().host),
                "fenced at this tab's own incarnation"
            );
            intent
        }

        /// Nothing has been dialed, and stays undialed for long enough
        /// that a dial which ignored the permit would have been seen.
        async fn nothing_dialed(&mut self, why: &str) {
            assert!(
                tokio::time::timeout(Duration::from_millis(250), &mut self.dialed)
                    .await
                    .is_err(),
                "{why}"
            );
        }
    }

    /// **No socket is opened until the permit is granted** (plan 065
    /// §3.10). The attach's ordering device is its place in the host's
    /// op queue, and this is the client's whole share of it: `begin`
    /// dials nothing until the barrier it enqueued has been answered.
    ///
    /// Until #473 the ticket mint was a queued op and the order came for
    /// free. With the handshake inline there is no control leg left, so
    /// the position has to be taken deliberately — and a dial that ran
    /// ahead of it would compose a snapshot with the palette
    /// `session.set_theme` had not installed yet.
    #[tokio::test]
    async fn the_attach_dials_nothing_until_its_permit_is_granted() {
        let mut stand_in = StandIn::new();
        // The connection task's other half: this incarnation is the one
        // being served, so a permit granted for it may dial.
        let _serving = stand_in.ops.serving().open(key().host);

        let (feed_tx, _feed_rx) = engine_feed::channel();
        let mut attach = HostAttach::new(key(), GEOMETRY, AttachIntent::Explicit);
        attach.begin(
            &stand_in.ops,
            Some("sess-1"),
            stand_in.socket.clone(),
            "gb",
            "iced-test",
            &feed_tx,
        );

        let barrier = stand_in.barrier().await;
        stand_in
            .nothing_dialed("nothing is dialed while the permit is still in the queue")
            .await;

        barrier.answer(Ok(serde_json::Value::Null));
        tokio::time::timeout(Duration::from_secs(5), stand_in.dialed)
            .await
            .expect("the dial follows the grant")
            .expect("the data plane accepted");

        attach.abort_tasks();
        stand_in.data_plane.abort();
    }

    /// **A grant is not a licence that outlives its connection.** The
    /// permit is answered where the worker stands and everything after
    /// it happens on a socket of its own, so the connection it was
    /// granted for can end before the dial. It must not dial anyway: the
    /// session process is often still listening — only the control or
    /// event leg died — so its `session_id` would match, and a focused
    /// attach would resize the tab for a connection this client has
    /// already lost.
    #[tokio::test]
    async fn an_attach_whose_connection_ended_after_the_grant_never_dials() {
        let mut stand_in = StandIn::new();
        let serving = stand_in.ops.serving().open(key().host);

        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.begin(
            &stand_in.ops,
            Some("sess-1"),
            stand_in.socket.clone(),
            "gb",
            "iced-test",
            &feed_tx,
        );

        let barrier = stand_in.barrier().await;
        // Exactly the window F3 lives in: the connection ends, and the
        // barrier it already dequeued is no longer on the queue to be
        // flushed with the rest.
        drop(serving);
        barrier.answer(Ok(serde_json::Value::Null));

        stand_in
            .nothing_dialed("a permit granted for a connection that has ended opens nothing")
            .await;
        let refusal = next_permit_refusal(&mut feed_rx).await;
        assert!(matches!(
            refusal,
            HostTabFrame::PermitRefused {
                error: HostOpError::Disconnected,
                ..
            }
        ));
        assert_eq!(
            attach.on_frame(refusal, &mut tab, &feed_tx),
            AttachStep::Detach,
            "and the tab is told the host connection owns the recovery"
        );

        attach.abort_tasks();
        stand_in.data_plane.abort();
    }

    #[test]
    fn only_a_full_queue_makes_a_refused_permit_retryable() {
        for (error, expected) in [
            (
                HostOpError::QueueFull,
                FailReason::Retryable as fn(String) -> FailReason,
            ),
            (HostOpError::Disconnected, FailReason::HostGone),
            (HostOpError::WorkerGone, FailReason::HostGone),
        ] {
            assert_eq!(
                classify_permit_failure(&error),
                expected(error.to_string()),
                "{error:?}"
            );
        }
    }

    /// **A full queue is a worker behind, not a host gone** (#500). The
    /// refused attach backs off instead of detaching, and the retry that
    /// comes due after the queue drains takes its place in line and
    /// dials.
    #[tokio::test]
    async fn an_attach_refused_by_a_full_queue_attaches_once_it_drains() {
        let mut stand_in = StandIn::new();
        let _serving = stand_in.ops.serving().open(key().host);
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();

        fill(&stand_in.ops);
        attach.begin(
            &stand_in.ops,
            Some("sess-1"),
            stand_in.socket.clone(),
            "gb",
            "iced-test",
            &feed_tx,
        );
        let refusal = next_permit_refusal(&mut feed_rx).await;
        let AttachStep::Reattach { delay } = attach.on_frame(refusal, &mut tab, &feed_tx) else {
            panic!("a full queue is retried, not detached");
        };
        assert!(!delay.is_zero(), "the retry waits its rung out");

        while stand_in.ops_rx.try_recv().is_ok() {}
        attach.arm_reattach(delay, &feed_tx);
        let due = next_host_frame(&mut feed_rx, |frame| {
            matches!(frame, HostTabFrame::ReattachDue { .. })
        })
        .await;
        assert_eq!(
            attach.on_frame(due, &mut tab, &feed_tx),
            AttachStep::Reattach {
                delay: Duration::ZERO
            }
        );
        attach.begin(
            &stand_in.ops,
            Some("sess-1"),
            stand_in.socket.clone(),
            "gb",
            "iced-test",
            &feed_tx,
        );

        stand_in.barrier().await.answer(Ok(serde_json::Value::Null));
        tokio::time::timeout(Duration::from_secs(5), stand_in.dialed)
            .await
            .expect("the retry dials once its permit is granted")
            .expect("the data plane accepted");

        attach.abort_tasks();
        stand_in.data_plane.abort();
    }

    /// **A queue that never drains ends the attach** once the ladder has
    /// been climbed, and every retry before that waits its rung out.
    #[tokio::test]
    async fn an_attach_refused_by_a_queue_that_never_drains_detaches_at_the_top_of_the_ladder() {
        let (ops, _stalled_worker) = HostOps::channel();
        fill(&ops);
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();

        let mut waits = Vec::new();
        let last = loop {
            attach.begin(
                &ops,
                Some("sess-1"),
                std::path::PathBuf::from("/nonexistent/roost-full-queue.sock"),
                "gb",
                "iced-test",
                &feed_tx,
            );
            let refusal = next_permit_refusal(&mut feed_rx).await;
            match attach.on_frame(refusal, &mut tab, &feed_tx) {
                AttachStep::Reattach { delay } if waits.len() <= FULL_QUEUE_RETRIES as usize => {
                    waits.push(delay);
                }
                step => break step,
            }
        };

        assert_eq!(last, AttachStep::Detach, "after {waits:?}");
        assert_eq!(waits.len(), FULL_QUEUE_RETRIES as usize, "{waits:?}");
        assert!(waits.iter().all(|delay| !delay.is_zero()), "{waits:?}");
        assert_eq!(
            &waits[waits.len() - 2..],
            &[BACKOFF_BASE * (1 << (BACKOFF_CAP_STEP - 1)), BACKOFF_CAP],
            "one retry on the capped rung, and it is the last"
        );
        assert!(!attach.live());
    }

    /// Reaching Live restarts the count with the rest of the ladder: a
    /// queue that was full before the stream came up does not shorten
    /// the retries of the next stall.
    #[tokio::test]
    async fn reaching_live_restarts_the_full_queue_count() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        let full = || HostTabFrame::PermitRefused {
            attempt: 1,
            error: HostOpError::QueueFull,
        };
        for _ in 0..FULL_QUEUE_RETRIES {
            assert!(matches!(
                attach.on_frame(full(), &mut tab, &feed_tx),
                AttachStep::Reattach { .. }
            ));
        }
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        assert!(
            matches!(
                attach.on_frame(full(), &mut tab, &feed_tx),
                AttachStep::Reattach { .. }
            ),
            "the count restarted when the stream went Live"
        );
    }

    /// A gone worker is not a queue that might drain: the attach detaches
    /// on the first refusal.
    #[tokio::test]
    async fn an_attach_refused_by_a_gone_worker_detaches_at_once() {
        let (ops, worker) = HostOps::channel();
        drop(worker);
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();

        attach.begin(
            &ops,
            Some("sess-1"),
            std::path::PathBuf::from("/nonexistent/roost-gone-worker.sock"),
            "gb",
            "iced-test",
            &feed_tx,
        );
        let refusal = next_permit_refusal(&mut feed_rx).await;
        assert!(matches!(
            refusal,
            HostTabFrame::PermitRefused {
                error: HostOpError::WorkerGone,
                ..
            }
        ));
        assert_eq!(
            attach.on_frame(refusal, &mut tab, &feed_tx),
            AttachStep::Detach
        );
    }

    /// A queue whose worker has stopped draining it.
    fn fill(ops: &HostOps) {
        while ops
            .send(HostIntent::new("fill", serde_json::json!({})))
            .is_ok()
        {}
    }

    async fn next_permit_refusal(feed_rx: &mut EngineFeedReceiver) -> HostTabFrame {
        next_host_frame(feed_rx, |frame| {
            matches!(frame, HostTabFrame::PermitRefused { .. })
        })
        .await
    }

    /// The first frame on the feed that `wanted` picks, skipping the rest.
    async fn next_host_frame(
        feed_rx: &mut EngineFeedReceiver,
        wanted: impl Fn(&HostTabFrame) -> bool,
    ) -> HostTabFrame {
        let mut batch = crate::engine_feed::EngineBatch::default();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                while let Some(item) = feed_rx.try_next(&mut batch) {
                    if let EngineFeed::HostTab(_, frame) = item {
                        if wanted(&frame) {
                            return frame;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the attempt put the frame on the feed")
    }

    /// A resize that runs out of patience during a `vt` payload is sent
    /// rather than re-attached for: the payload keeps its grid, and a
    /// `GRID` the session sends back is what moves it — after the
    /// terminator, where the session holds it.
    #[tokio::test]
    async fn a_withheld_resize_during_a_vt_payload_is_sent_at_the_deadline() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted_as(PayloadKind::Vt, false, 100), &mut tab, &feed_tx);
        attach.on_frame(snap(b"half a payload"), &mut tab, &feed_tx);
        attach.note_resize(
            Geometry {
                cols: 132,
                rows: 50,
                ..GEOMETRY
            },
            true,
        );
        assert_eq!(
            attach.on_frame(
                HostTabFrame::WithholdDeadline { attempt: 1 },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::None
        );
        assert!(matches!(
            attach.test_drain_input().as_slice(),
            [HostDataMsg::Resize { cols: 132, .. }]
        ));
        assert!(matches!(attach.phase, Phase::Hydrating(_)));
    }

    /// The deadline bounds the *hold*, not the accept, so a resize that
    /// arrives after one has already fired is still sent within the next.
    /// A `vt` hydration is where this bites: it holds for the whole
    /// payload, so most of its life is after that first deadline.
    #[tokio::test(start_paused = true)]
    async fn a_resize_after_the_deadline_is_still_bounded_by_it() {
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(accepted_as(PayloadKind::Vt, false, 100), &mut tab, &feed_tx);
        attach.on_frame(snap(b"half a payload"), &mut tab, &feed_tx);

        // The deadline armed at the accept, with nothing on hold.
        let deadline = next_withhold_deadline(&mut feed_rx).await;
        assert_eq!(
            attach.on_frame(deadline, &mut tab, &feed_tx),
            AttachStep::None
        );
        assert!(attach.test_drain_input().is_empty());

        // Only now does the user drag the window, mid-payload.
        attach.note_resize(
            Geometry {
                cols: 132,
                rows: 50,
                ..GEOMETRY
            },
            true,
        );
        let deadline = next_withhold_deadline(&mut feed_rx).await;
        attach.on_frame(deadline, &mut tab, &feed_tx);
        assert!(
            matches!(
                attach.test_drain_input().as_slice(),
                [HostDataMsg::Resize { cols: 132, .. }]
            ),
            "the re-armed deadline sent it"
        );
    }

    /// Wait out the withhold deadline and take the frame its timer put
    /// on the feed. Callers pause the clock, so this costs no real time.
    async fn next_withhold_deadline(feed_rx: &mut EngineFeedReceiver) -> HostTabFrame {
        tokio::time::sleep(WITHHOLD_DEADLINE + Duration::from_millis(1)).await;
        let mut batch = crate::engine_feed::EngineBatch::default();
        loop {
            let item = feed_rx
                .try_next(&mut batch)
                .expect("a withhold deadline on the feed");
            if let EngineFeed::HostTab(_, frame @ HostTabFrame::WithholdDeadline { .. }) = item {
                return frame;
            }
        }
    }

    /// A detaching attempt's connection ends without a word: the attach
    /// that replaced it may be on the same attempt number, and a `Closed`
    /// from this one would abort that.
    #[tokio::test]
    async fn a_detaching_connection_ends_without_a_word() {
        use tokio::io::AsyncWriteExt;

        let (client, server) = tokio::net::UnixStream::pair().expect("socket pair");
        let (read, write) = client.into_split();
        let (server_read, mut server_write) = server.into_split();
        let reply = roost_ipc::messages::AttachHandshakeReply::Accepted(
            roost_ipc::messages::AttachAccepted {
                kind: AttachPayloadKind::from("ghostty-snapshot"),
                mode: roost_ipc::messages::AttachMode::Snapshot,
                seq: 0,
                server_epoch: 1,
                tab_generation: 1,
                snapshot_cols: GEOMETRY.cols,
                snapshot_rows: GEOMETRY.rows,
                owner: None,
            },
        );
        let body = serde_json::to_vec(&reply).expect("encode the reply");
        roost_ipc::framing::write_frame(&mut server_write, &body)
            .await
            .expect("reply");
        server_write
            .write_all(&roost_ipc::dataframe::PREAMBLE)
            .await
            .expect("preamble");
        let handshake = choose_handshake(key().tab, terms(), None);
        let (_, conn) = DataConnection::handshake(read, write, &handshake)
            .await
            .expect("accepted");
        drop((server_read, server_write));

        let (feed_tx, mut feed_rx) = engine_feed::channel();
        let (_input_tx, mut input_rx) = mpsc::unbounded_channel();
        let detaching = watch::Sender::new(true).subscribe();
        tokio::time::timeout(
            Duration::from_secs(5),
            stream_attempt(key(), 1, conn, &mut input_rx, &detaching, &feed_tx),
        )
        .await
        .expect("the attempt ends with its connection");
        let mut batch = crate::engine_feed::EngineBatch::default();
        assert!(
            feed_rx.try_next(&mut batch).is_none(),
            "a detaching attempt publishes nothing"
        );
    }

    /// The `take` each handshake sent, as the trace has them.
    fn takes_sent(attach: &HostAttach) -> Vec<bool> {
        trace_of(attach)
            .into_iter()
            .filter_map(|action| match action {
                SizeTraceAction::AttachSent { take } => Some(take),
                _ => None,
            })
            .collect()
    }

    /// Plan 084 D6: an explicit switch's attach takes the tab's size on
    /// every attempt up to and including the first the session accepts
    /// — a refused dial is no switch done — and never after it: every
    /// later re-attach is the window's own.
    #[tokio::test]
    async fn an_explicit_attach_takes_until_its_first_accepted_handshake() {
        let (ops, _worker) = HostOps::channel();
        let nowhere = std::path::PathBuf::from("/nonexistent/roost-intent.sock");
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.begin(&ops, Some("sess-1"), nowhere.clone(), "gb", ME, &feed_tx);
        assert!(matches!(
            attach.on_frame(
                HostTabFrame::Failed {
                    attempt: attach.attempt,
                    reason: FailReason::Retryable("dial refused".into()),
                },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::Reattach { .. }
        ));
        attach.begin(&ops, Some("sess-1"), nowhere.clone(), "gb", ME, &feed_tx);
        let second = attach.attempt;
        attach.on_frame(
            HostTabFrame::Accepted {
                attempt: second,
                resumed: true,
                kind: PayloadKind::GhosttySnapshot,
                fence: 41,
                server_epoch: 11,
                tab_generation: 2,
                grid: GEOMETRY.grid(),
                owner: owner(ME),
            },
            &mut tab,
            &feed_tx,
        );
        attach.on_frame(HostTabFrame::Closed { attempt: second }, &mut tab, &feed_tx);
        attach.begin(&ops, Some("sess-1"), nowhere, "gb", ME, &feed_tx);
        assert_eq!(takes_sent(&attach), vec![true, true, false]);
        attach.abort_tasks();
    }

    /// An attach the window made on its own never asks to take: it gets
    /// the tab only if nobody sizes it.
    #[tokio::test]
    async fn an_automatic_attach_never_asks_to_take() {
        let (ops, _worker) = HostOps::channel();
        let nowhere = std::path::PathBuf::from("/nonexistent/roost-intent.sock");
        let (feed_tx, _feed_rx) = engine_feed::channel();
        let mut attach = HostAttach::new(key(), GEOMETRY, AttachIntent::Automatic)
            .with_trace(Some(SizeTrace::default()));
        attach.begin(&ops, Some("sess-1"), nowhere.clone(), "gb", ME, &feed_tx);
        attach.begin(&ops, Some("sess-1"), nowhere, "gb", ME, &feed_tx);
        assert_eq!(takes_sent(&attach), vec![false, false]);
        attach.abort_tasks();
    }

    /// A `GRID` applies at its place in the stream — after PTY frame
    /// `seq` — and resizes the mirror, never the window: no RESIZE goes
    /// back (no echo) and the window's own grid stays what it was.
    #[tokio::test]
    async fn a_live_grid_resizes_the_mirror_at_its_place_and_sends_nothing_back() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        attach.on_frame(pty(42, b"at 80 columns"), &mut tab, &feed_tx);
        assert_eq!(
            attach.on_frame(grid(42, (120, 30), owner(DESK)), &mut tab, &feed_tx),
            AttachStep::Refresh
        );
        assert_eq!(tab.grid(), (120, 30));
        assert_eq!(attach.advisory, GEOMETRY, "the window's grid is untouched");
        assert!(attach.test_drain_input().is_empty(), "no echo");
        assert_eq!(
            trace_of(&attach),
            vec![SizeTraceAction::GridApplied {
                cols: 120,
                rows: 30
            }]
        );
        assert_eq!(
            attach.on_frame(pty(43, b"after"), &mut tab, &feed_tx),
            AttachStep::Refresh,
            "and the next PTY frame follows it"
        );
    }

    /// A `GRID` names the PTY frame it follows; one that names any other
    /// is a stream this client cannot place, which re-attach rebuilds.
    #[tokio::test]
    async fn a_grid_out_of_its_place_in_the_stream_reattaches() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        assert!(matches!(
            attach.on_frame(grid(43, (120, 30), None), &mut tab, &feed_tx),
            AttachStep::Reattach { .. }
        ));
        assert_eq!(tab.grid(), GEOMETRY.grid());
    }

    /// The size owner is what the accepted handshake and every `GRID`
    /// after it say, and nothing else.
    #[tokio::test]
    async fn the_owner_is_what_the_accept_and_each_grid_say() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(
            accepted_at(
                PayloadKind::GhosttySnapshot,
                true,
                41,
                GEOMETRY.grid(),
                owner(DESK),
            ),
            &mut tab,
            &feed_tx,
        );
        assert!(attach.owned_by_another());
        attach.on_frame(grid(41, GEOMETRY.grid(), owner(ME)), &mut tab, &feed_tx);
        let mine = attach.owner().expect("an owner").clone();
        assert!(attach.is_me(&mine));
        assert!(!attach.owned_by_another());
        attach.on_frame(grid(41, GEOMETRY.grid(), None), &mut tab, &feed_tx);
        assert_eq!(attach.owner(), None);
        assert!(!attach.owned_by_another(), "nobody is not another client");
    }

    /// Past READY a `GRID` resizes the payload's decoder at its place
    /// (forfeiting the history pages still to come), and FINISH installs
    /// the terminal at that grid, never back at the one the payload
    /// started at.
    #[tokio::test]
    async fn a_grid_during_hydration_applies_to_the_payload_and_finish_keeps_it() {
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        let encoded = snapshot_with("mid-hydration");
        let boundary = roost_vt::ready_boundary(&encoded).expect("READY boundary");
        attach.on_frame(snap(&encoded[..boundary]), &mut tab, &feed_tx);
        attach.on_frame(grid(100, (100, 30), owner(DESK)), &mut tab, &feed_tx);
        let Phase::Hydrating(hydration) = &attach.phase else {
            panic!("still hydrating");
        };
        assert_eq!(hydration.built_at, (100, 30));
        assert_eq!(
            tab.grid(),
            GEOMETRY.grid(),
            "the rendered terminal waits for FINISH"
        );
        hydrate_fully(
            &mut attach,
            &mut tab,
            &feed_tx,
            &mut feed_rx,
            encoded[boundary..].to_vec(),
        );
        assert!(matches!(attach.phase, Phase::Live));
        assert_eq!(tab.grid(), (100, 30), "FINISH kept the GRID's grid");
        assert!(attach.test_drain_input().is_empty());
    }

    /// Short of READY a `GRID` has no screen to apply to, so it waits
    /// with the PTY frames, in their order.
    #[tokio::test]
    async fn a_grid_before_ready_waits_its_turn_with_the_pty_frames() {
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        attach.on_frame(pty(101, b"before"), &mut tab, &feed_tx);
        attach.on_frame(grid(101, (100, 30), owner(DESK)), &mut tab, &feed_tx);
        attach.on_frame(pty(102, b"-after"), &mut tab, &feed_tx);
        assert!(trace_of(&attach).is_empty(), "nothing applied yet");
        hydrate_fully(
            &mut attach,
            &mut tab,
            &feed_tx,
            &mut feed_rx,
            snapshot_with("base "),
        );
        assert_eq!(tab.grid(), (100, 30));
        assert!(screen(&mut tab).contains("before-after"));
        assert_eq!(
            trace_of(&attach),
            vec![SizeTraceAction::GridApplied {
                cols: 100,
                rows: 30
            }]
        );
    }

    /// The same inside a `vt` payload, which is a screen only once its
    /// terminator lands: the `GRID` applies there, in order.
    #[tokio::test]
    async fn a_grid_inside_a_vt_payload_applies_at_the_terminator() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted_as(PayloadKind::Vt, false, 100), &mut tab, &feed_tx);
        attach.on_frame(snap(b"payload"), &mut tab, &feed_tx);
        attach.on_frame(grid(100, (100, 30), owner(DESK)), &mut tab, &feed_tx);
        assert_eq!(
            attach.on_frame(snap(b""), &mut tab, &feed_tx),
            AttachStep::Refresh
        );
        assert_eq!(tab.grid(), (100, 30));
        assert!(screen(&mut tab).contains("payload"));
    }

    /// Plan 084 D6: a copy off another client's tab takes the size
    /// **after** the text is captured, so the copy holds the text as it
    /// was laid out when the user chose it; a copy off this window's own
    /// tab takes nothing.
    #[tokio::test]
    async fn a_copy_takes_another_clients_tab_after_the_capture() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(
            accepted_at(
                PayloadKind::GhosttySnapshot,
                true,
                41,
                GEOMETRY.grid(),
                owner(DESK),
            ),
            &mut tab,
            &feed_tx,
        );
        attach.copied();
        assert_eq!(
            trace_of(&attach),
            vec![SizeTraceAction::CopyCaptured, SizeTraceAction::TakeSent]
        );
        assert_eq!(attach.test_drain_input(), vec![HostDataMsg::Take]);

        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        attach.copied();
        assert_eq!(trace_of(&attach), vec![SizeTraceAction::CopyCaptured]);
        assert!(attach.test_drain_input().is_empty());
    }

    /// The settle, with a paused clock: scrolls every 100 ms keep pushing
    /// it out, so only the last one's timer survives, and it takes the
    /// tab 300 ms after the scrolling stopped.
    #[tokio::test(start_paused = true)]
    async fn a_local_scroll_takes_another_clients_tab_once_it_has_settled() {
        let settle = Duration::from_millis(300);
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(
            accepted_at(
                PayloadKind::GhosttySnapshot,
                true,
                41,
                GEOMETRY.grid(),
                owner(DESK),
            ),
            &mut tab,
            &feed_tx,
        );
        for _ in 0..10 {
            attach.scrolled(settle, &feed_tx);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let settled = drain_settled(&mut feed_rx);
        assert!(settled.is_empty(), "nothing settled while scrolling");
        tokio::time::sleep(settle).await;
        let settled = drain_settled(&mut feed_rx);
        assert_eq!(settled.len(), 1, "one settle, the last scroll's");
        for frame in settled {
            attach.on_frame(frame, &mut tab, &feed_tx);
        }
        assert_eq!(trace_of(&attach), vec![SizeTraceAction::TakeSent]);
        assert_eq!(attach.test_drain_input(), vec![HostDataMsg::Take]);
    }

    /// A settle that comes due on a tab this window has taken meanwhile
    /// — or that it owned all along — takes nothing.
    #[tokio::test(start_paused = true)]
    async fn a_settled_scroll_takes_nothing_from_this_window_itself() {
        let settle = Duration::from_millis(300);
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        attach.scrolled(settle, &feed_tx);
        tokio::time::sleep(settle * 2).await;
        assert!(
            drain_settled(&mut feed_rx).is_empty(),
            "its own tab arms nothing"
        );

        attach.on_frame(grid(41, GEOMETRY.grid(), owner(DESK)), &mut tab, &feed_tx);
        attach.scrolled(settle, &feed_tx);
        attach.on_frame(grid(41, GEOMETRY.grid(), owner(ME)), &mut tab, &feed_tx);
        tokio::time::sleep(settle * 2).await;
        for frame in drain_settled(&mut feed_rx) {
            attach.on_frame(frame, &mut tab, &feed_tx);
        }
        assert!(attach.test_drain_input().is_empty(), "taken meanwhile");
    }

    fn drain_settled(feed_rx: &mut EngineFeedReceiver) -> Vec<HostTabFrame> {
        let mut batch = crate::engine_feed::EngineBatch::default();
        let mut settled = Vec::new();
        while let Some(item) = feed_rx.try_next(&mut batch) {
            if let EngineFeed::HostTab(_, frame @ HostTabFrame::ScrollSettled { .. }) = item {
                settled.push(frame);
            }
        }
        settled
    }

    /// Plan 084 D6: a detach while the handshake's reply is still on its
    /// way puts `DETACH` right behind the handshake line — what cancels
    /// an attach the session has not committed, and deliberately ends
    /// one it has. A connection that merely closed would hold the tab's
    /// size for a grace period.
    #[tokio::test]
    async fn a_detach_while_the_reply_is_on_its_way_follows_the_handshake_with_detach() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("roost.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let (ops, mut ops_rx) = HostOps::channel();
        let _serving = ops.serving().open(key().host);
        let (feed_tx, _feed_rx) = engine_feed::channel();
        let mut attach = HostAttach::new(key(), GEOMETRY, AttachIntent::Explicit)
            .with_trace(Some(SizeTrace::default()));
        attach.begin(&ops, Some("sess-1"), socket, "gb", ME, &feed_tx);
        tokio::time::timeout(Duration::from_secs(5), ops_rx.recv())
            .await
            .expect("the permit was asked for")
            .expect("on an open queue")
            .answer(Ok(serde_json::Value::Null));
        let (conn, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("the dial")
            .expect("accept");
        let (read, _write) = conn.into_split();
        let mut lines = roost_ipc::framing::FrameReader::new(read);
        let line = lines
            .read_line()
            .await
            .expect("read")
            .expect("the handshake");
        let handshake: serde_json::Value = serde_json::from_slice(&line).expect("json");
        assert_eq!(handshake["take"], true);

        let trace = attach.trace.clone().expect("traced");
        assert_eq!(attach.detach(None), None);
        let (read, residue) = lines.into_parts();
        let mut frames = roost_ipc::dataframe::DataFrameReader::new(read, residue);
        let frame = tokio::time::timeout(Duration::from_secs(5), frames.next_frame())
            .await
            .expect("DETACH within the drain")
            .expect("read")
            .expect("a frame, not EOF");
        assert_eq!(frame.frame_type, roost_ipc::dataframe::FRAME_DETACH);
        assert_eq!(
            trace.lock().expect("trace").last(),
            Some(&SizeTraceAction::DetachSent)
        );
    }

    /// A live detach queues `DETACH` behind whatever input is still
    /// waiting, for the data task's drain to write.
    #[tokio::test]
    async fn a_live_detach_queues_detach_behind_input() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        tab.session
            .send_input(b"last".to_vec(), roost_ipc::dataframe::InputTake::Before);
        let queue = Arc::clone(&attach.input_rx);
        attach.detach(Some(tab.grid()));
        let mut rx = queue.try_lock().expect("no writer task in tests");
        assert!(matches!(rx.try_recv(), Ok(HostDataMsg::Input { .. })));
        assert_eq!(rx.try_recv(), Ok(HostDataMsg::Detach));
    }

    /// The intent is spent by the accept itself, before anything else
    /// about that attempt is judged: an accepted resume this client then
    /// refuses (it is at a grid the mirror is not at) was still committed
    /// by the session, and the fresh attach behind it must not take.
    #[tokio::test]
    async fn an_accepted_resume_that_is_refused_still_spends_the_intent() {
        let (ops, _worker) = HostOps::channel();
        let nowhere = std::path::PathBuf::from("/nonexistent/roost-intent.sock");
        let (attach, mut tab, feed_tx, _feed_rx) = rig();
        let mut attach = attach.with_resume(Some(POINT));
        attach.on_frame(
            accepted_at(
                PayloadKind::GhosttySnapshot,
                true,
                41,
                WIDER.grid(),
                owner(DESK),
            ),
            &mut tab,
            &feed_tx,
        );
        attach.begin(&ops, Some("sess-1"), nowhere, "gb", ME, &feed_tx);
        assert_eq!(takes_sent(&attach), vec![false]);
        attach.abort_tasks();
    }

    /// The session committed the attach when it wrote the accepted line;
    /// a connection that then drops before the preamble behind it must
    /// not leave the explicit intent to be sent again. The accept reaches
    /// the tab before the drop does, and the re-attach asks for nothing.
    #[tokio::test]
    async fn a_drop_between_the_accept_and_the_preamble_spends_the_intent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("roost.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.expect("accept");
            let (read, mut write) = conn.into_split();
            let mut lines = roost_ipc::framing::FrameReader::new(read);
            lines
                .read_line()
                .await
                .expect("read")
                .expect("the handshake");
            let reply = roost_ipc::messages::AttachHandshakeReply::Accepted(
                roost_ipc::messages::AttachAccepted {
                    kind: AttachPayloadKind::from("ghostty-snapshot"),
                    mode: roost_ipc::messages::AttachMode::Snapshot,
                    seq: 0,
                    server_epoch: 1,
                    tab_generation: 1,
                    snapshot_cols: GEOMETRY.cols,
                    snapshot_rows: GEOMETRY.rows,
                    owner: owner(ME),
                },
            );
            let body = serde_json::to_vec(&reply).expect("encode");
            roost_ipc::framing::write_frame(&mut write, &body)
                .await
                .expect("reply");
        });
        let (ops, mut ops_rx) = HostOps::channel();
        let _serving = ops.serving().open(key().host);
        let (feed_tx, mut feed_rx) = engine_feed::channel();
        let (mut attach, mut tab, _, _) = rig();
        attach.begin(&ops, Some("sess-1"), socket, "gb", ME, &feed_tx);
        tokio::time::timeout(Duration::from_secs(5), ops_rx.recv())
            .await
            .expect("the permit was asked for")
            .expect("on an open queue")
            .answer(Ok(serde_json::Value::Null));
        server.await.expect("the server answered and hung up");

        let mut seen = Vec::new();
        while !seen
            .iter()
            .any(|f| matches!(f, HostTabFrame::Failed { .. } | HostTabFrame::Closed { .. }))
        {
            seen.push(next_host_frame(&mut feed_rx, |_| true).await);
        }
        assert!(
            matches!(seen[0], HostTabFrame::Accepted { .. }),
            "the accept went out before the preamble it never got"
        );
        for frame in seen {
            attach.on_frame(frame, &mut tab, &feed_tx);
        }
        let (ops, _worker) = HostOps::channel();
        let nowhere = std::path::PathBuf::from("/nonexistent/roost-intent.sock");
        attach.begin(&ops, Some("sess-1"), nowhere, "gb", ME, &feed_tx);
        assert_eq!(takes_sent(&attach), vec![true, false]);
        attach.abort_tasks();
    }

    /// An explicit switch to a tab the window is already attaching on its
    /// own, before any handshake of that attach was accepted, makes it
    /// take; once one has been accepted, re-selecting changes nothing.
    #[tokio::test]
    async fn an_explicit_switch_upgrades_an_automatic_attach_not_yet_accepted() {
        let (ops, _worker) = HostOps::channel();
        let nowhere = std::path::PathBuf::from("/nonexistent/roost-intent.sock");
        let (feed_tx, _feed_rx) = engine_feed::channel();
        let mut attach = HostAttach::new(key(), GEOMETRY, AttachIntent::Automatic)
            .with_trace(Some(SizeTrace::default()));
        attach.begin(&ops, Some("sess-1"), nowhere.clone(), "gb", ME, &feed_tx);
        assert!(
            attach.make_explicit(),
            "not accepted yet: it becomes the switch"
        );
        assert!(!attach.make_explicit(), "already explicit");
        attach.begin(&ops, Some("sess-1"), nowhere, "gb", ME, &feed_tx);
        assert_eq!(takes_sent(&attach), vec![false, true]);
        attach.abort_tasks();

        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.take_pending = false;
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        assert!(
            !attach.make_explicit(),
            "an established attachment is left alone"
        );
    }

    /// A settle whose frame is already on the feed when the attempt it was
    /// armed in ends takes nothing on the next attempt's behalf.
    #[tokio::test(start_paused = true)]
    async fn a_settle_queued_before_a_close_takes_nothing() {
        let settle = Duration::from_millis(300);
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(
            accepted_at(
                PayloadKind::GhosttySnapshot,
                true,
                41,
                GEOMETRY.grid(),
                owner(DESK),
            ),
            &mut tab,
            &feed_tx,
        );
        attach.scrolled(settle, &feed_tx);
        tokio::time::sleep(settle * 2).await;
        let settled = drain_settled(&mut feed_rx);
        assert_eq!(settled.len(), 1, "the settle is on the feed");
        assert!(matches!(
            attach.on_frame(HostTabFrame::Closed { attempt: 1 }, &mut tab, &feed_tx),
            AttachStep::Reattach { .. }
        ));
        for frame in settled {
            attach.on_frame(frame, &mut tab, &feed_tx);
        }
        assert!(attach.test_drain_input().is_empty(), "no TAKE");
        assert_eq!(trace_of(&attach), vec![]);
    }

    /// Two attaches of one tab never share an attempt number, so what a
    /// draining connection of the first still delivers cannot land on the
    /// second.
    #[tokio::test]
    async fn attempt_numbers_are_unique_across_attaches() {
        let (ops, _worker) = HostOps::channel();
        let nowhere = std::path::PathBuf::from("/nonexistent/roost-attempt.sock");
        let (feed_tx, _feed_rx) = engine_feed::channel();
        let mut first = HostAttach::new(key(), GEOMETRY, AttachIntent::Explicit);
        let mut second = HostAttach::new(key(), GEOMETRY, AttachIntent::Explicit);
        first.begin(&ops, Some("sess-1"), nowhere.clone(), "gb", ME, &feed_tx);
        second.begin(&ops, Some("sess-1"), nowhere, "gb", ME, &feed_tx);
        assert_ne!(first.attempt, second.attempt);
        first.abort_tasks();
        second.abort_tasks();
    }

    /// A data task whose attach is detaching publishes nothing, whatever
    /// it hears: a refused permit included.
    #[tokio::test]
    async fn a_detaching_attempt_publishes_nothing() {
        let (ops, worker) = HostOps::channel();
        drop(worker);
        let (feed_tx, mut feed_rx) = engine_feed::channel();
        let (input_tx, input_rx) = mpsc::unbounded_channel::<HostDataMsg>();
        let detaching = watch::Sender::new(true);
        run_attempt(
            key(),
            1,
            ops.attach_permit(key().host),
            Dial {
                socket: std::path::PathBuf::from("/nonexistent/roost-detaching.sock"),
                handshake: choose_handshake(key().tab, terms(), None),
            },
            Arc::new(tokio::sync::Mutex::new(input_rx)),
            detaching.subscribe(),
            feed_tx,
        )
        .await;
        drop(input_tx);
        let mut batch = crate::engine_feed::EngineBatch::default();
        assert!(feed_rx.try_next(&mut batch).is_none(), "nothing published");
    }

    /// A focused resize of a tab this window does not size is a take even
    /// at the grid it already advertised. Withheld, latest-wins keeps it:
    /// focused to 90 columns and back to 80 still sends a focused 80.
    /// Unfocused, or of a tab it sizes, the same grid sends nothing.
    #[tokio::test]
    async fn a_focused_resize_at_the_advertised_grid_still_takes_another_clients_tab() {
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(
            accepted_at(
                PayloadKind::GhosttySnapshot,
                false,
                100,
                GEOMETRY.grid(),
                owner(DESK),
            ),
            &mut tab,
            &feed_tx,
        );
        attach.note_resize(
            Geometry {
                cols: 90,
                ..GEOMETRY
            },
            true,
        );
        attach.note_resize(GEOMETRY, true);
        hydrate_fully(
            &mut attach,
            &mut tab,
            &feed_tx,
            &mut feed_rx,
            snapshot_with("x"),
        );
        assert!(matches!(
            attach.test_drain_input().as_slice(),
            [HostDataMsg::Resize {
                cols: 80,
                focused: true,
                ..
            }]
        ));

        attach.note_resize(GEOMETRY, false);
        assert!(
            attach.test_drain_input().is_empty(),
            "unfocused: nothing new"
        );
        attach.note_resize(GEOMETRY, true);
        assert!(matches!(
            attach.test_drain_input().as_slice(),
            [HostDataMsg::Resize {
                cols: 80,
                focused: true,
                ..
            }]
        ));

        attach.on_frame(grid(100, GEOMETRY.grid(), owner(ME)), &mut tab, &feed_tx);
        attach.note_resize(GEOMETRY, true);
        assert!(
            attach.test_drain_input().is_empty(),
            "its own tab: nothing new"
        );
    }

    /// The window's teardown waits out the detach drains from outside the
    /// runtime, which is where a timer must not be built.
    #[test]
    fn waiting_out_the_detach_drains_from_outside_the_runtime_does_not_panic() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let started = std::time::Instant::now();
        wait_out_detach_drains(&runtime);
        assert!(started.elapsed() >= DETACH_DRAIN);
    }

    /// FINISH promotes the hydration's fence — the moment it becomes
    /// true of the rendered terminal.
    #[tokio::test]
    async fn finish_promotes_the_hydrations_fence() {
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        attach.on_frame(pty(101, b"early"), &mut tab, &feed_tx);
        hydrate_fully(
            &mut attach,
            &mut tab,
            &feed_tx,
            &mut feed_rx,
            snapshot_with("done"),
        );
        assert_eq!(
            attach.detach(Some(tab.grid())),
            Some(ResumePoint {
                server_epoch: 11,
                tab_generation: 2,
                next_seq: 102,
            })
        );
    }
}
