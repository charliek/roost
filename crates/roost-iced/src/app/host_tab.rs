//! The focused host tab's attach: token mint → data dial → hydration →
//! live, with resume across refocus (host-sessions plan 037 §3.4).
//!
//! Ownership is split the way the threading rules demand (CLAUDE.md):
//! background tasks only move bytes — the dial task mints the token and
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

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use roost_ipc::client::{ClientError, DataConnection, ServerCode, ServerFrame};
use roost_ipc::messages::{ops, AttachHandshake, AttachPayloadKind, TabAttachResult};
use roost_ui_model::keys::TabKey;
use roost_vt::{HistoryStep, ReadyState, SnapshotDecodeOptions, SnapshotDecoder, Terminal};
use tokio::sync::mpsc;

use super::tab_backend::HostDataMsg;
use crate::engine_feed::{EngineFeed, EngineFeedSender};
use crate::host_conn::queue::HostOps;
use crate::host_conn::state::CLIENT_PAYLOAD_KINDS;

/// History pages stepped per feed-drain pass. Bounds main-thread work so
/// a large-scrollback attach never stalls a frame; the drain re-arms
/// itself with [`HostTabFrame::StepDecoder`] while pages remain.
const PAGES_PER_PASS: usize = 8;

/// The pre-swap resize withhold (architecture §5: a resize mid-snapshot
/// forfeits the remaining history pages, so a withheld one is sent when
/// the hydration completes — but never held longer than this).
const WITHHOLD_DEADLINE: Duration = Duration::from_secs(2);

/// Re-attach backoff: base doubling, capped. Deterministic (no jitter):
/// one client re-attaching one focused tab is not a thundering herd.
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

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

/// Where a detached tab can pick its stream back up: the resume identity
/// from `tab.attach` plus the next seq this client has not applied. Kept
/// per tab across detach — refocus hands it back and the wire answers
/// `mode: "resume"` when the ring still covers it, or falls back to a
/// fresh snapshot in the same reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResumePoint {
    pub(crate) server_epoch: u64,
    pub(crate) tab_generation: u64,
    pub(crate) next_seq: u64,
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
        /// The geometry the payload was encoded at, when the session
        /// says it is not the one this attach asked for — an unfocused
        /// attach, which resizes nothing. `None` on a focused attach, on
        /// a resume, and from every session predating `open_input`.
        snapshot_size: Option<(u16, u16)>,
    },
    /// The attach op or the dial failed before any frame flowed.
    Failed {
        attempt: u64,
        reason: FailReason,
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
    /// PTY frames that arrived before the payload could take them,
    /// replayed in order once it can. The server holds live PTY on its
    /// side too, so this is belt-and-braces for the tiny window the two
    /// rules can miss.
    deferred: VecDeque<Vec<u8>>,
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
    /// The cols/rows the terminal being built here is at — the
    /// snapshot's own where the session reported one
    /// (`AttachAccepted.snapshot_cols/rows`), this attach's otherwise.
    /// What [`HostAttach::finish_hydration`] resizes *from* once the
    /// payload is whole; nothing else touches it while the hydration
    /// runs.
    built_at: (u16, u16),
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
    /// Token mint + dial in flight on the attempt's task.
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
    /// The latest user resize withheld during hydration (latest-wins).
    withheld: Option<Geometry>,
    /// The geometry the current attempt negotiated (or is negotiating).
    geometry: Geometry,
    /// The persistent input queue: keystrokes survive re-attach windows
    /// here. The sender side also lives in the tab's `TabHandle`.
    input_tx: mpsc::UnboundedSender<HostDataMsg>,
    input_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<HostDataMsg>>>,
    /// The current attempt's tasks (dial/reader/writer/timers), aborted
    /// wholesale on detach or re-attach.
    tasks: Vec<tokio::task::AbortHandle>,
    backoff_step: u32,
}

impl HostAttach {
    pub(super) fn new(key: TabKey, geometry: Geometry) -> Self {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        Self {
            key,
            attempt: 0,
            phase: Phase::Requesting,
            resume: None,
            kind: None,
            withheld: None,
            geometry,
            input_tx,
            input_rx: Arc::new(tokio::sync::Mutex::new(input_rx)),
            tasks: Vec::new(),
            backoff_step: 0,
        }
    }

    /// Restore the resume point a previous attach of this tab left
    /// behind (refocus). Must be set before [`Self::begin`].
    pub(super) fn with_resume(mut self, resume: Option<ResumePoint>) -> Self {
        self.resume = resume;
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

    /// Whether input queued here still has a reader.
    ///
    /// True for every phase but [`Phase::Ended`]. `Requesting` and
    /// `Hydrating` queue rather than drop — the input queue outlives an
    /// attempt, which is the whole reason it lives on the attach and not
    /// on the attempt's task.
    pub(super) fn live(&self) -> bool {
        !matches!(self.phase, Phase::Ended)
    }

    /// Start (or restart) an attach attempt. Must be called inside the
    /// app runtime (`Runtime::enter`) — every task binds to the ambient
    /// runtime. `ops` is the host's op queue (token minting rides it so
    /// it cannot interleave with `session.set_theme`), `socket` the
    /// host's endpoint.
    pub(super) fn begin(
        &mut self,
        ops: &HostOps,
        socket: std::path::PathBuf,
        libghostty_build: &str,
        feed: &EngineFeedSender,
    ) {
        self.abort_tasks();
        self.attempt += 1;
        self.phase = Phase::Requesting;
        let attempt = self.attempt;
        let key = self.key;
        let geometry = self.geometry;
        let resume = self.resume;
        let attach_call = ops.call(
            ops::TAB_ATTACH,
            serde_json::json!({
                "tab_id": key.tab.to_string(),
                "kinds": CLIENT_PAYLOAD_KINDS,
                "cols": geometry.cols,
                "rows": geometry.rows,
                "cell_w_px": geometry.cell_w,
                "cell_h_px": geometry.cell_h,
                "libghostty_build": libghostty_build,
                // Attach is on-focus in this client, so the claim is
                // always true. Stated rather than omitted: protocol 5
                // requires the field, and the omit-when-true shim that
                // used to cover this is gone.
                "focus": true,
            }),
        );
        let input_rx = Arc::clone(&self.input_rx);
        let task = tokio::spawn(run_attempt(
            key,
            attempt,
            attach_call,
            socket,
            resume,
            input_rx,
            feed.clone(),
        ));
        self.tasks.push(task.abort_handle());
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
        let key = self.key;
        let feed = feed.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            feed.send(EngineFeed::HostTab(key, frame));
        });
        self.tasks.push(timer.abort_handle());
    }

    /// Adopt `geometry` and queue its RESIZE behind whatever input is
    /// already waiting, so the wire sees the two in submission order.
    fn queue_geometry(&mut self, geometry: Geometry) {
        self.geometry = geometry;
        let _ = self.input_tx.send(HostDataMsg::Resize {
            cols: geometry.cols,
            rows: geometry.rows,
            cell_w: geometry.cell_w,
            cell_h: geometry.cell_h,
        });
    }

    /// A user resize while attached. Live: queued onto the wire in order
    /// with input. Hydrating: withheld (latest-wins) — sent at FINISH or
    /// at the 2 s deadline, because a mid-snapshot resize forfeits the
    /// remaining history pages.
    pub(super) fn note_resize(&mut self, geometry: Geometry) {
        if geometry == self.geometry {
            return;
        }
        match &self.phase {
            Phase::Live => self.queue_geometry(geometry),
            Phase::Requesting | Phase::Hydrating(_) => {
                self.withheld = Some(geometry);
            }
            Phase::Ended => {}
        }
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
                snapshot_size,
                ..
            } => {
                self.kind = Some(kind);
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
                    // Reaching Live is what resets the backoff ladder —
                    // a mere accept from a server that then dies would
                    // otherwise hot-loop at the base delay.
                    self.backoff_step = 0;
                    self.phase = Phase::Live;
                    if let Some(geometry) = self.withheld.take() {
                        // A stale withhold from a previous attempt's
                        // hydration: send it now, it gates nothing.
                        self.note_resize(geometry);
                    }
                } else {
                    // The payload's own geometry when the session named
                    // one, this attach's otherwise. A session names it
                    // whenever it can — this attach's own request is no
                    // evidence, because raw input is open and anything
                    // else may have sized the tab between the resize and
                    // the encode. Replaying a payload composed at
                    // another width into a terminal of this width wraps
                    // its lines and misplaces its absolute cursor moves,
                    // so the terminal is built at the payload's size and
                    // resized to this client's afterwards.
                    let built_at =
                        snapshot_size.unwrap_or((self.geometry.cols, self.geometry.rows));
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
                AttachStep::None
            }
            HostTabFrame::Failed { reason, .. } => match reason {
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
            },
            HostTabFrame::Snap { bytes, .. } => self.apply_snap(&bytes, tab, feed),
            HostTabFrame::Pty { seq, bytes, .. } => self.apply_pty(seq, bytes, tab),
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
                // Still hydrating with a resize on hold: stop holding.
                let Phase::Hydrating(hydration) = &mut self.phase else {
                    return AttachStep::None;
                };
                let Some(geometry) = self.withheld.take() else {
                    // Nothing on hold *yet*. What this bounds is the
                    // hold, not the attach, so the next deadline is
                    // armed here: a hydration outlives this one (a `vt`
                    // payload by its whole length), and a resize
                    // arriving from now on would otherwise have nothing
                    // left to act on it until the payload ended.
                    let attempt = self.attempt;
                    self.arm_timer(
                        WITHHOLD_DEADLINE,
                        feed,
                        HostTabFrame::WithholdDeadline { attempt },
                    );
                    return AttachStep::None;
                };
                let Some(decoder) = hydration.live_decoder() else {
                    // Nothing to mirror it onto: a decoder short of READY
                    // has no screen yet, and a `vt` payload was composed
                    // for the attach geometry and cannot be re-cut.
                    // Attach fresh at the new size — attach is when the
                    // server resizes.
                    tracing::debug!(key = %self.key, "withhold deadline with no hydration to mirror it; re-attaching at the new size");
                    self.geometry = geometry;
                    return self.schedule_reattach();
                };
                // The decoder mirrors the resize before the RESIZE goes
                // out — the remaining history pages are forfeited, which
                // the decoder reports as zero-row pages (snapshot.h). A
                // decoder that refuses leaves the wire untouched: the
                // stream is about to be replaced anyway.
                if let Err(error) = decoder.resize(
                    geometry.cols,
                    geometry.rows,
                    geometry.cell_w,
                    geometry.cell_h,
                ) {
                    tracing::debug!(key = %self.key, %error, "decoder resize failed; re-attaching");
                    self.geometry = geometry;
                    return self.schedule_reattach();
                }
                self.queue_geometry(geometry);
                AttachStep::None
            }
            HostTabFrame::StepDecoder { .. } => {
                if let Phase::Hydrating(hydration) = &mut self.phase {
                    hydration.stepping = false;
                }
                self.drive_decoder(tab, feed)
            }
        }
    }

    /// Detach: abort this attempt's tasks and abandon a mid-flight
    /// decoder on this thread — never dropping it mid-`feed` from an
    /// aborted task, which is why the decoder lives here and not there.
    /// The resume point survives in the return value.
    pub(super) fn detach(mut self) -> Option<ResumePoint> {
        self.abort_tasks();
        if let Phase::Hydrating(hydration) = std::mem::replace(&mut self.phase, Phase::Ended) {
            hydration.hydrator.abandon();
        }
        self.resume
    }

    fn abort_tasks(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
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

    fn schedule_reattach(&mut self) -> AttachStep {
        self.abort_tasks();
        if let Phase::Hydrating(hydration) = std::mem::replace(&mut self.phase, Phase::Requesting) {
            hydration.hydrator.abandon();
        }
        let delay = BACKOFF_BASE
            .saturating_mul(1u32 << self.backoff_step.min(5))
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
                    terminal.vt_write(&deferred);
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
                            // these are live bytes from after the
                            // snapshot's fence, and the decoder
                            // interleaves them correctly from READY.
                            while let Some(bytes) = hydration.deferred.pop_front() {
                                if let Err(error) = decoder.vt_write(&bytes) {
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

    fn apply_pty(
        &mut self,
        seq: u64,
        bytes: Vec<u8>,
        tab: &mut super::terminal_tab::TerminalTab,
    ) -> AttachStep {
        let Some(expected) = self.expected_next_seq() else {
            tracing::debug!(key = %self.key, "PTY before an accepted handshake; re-attaching");
            return self.schedule_reattach();
        };
        if seq != expected {
            // A gap or a duplicate: the terminal would silently diverge
            // and could never tell. Fatal by contract; re-attach.
            tracing::debug!(
                key = %self.key, seq, expected,
                "PTY seq discontinuity; re-attaching"
            );
            return self.schedule_reattach();
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
                        hydration.deferred.push_back(bytes);
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
                Ok(HistoryStep::Page { .. }) => {}
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
        if let Err(error) = tab.swap_terminal(terminal, built_at.0, built_at.1) {
            tracing::warn!(key = %self.key, %error, "hydrated terminal swap failed; re-attaching");
            return self.schedule_reattach();
        }
        // Only now is the hydration's fence true of the rendered
        // terminal — this is what makes an aborted hydration resume from
        // the OLD point instead of claiming bytes it never applied.
        self.resume = Some(identity);
        // The stream proved itself end to end; the next failure starts
        // the ladder from the bottom.
        self.backoff_step = 0;
        if let Some(geometry) = self.withheld.take() {
            // Held through hydration; send it now, in order behind any
            // buffered input.
            self.queue_geometry(geometry);
        }
        // The swapped-in terminal is at the geometry its payload was
        // composed for, which is not always this client's: a resize
        // withheld through the hydration has just moved it, and an
        // unfocused attach hydrated at the snapshot's own size. Either
        // way nothing else would ever correct it — no later resize pass
        // runs unless the window moves again.
        let target = self.geometry;
        if built_at != (target.cols, target.rows) {
            if let Err(error) =
                tab.resize_for_host(target.cols, target.rows, target.cell_w, target.cell_h)
            {
                tracing::warn!(key = %self.key, %error, "post-swap resize failed; re-attaching");
                return self.schedule_reattach();
            }
        }
        AttachStep::Refresh
    }
}

fn frame_attempt(frame: &HostTabFrame) -> u64 {
    match frame {
        HostTabFrame::Accepted { attempt, .. }
        | HostTabFrame::Failed { attempt, .. }
        | HostTabFrame::Snap { attempt, .. }
        | HostTabFrame::Pty { attempt, .. }
        | HostTabFrame::Exit { attempt, .. }
        | HostTabFrame::Error { attempt, .. }
        | HostTabFrame::Closed { attempt }
        | HostTabFrame::ReattachDue { attempt }
        | HostTabFrame::WithholdDeadline { attempt }
        | HostTabFrame::StepDecoder { attempt } => *attempt,
    }
}

/// Pick the handshake: hand the resume identity back only when it
/// matches this session process — a stale epoch or generation would
/// just round-trip to a snapshot fallback anyway, but not asking is
/// clearer than asking wrong.
fn choose_handshake(result: &TabAttachResult, resume: Option<ResumePoint>) -> AttachHandshake {
    match resume {
        Some(r)
            if r.server_epoch == result.server_epoch
                && r.tab_generation == result.tab_generation =>
        {
            AttachHandshake::resume(
                &result.attach_token,
                r.next_seq,
                r.server_epoch,
                r.tab_generation,
            )
        }
        _ => AttachHandshake::snapshot(&result.attach_token),
    }
}

/// What the payload that is about to arrive will be decoded as.
///
/// The data connection's own `AttachAccepted.kind` is the authority: it
/// rides the ticket the server admitted, and the bytes behind it are
/// what that server composed. `tab.attach`'s reply must have said the
/// same thing — two different answers to one negotiation is a protocol
/// error, and picking either of them would be guessing which one the
/// stream honors. Re-attaching is the recovery, as it is for every other
/// stream this client cannot trust.
fn negotiated_kind(
    control: &AttachPayloadKind,
    accepted: &AttachPayloadKind,
) -> Result<PayloadKind, String> {
    if control != accepted {
        return Err(format!(
            "tab.attach negotiated {control}, the data connection accepted {accepted}"
        ));
    }
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

/// Classify a token-mint refusal off the op queue.
fn classify_op_failure(error: &crate::host_conn::queue::HostOpError) -> FailReason {
    use crate::host_conn::queue::HostOpError;
    match error {
        HostOpError::Rejected { code, .. } => reason_for(Some(code), error.to_string()),
        // `Local` is the upload lane's own refusal and cannot reach a
        // token mint at all; grouped with the two that do not retry
        // because an unexplained client-side refusal is not something a
        // second attach attempt would fix either.
        HostOpError::Disconnected | HostOpError::Unavailable | HostOpError::Local(_) => {
            FailReason::HostGone(error.to_string())
        }
        HostOpError::Transport(_) => FailReason::Retryable(error.to_string()),
    }
}

/// Classify an attach-op or dial refusal, applied one step earlier in
/// the lifecycle.
fn classify_failure(error: &ClientError) -> FailReason {
    reason_for(error.server_code().as_ref(), error.to_string())
}

/// The background half of one attempt: mint the ticket through the op
/// queue, dial the data connection, then split into a reader loop
/// (frames → feed) and a writer loop (input queue → wire). Every await
/// lives out here; the main thread only ever sees feed items.
async fn run_attempt(
    key: TabKey,
    attempt: u64,
    attach_call: impl std::future::Future<
        Output = Result<serde_json::Value, crate::host_conn::queue::HostOpError>,
    >,
    socket: std::path::PathBuf,
    resume: Option<ResumePoint>,
    input_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<HostDataMsg>>>,
    feed: EngineFeedSender,
) {
    let fail = |reason: FailReason, feed: &EngineFeedSender| {
        feed.send(EngineFeed::HostTab(
            key,
            HostTabFrame::Failed { attempt, reason },
        ));
    };
    let result = match attach_call.await {
        Ok(value) => value,
        Err(error) => {
            return fail(classify_op_failure(&error), &feed);
        }
    };
    let result: TabAttachResult = match serde_json::from_value(result) {
        Ok(result) => result,
        Err(error) => {
            return fail(
                FailReason::Retryable(format!("tab.attach reply did not decode: {error}")),
                &feed,
            );
        }
    };
    let handshake = choose_handshake(&result, resume);
    // Bounded, on the same budget a control leg gets. `DataConnection::dial`
    // has no timeout of its own, and the socket it dials is not always a
    // local one: over the ssh transport it is a bridge whose accept is a
    // remote `ssh` exec, so an unreachable host would otherwise park this
    // attempt forever with the tab showing "attaching…" and no retry. A
    // timeout is a failed dial like any other — retryable, and the
    // re-attach backoff decides what happens next.
    let budget = crate::host_conn::leg_budget();
    let (accepted, conn) =
        match tokio::time::timeout(budget, DataConnection::dial(&socket, &handshake)).await {
            Ok(Ok(accepted)) => accepted,
            Ok(Err(error)) => return fail(classify_failure(&error), &feed),
            Err(_elapsed) => {
                return fail(
                    FailReason::Retryable(format!(
                        "attaching to {} timed out after {}s",
                        socket.display(),
                        budget.as_secs().max(1)
                    )),
                    &feed,
                )
            }
        };
    let kind = match negotiated_kind(&result.kind, &accepted.kind) {
        Ok(kind) => kind,
        Err(message) => return fail(FailReason::Retryable(message), &feed),
    };
    feed.send(EngineFeed::HostTab(
        key,
        HostTabFrame::Accepted {
            attempt,
            resumed: accepted.mode == roost_ipc::messages::AttachMode::Resume,
            kind,
            fence: accepted.seq,
            server_epoch: accepted.server_epoch,
            tab_generation: accepted.tab_generation,
            // Both or neither: the pair names one geometry, and half of
            // one is not a size to build a terminal at.
            snapshot_size: accepted.snapshot_cols.zip(accepted.snapshot_rows),
        },
    ));
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
    let mut rx = input_rx.lock().await;
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
                    HostDataMsg::Input(bytes) => writer.send_input(&bytes).await,
                    HostDataMsg::Resize {
                        cols,
                        rows,
                        cell_w,
                        cell_h,
                    } => {
                        writer
                            .send_resize(cols, rows, cell_w as u16, cell_h as u16)
                            .await
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
    }
}

#[cfg(test)]
mod tests {
    use roost_vt::{Terminal, TerminalOptions};

    use super::*;
    use crate::app::terminal_tab::TerminalTab;
    use crate::engine_feed::{self, EngineFeedReceiver};

    const GEOMETRY: Geometry = Geometry {
        cols: 80,
        rows: 24,
        cell_w: 9,
        cell_h: 18,
    };

    fn key() -> TabKey {
        TabKey::new(roost_ui_model::keys::HostId::new(3), 7)
    }

    /// A machine plus the tab it drives and the feed its timers write to.
    fn rig() -> (
        HostAttach,
        TerminalTab,
        EngineFeedSender,
        EngineFeedReceiver,
    ) {
        let (feed_tx, feed_rx) = engine_feed::channel();
        let mut attach = HostAttach::new(key(), GEOMETRY);
        // The frames below are hand-fed, so the machine must be on the
        // attempt they carry.
        attach.attempt = 1;
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
        accepted_at(kind, resumed, fence, None)
    }

    /// The accepted handshake as an *unfocused* attach gets it: the
    /// session reports the geometry it composed the payload at, because
    /// it did not resize the tab to this client's.
    fn accepted_at(
        kind: PayloadKind,
        resumed: bool,
        fence: u64,
        snapshot_size: Option<(u16, u16)>,
    ) -> HostTabFrame {
        HostTabFrame::Accepted {
            attempt: 1,
            resumed,
            kind,
            fence,
            server_epoch: 11,
            tab_generation: 2,
            snapshot_size,
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
        let mut step = attach.on_frame(HostTabFrame::Snap { attempt: 1, bytes }, tab, feed_tx);
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
            AttachStep::None
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

    /// A stale identity is not handed back: the handshake downgrades to
    /// a plain snapshot request when epoch or generation moved.
    #[test]
    fn a_stale_resume_identity_asks_for_a_snapshot() {
        let result = TabAttachResult {
            attach_token: "t".into(),
            kind: AttachPayloadKind::GHOSTTY_SNAPSHOT.into(),
            server_epoch: 11,
            tab_generation: 2,
        };
        let stale_epoch = choose_handshake(
            &result,
            Some(ResumePoint {
                server_epoch: 10,
                tab_generation: 2,
                next_seq: 42,
            }),
        );
        assert_eq!(stale_epoch.resume_from_seq, None, "stale epoch: snapshot");
        let stale_generation = choose_handshake(
            &result,
            Some(ResumePoint {
                server_epoch: 11,
                tab_generation: 1,
                next_seq: 42,
            }),
        );
        assert_eq!(
            stale_generation.resume_from_seq, None,
            "stale generation: snapshot"
        );
        let hit = choose_handshake(
            &result,
            Some(ResumePoint {
                server_epoch: 11,
                tab_generation: 2,
                next_seq: 42,
            }),
        );
        assert_eq!(hit.resume_from_seq, Some(42), "matching identity resumes");
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

    /// A resize during hydration is withheld (latest-wins) and fires
    /// once at FINISH.
    #[tokio::test]
    async fn a_withheld_resize_fires_at_finish() {
        let (mut attach, mut tab, feed_tx, mut feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        attach.note_resize(Geometry {
            cols: 100,
            rows: 30,
            ..GEOMETRY
        });
        attach.note_resize(Geometry {
            cols: 120,
            rows: 40,
            ..GEOMETRY
        });
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
        let sent = attach.test_drain_input();
        assert_eq!(sent.len(), 1, "latest-wins: one RESIZE, not two");
        assert!(
            matches!(
                sent[0],
                HostDataMsg::Resize {
                    cols: 120,
                    rows: 40,
                    ..
                }
            ),
            "the latest geometry is the one sent"
        );
    }

    /// The same withhold fires at the 2 s deadline once READY has
    /// landed, forfeiting the remaining history rather than holding the
    /// user's geometry hostage.
    #[tokio::test]
    async fn a_withheld_resize_fires_at_the_deadline() {
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
        attach.note_resize(Geometry {
            cols: 132,
            rows: 50,
            ..GEOMETRY
        });
        assert_eq!(
            attach.on_frame(
                HostTabFrame::WithholdDeadline { attempt: 1 },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::None
        );
        let sent = attach.test_drain_input();
        assert_eq!(sent.len(), 1);
        assert!(matches!(
            sent[0],
            HostDataMsg::Resize {
                cols: 132,
                rows: 50,
                ..
            }
        ));
    }

    /// A deadline that beats READY re-attaches at the new geometry
    /// instead — a decoder that has not reached READY cannot mirror a
    /// resize, and attach is when the server resizes anyway.
    #[tokio::test]
    async fn a_deadline_before_ready_reattaches_at_the_new_size() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(false, 100), &mut tab, &feed_tx);
        attach.note_resize(Geometry {
            cols: 132,
            rows: 50,
            ..GEOMETRY
        });
        assert!(matches!(
            attach.on_frame(
                HostTabFrame::WithholdDeadline { attempt: 1 },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::Reattach { .. }
        ));
        assert!(
            attach.test_drain_input().is_empty(),
            "no RESIZE rides a stream about to be replaced"
        );
        assert_eq!(
            attach.geometry.cols, 132,
            "the retry attaches at the new size"
        );
    }

    /// While live, a resize goes straight out — in order behind input.
    #[tokio::test]
    async fn a_live_resize_queues_behind_input() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted(true, 41), &mut tab, &feed_tx);
        tab.session.send_input(b"typed".to_vec());
        attach.note_resize(Geometry {
            cols: 90,
            rows: 28,
            ..GEOMETRY
        });
        let sent = attach.test_drain_input();
        assert!(matches!(sent[0], HostDataMsg::Input(ref bytes) if bytes == b"typed"));
        assert!(matches!(sent[1], HostDataMsg::Resize { cols: 90, .. }));
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
            attach.detach(),
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
            attach.detach(),
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
            attach.detach(),
            Some(ResumePoint {
                server_epoch: 11,
                tab_generation: 2,
                next_seq: 101,
            }),
            "the terminator promotes the fence, exactly as FINISH does"
        );
    }

    /// An **unfocused** attach gets a payload composed at the server's
    /// geometry, not at the one it asked for — it resized nothing — and
    /// the accepted handshake says so (plan 057 §3.3). A `vt` client has
    /// to build its terminal at that size, because those bytes replay
    /// into a terminal *of the payload's* width: replaying them into a
    /// narrower one wraps their lines and misplaces their absolute cursor
    /// moves. Then it resizes to its own, which nothing else would ever
    /// do — no resize pass runs again unless the window moves.
    ///
    /// Roost's own client always attaches focused today, so this is the
    /// path a future unfocused attacher takes; the negative control below
    /// is the one this build exercises.
    #[tokio::test]
    async fn a_vt_payload_hydrates_at_the_snapshot_geometry_and_then_takes_this_clients() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(
            accepted_at(PayloadKind::Vt, false, 100, Some((100, 30))),
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
        tab.refresh_snapshot().expect("refresh");
        assert_eq!(
            tab.dump(0).expect("dump").rows_text.len(),
            usize::from(GEOMETRY.rows),
            "and the swapped-in terminal is then this client's size, not the server's"
        );
    }

    /// The negative control, and the path every attach roost makes today
    /// takes: a focused attach resized the tab to this client's geometry
    /// before composing, so the handshake reports no snapshot size and
    /// nothing resizes after the swap.
    #[tokio::test]
    async fn a_focused_attach_hydrates_at_its_own_geometry_and_resizes_nothing() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(
            accepted_at(PayloadKind::Vt, false, 100, None),
            &mut tab,
            &feed_tx,
        );
        let Phase::Hydrating(hydration) = &attach.phase else {
            panic!("a fresh vt payload hydrates");
        };
        assert_eq!(hydration.built_at, (GEOMETRY.cols, GEOMETRY.rows));

        attach.on_frame(snap(b"focused"), &mut tab, &feed_tx);
        attach.on_frame(snap(b""), &mut tab, &feed_tx);
        tab.refresh_snapshot().expect("refresh");
        let dump = tab.dump(0).expect("dump");
        assert_eq!(dump.rows_text.len(), usize::from(GEOMETRY.rows));
        assert!(dump.rows_text.join("\n").contains("focused"));
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

    /// The data connection's `AttachAccepted.kind` is the authority, and
    /// `tab.attach`'s reply must have agreed with it: two answers to one
    /// negotiation is a protocol error, not a coin toss.
    #[test]
    fn a_handshake_that_contradicts_the_control_reply_is_refused() {
        let snapshot = AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT);
        let vt = AttachPayloadKind::from(AttachPayloadKind::VT);
        assert_eq!(
            negotiated_kind(&snapshot, &snapshot),
            Ok(PayloadKind::GhosttySnapshot)
        );
        assert_eq!(negotiated_kind(&vt, &vt), Ok(PayloadKind::Vt));

        let disagreed = negotiated_kind(&snapshot, &vt).unwrap_err();
        assert!(
            disagreed.contains(AttachPayloadKind::GHOSTTY_SNAPSHOT)
                && disagreed.contains(AttachPayloadKind::VT),
            "the refusal names both answers: {disagreed}"
        );

        let unoffered = AttachPayloadKind::from("sixel-mosaic");
        assert!(
            negotiated_kind(&unoffered, &unoffered).is_err(),
            "a kind this client never offered is not one it can decode"
        );
    }

    /// A resize that runs out of patience during a `vt` payload
    /// re-attaches at the new size instead of being mirrored: the server
    /// composed those bytes for the old geometry, and unlike the
    /// decoder's screen there is nothing here to re-cut.
    #[tokio::test]
    async fn a_withheld_resize_during_a_vt_payload_reattaches_at_the_new_size() {
        let (mut attach, mut tab, feed_tx, _feed_rx) = rig();
        attach.on_frame(accepted_as(PayloadKind::Vt, false, 100), &mut tab, &feed_tx);
        attach.on_frame(snap(b"half a payload"), &mut tab, &feed_tx);
        attach.note_resize(Geometry {
            cols: 132,
            rows: 50,
            ..GEOMETRY
        });
        assert!(matches!(
            attach.on_frame(
                HostTabFrame::WithholdDeadline { attempt: 1 },
                &mut tab,
                &feed_tx,
            ),
            AttachStep::Reattach { .. }
        ));
        assert!(
            attach.test_drain_input().is_empty(),
            "no RESIZE rides a stream about to be replaced"
        );
        assert_eq!(
            attach.geometry.cols, 132,
            "the retry attaches at the new size"
        );
    }

    /// The deadline bounds the *hold*, not the accept, so a resize that
    /// arrives after one has already fired is still acted on within it.
    /// A `vt` hydration is where this bites: it holds for the whole
    /// payload, so most of its life is after that first deadline, and a
    /// resize landing there would otherwise wait out the entire attach
    /// and then be applied to a terminal replayed at the old size.
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

        // Only now does the user drag the window, mid-payload.
        attach.note_resize(Geometry {
            cols: 132,
            rows: 50,
            ..GEOMETRY
        });
        let deadline = next_withhold_deadline(&mut feed_rx).await;
        assert!(matches!(
            attach.on_frame(deadline, &mut tab, &feed_tx),
            AttachStep::Reattach { .. }
        ));
        assert_eq!(
            attach.geometry.cols, 132,
            "the retry attaches at the size the user is looking at"
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
            attach.detach(),
            Some(ResumePoint {
                server_epoch: 11,
                tab_generation: 2,
                next_seq: 102,
            })
        );
    }
}
