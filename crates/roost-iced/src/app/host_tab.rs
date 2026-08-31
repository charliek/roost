//! The focused host tab's attach: token mint → data dial → hydration →
//! live, with resume across refocus (host-sessions plan 037 §3.4).
//!
//! Ownership is split the way the threading rules demand (CLAUDE.md):
//! background tasks only move bytes — the dial task mints the token and
//! runs the handshake, a reader task turns data-plane frames into
//! [`HostTabFrame`]s on the engine feed, a writer task drains the tab's
//! input queue onto the wire — while the [`SnapshotDecoder`] and both
//! terminals (the old one still rendering, the new one hydrating) live
//! in [`HostAttach`] on the main thread and are driven from the feed
//! drain.
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
use roost_vt::{HistoryStep, ReadyState, SnapshotDecodeOptions, SnapshotDecoder};
use tokio::sync::mpsc;

use super::tab_backend::HostDataMsg;
use crate::engine_feed::{EngineFeed, EngineFeedSender};
use crate::host_conn::queue::HostOps;

/// History pages stepped per feed-drain pass. Bounds main-thread work so
/// a large-scrollback attach never stalls a frame; the drain re-arms
/// itself with [`HostTabFrame::StepDecoder`] while pages remain.
const PAGES_PER_PASS: usize = 8;

/// The pre-FINISH resize withhold (architecture §5: a resize mid-
/// snapshot forfeits the remaining history pages, so a withheld one is
/// sent at FINISH — but never held longer than this).
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
    /// The handshake was accepted: the stream identity and the fence.
    Accepted {
        attempt: u64,
        resumed: bool,
        fence: u64,
        server_epoch: u64,
        tab_generation: u64,
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
    /// The lease moved or the session is stopping — the host connection
    /// owns the recovery; the tab detaches passively.
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
    /// (`superseded`), the lease moved, or the build mismatches. The
    /// host-level state (banner, NeedsRestart) is the connection's to
    /// publish, not this tab's.
    Detach,
    /// EXIT validated — the tab is over; the mirror's `tab.closed`
    /// drives the row out of the sidebar.
    Closed {
        code: i32,
    },
}

/// Hydration-phase state: the decoder plus the deferral and rendering it
/// needs. Lives on the main thread only.
struct Hydration {
    decoder: SnapshotDecoder,
    /// The stream identity + fence progress of THIS hydration. Promoted
    /// to [`HostAttach::resume`] only at FINISH: until the swap, the
    /// rendered terminal is still the old one, and advertising the new
    /// fence early would let an aborted hydration "resume" onto a
    /// terminal that never took the snapshot — permanent divergence.
    identity: ResumePoint,
    /// PTY frames that arrived before READY, replayed in order at READY.
    /// The server holds live PTY until READY on its side too, so this is
    /// belt-and-braces for the tiny window the two rules can miss.
    deferred: VecDeque<Vec<u8>>,
    /// Bytes queued in `deferred` — bounded, because a server violating
    /// hold-until-READY must not grow client memory without limit.
    deferred_bytes: usize,
    ready: bool,
    /// Bounded stepping left pages behind; a `StepDecoder` self-wake is
    /// in flight.
    stepping: bool,
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
                "kinds": [AttachPayloadKind::GHOSTTY_SNAPSHOT],
                "cols": geometry.cols,
                "rows": geometry.rows,
                "cell_w_px": geometry.cell_w,
                "cell_h_px": geometry.cell_h,
                "libghostty_build": libghostty_build,
            }),
            true,
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
                fence,
                server_epoch,
                tab_generation,
                ..
            } => {
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
                    // A fresh snapshot supersedes anything the old
                    // stream had applied; the fence restarts the count —
                    // inside the hydration, not in `self.resume`, which
                    // keeps describing the terminal actually rendered.
                    self.phase = Phase::Hydrating(Box::new(Hydration {
                        decoder: SnapshotDecoder::new(SnapshotDecodeOptions::default()),
                        identity,
                        deferred: VecDeque::new(),
                        deferred_bytes: 0,
                        ready: false,
                        stepping: false,
                    }));
                    // The withhold deadline covers only snapshot mode; a
                    // resume has no FINISH to wait for.
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
                    ServerCode::Superseded => {
                        // Another window took the tab: this one lets go.
                        self.phase = Phase::Ended;
                        AttachStep::Detach
                    }
                    ServerCode::TakenOver | ServerCode::ShuttingDown => {
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
                    return AttachStep::None;
                };
                if !hydration.ready {
                    // READY has not even landed after 2 s: the snapshot
                    // has nothing worth keeping and a decoder that has
                    // not reached READY cannot mirror a resize. Attach
                    // fresh at the new geometry — attach is when the
                    // server resizes.
                    tracing::debug!(key = %self.key, "withhold deadline before READY; re-attaching at the new size");
                    self.geometry = geometry;
                    return self.schedule_reattach();
                }
                // The decoder mirrors the resize before the RESIZE goes
                // out — the remaining history pages are forfeited, which
                // the decoder reports as zero-row pages (snapshot.h). A
                // decoder that refuses leaves the wire untouched: the
                // stream is about to be replaced anyway.
                if let Err(error) = hydration.decoder.resize(
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
            drop(hydration.decoder.abandon());
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
            drop(hydration.decoder.abandon());
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
            // SNAP outside hydration: the stream is confused. Rebuild.
            tracing::debug!(key = %self.key, "SNAP outside hydration; re-attaching");
            return self.schedule_reattach();
        };
        if let Err(error) = hydration.decoder.feed(bytes) {
            tracing::debug!(key = %self.key, %error, "snapshot decode failed; re-attaching");
            return self.schedule_reattach();
        }
        if !hydration.ready {
            match hydration.decoder.try_ready() {
                Ok(ReadyState::NeedMoreBytes) => return AttachStep::None,
                Ok(ReadyState::Ready) => {
                    hydration.ready = true;
                    // Replay the deferral in arrival order — these are
                    // live bytes from after the snapshot's fence, and
                    // the decoder interleaves them correctly from READY.
                    while let Some(bytes) = hydration.deferred.pop_front() {
                        if let Err(error) = hydration.decoder.vt_write(&bytes) {
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
                if !hydration.ready {
                    hydration.deferred_bytes += bytes.len();
                    if hydration.deferred_bytes > MAX_DEFERRED_BYTES {
                        tracing::debug!(
                            key = %self.key,
                            "peer streamed PTY past the pre-READY budget; re-attaching"
                        );
                        return self.schedule_reattach();
                    }
                    hydration.deferred.push_back(bytes);
                    AttachStep::None
                } else if let Err(error) = hydration.decoder.vt_write(&bytes) {
                    tracing::debug!(key = %self.key, %error, "hydration vt_write failed; re-attaching");
                    self.schedule_reattach()
                } else {
                    AttachStep::Refresh
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
        if !hydration.ready {
            return AttachStep::None;
        }
        for _ in 0..PAGES_PER_PASS {
            match hydration.decoder.try_next() {
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
        let decoded = match hydration.decoder.finish() {
            Ok(decoded) => decoded,
            Err(error) => {
                tracing::debug!(key = %self.key, %error, "snapshot finish failed; re-attaching");
                return self.schedule_reattach();
            }
        };
        if let Err(error) =
            tab.swap_terminal(decoded.terminal, self.geometry.cols, self.geometry.rows)
        {
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
            // buffered input — and mirror it onto the freshly swapped
            // terminal, which was decoded at the attach geometry and
            // would otherwise stay there (no later resize pass runs
            // unless the window moves again).
            self.queue_geometry(geometry);
            if let Err(error) = tab.resize_for_host(
                geometry.cols,
                geometry.rows,
                geometry.cell_w,
                geometry.cell_h,
            ) {
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

/// Plan §3.4's ERROR mapping: which refusals are terminal, which mean
/// the host connection owns the recovery, and which are worth a retry.
/// The one table both refusal paths below classify against.
fn reason_for(code: Option<&ServerCode>, message: String) -> FailReason {
    match code {
        Some(ServerCode::BuildMismatch) => FailReason::BuildMismatch(message),
        Some(ServerCode::TakenOver | ServerCode::ConnectRequired | ServerCode::ShuttingDown) => {
            FailReason::HostGone(message)
        }
        _ => FailReason::Retryable(message),
    }
}

/// Classify a token-mint refusal off the op queue.
fn classify_op_failure(error: &crate::host_conn::queue::HostOpError) -> FailReason {
    use crate::host_conn::queue::HostOpError;
    match error {
        HostOpError::Rejected { code, .. } => reason_for(Some(code), error.to_string()),
        HostOpError::Disconnected | HostOpError::Unavailable => {
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
    feed.send(EngineFeed::HostTab(
        key,
        HostTabFrame::Accepted {
            attempt,
            resumed: accepted.mode == roost_ipc::messages::AttachMode::Resume,
            fence: accepted.seq,
            server_epoch: accepted.server_epoch,
            tab_generation: accepted.tab_generation,
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
    // server labels its closes (`superseded`, `taken-over`, `ERROR`
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
    use roost_ui_model::theme::Theme;
    use roost_vt::{Terminal, TerminalOptions};

    use super::*;
    use crate::app::tab_backend::TabHandle;
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
        let handle = TabHandle::host(attach.input_tx(), true);
        let tab = TerminalTab::attach_host(
            GEOMETRY.cols,
            GEOMETRY.rows,
            Theme::roost_dark_fallback(),
            String::new(),
            handle,
        )
        .expect("host tab terminal");
        (attach, tab, feed_tx, feed_rx)
    }

    fn accepted(resumed: bool, fence: u64) -> HostTabFrame {
        HostTabFrame::Accepted {
            attempt: 1,
            resumed,
            fence,
            server_epoch: 11,
            tab_generation: 2,
        }
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
            tab.dump().rows_text.join("\n").contains("after-resume"),
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
            tab.dump().rows_text.join("\n").contains("old-screen"),
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
        tab.refresh_snapshot().expect("refresh");
        let text = tab.dump().rows_text.join("\n");
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
        let text = tab.dump().rows_text.join("\n");
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

    /// The ERROR-code table: `superseded` and `taken-over` detach
    /// passively (someone else drives now), `overflow`/`desync` rebuild.
    #[tokio::test]
    async fn error_codes_map_to_their_recoveries() {
        for (code, detaches) in [
            ("superseded", true),
            ("taken-over", true),
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
            !tab.dump().rows_text.join("\n").contains("from the dead"),
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
