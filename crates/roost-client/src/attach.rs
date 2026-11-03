//! One tab's data plane, transport-agnostic and ffi-free: the handshake,
//! frame validation, seq contiguity, resume bookkeeping and re-dial.
//! The terminal itself belongs to the consumer, which receives
//! [`AttachEvent`]s in wire order.
//!
//! Drop order: see [`crate::host`]. Dropping an [`AttachStream`] asks its
//! task to write `DETACH`, which only happens while the runtime is up.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use roost_ipc::client::{ClientError, DataConnection, DataWriter, ServerCode, ServerFrame};
use roost_ipc::dataframe::{DataFrameReader, InputTake};
use roost_ipc::messages::{
    AttachAccepted, AttachHandshake, AttachHandshakeTerms, AttachMode, AttachPayloadKind,
    GridFrame, SizeOwner,
};
use tokio::sync::{mpsc, watch};

use crate::dial::{BoxRead, BoxWrite, Dialer};
use crate::host::{leg, HostClient, SessionFacts};
use crate::state::{Backoff, CLIENT_PAYLOAD_KINDS};
use crate::task::jitter;

/// What to attach to, at what size, and whether the attach takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachRequest {
    pub tab_id: i64,
    pub cols: u16,
    pub rows: u16,
    pub cell_w_px: u16,
    pub cell_h_px: u16,
    pub take: bool,
}

/// The payload kind an accepted attach settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    GhosttySnapshot,
    Vt,
}

/// Delivered in wire order on one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    /// A new stream. [`AttachMode::Snapshot`]: build a fresh terminal at
    /// this geometry and feed it the [`AttachEvent::Snap`] bytes that
    /// follow, then call [`AttachStream::hydrated`] once it holds the
    /// whole payload. [`AttachMode::Resume`]: keep the terminal; only
    /// `Pty` follows.
    Accepted {
        kind: PayloadKind,
        mode: AttachMode,
        cols: u16,
        rows: u16,
        cell_w_px: u16,
        cell_h_px: u16,
        owner: Option<SizeOwner>,
    },
    /// Snapshot bytes (GHOSTSNP stream or `vt` replay). An empty `vt`
    /// frame is that payload's terminator.
    Snap(Vec<u8>),
    /// Live output; contiguity is already checked.
    Pty(Vec<u8>),
    /// The tab's grid, cell size or owner changed at this point in the
    /// stream.
    Grid {
        cols: u16,
        rows: u16,
        cell_w_px: u16,
        cell_h_px: u16,
        owner: Option<SizeOwner>,
    },
    /// The tab's program exited. Terminal.
    Exit { code: i32 },
    /// The stream dropped and is being re-dialed.
    Reconnecting { reason: String },
    /// Given up: the tab is gone, or the caller detached. Terminal.
    Ended { reason: String },
}

/// First re-dial delay after a drop.
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(10);

/// What input and focused resizes may queue while no stream is up. Past
/// it the oldest go first: a user who typed a megabyte into a dead
/// connection cares about the newest keys.
pub const OUTBOX_CAP_BYTES: usize = 1024 * 1024;

/// A handle on one tab's attach task. Dropping it detaches.
pub struct AttachStream {
    commands: mpsc::UnboundedSender<Command>,
}

#[derive(Debug)]
enum Command {
    Input(Vec<u8>, InputTake),
    Resize(Grid, bool),
    Take,
    ForgetResume,
    Hydrated,
    Detach,
}

impl AttachStream {
    /// Start attaching `req.tab_id` on the session `host` is connected
    /// to. The handshake names `host`'s client id and current session;
    /// while `host` has no session the attach waits for one.
    pub fn spawn(
        rt: &tokio::runtime::Handle,
        dialer: Arc<dyn Dialer>,
        host: &HostClient,
        req: AttachRequest,
    ) -> (AttachStream, mpsc::UnboundedReceiver<AttachEvent>) {
        let config = host.config();
        let who = Who {
            client_id: config.client_id.clone(),
            libghostty_build: config.libghostty_build.clone(),
        };
        spawn_with(rt, dialer, who, host.watch_session(), req)
    }

    /// Bytes for the tab — keys, a paste, a mouse report. A paste past
    /// the 1 MiB frame cap is split across frames, the take flag riding
    /// the side of the bytes it was asked for.
    pub fn input(&self, bytes: Vec<u8>, take: InputTake) {
        if !bytes.is_empty() {
            let _ = self.commands.send(Command::Input(bytes, take));
        }
    }

    /// This client's grid. `focused`: the user resized while looking at
    /// the tab, which takes its size; otherwise only the advisory grid
    /// moves.
    pub fn resize(&self, cols: u16, rows: u16, cell_w: u16, cell_h: u16, focused: bool) {
        let grid = Grid {
            cols,
            rows,
            cell_w_px: cell_w,
            cell_h_px: cell_h,
        };
        let _ = self.commands.send(Command::Resize(grid, focused));
    }

    /// Take the tab's size at this attachment's grid.
    pub fn take(&self) {
        let _ = self.commands.send(Command::Take);
    }

    /// The consumer's terminal is no longer at the stream's resume point
    /// (a decode failed, it was rebuilt): the next attach is a snapshot.
    pub fn forget_resume(&self) {
        let _ = self.commands.send(Command::ForgetResume);
    }

    /// The consumer's terminal holds the whole snapshot payload (the
    /// GHOSTSNP FINISH, or the `vt` terminator). Until it says so, a
    /// snapshot stream that drops is re-attached with a snapshot, never
    /// resumed: a resume replays output only, onto a terminal that would
    /// still be missing the rest of the snapshot.
    pub fn hydrated(&self) {
        let _ = self.commands.send(Command::Hydrated);
    }

    /// `DETACH`, then close. The tab's size is handed on at once.
    pub fn detach(self) {
        drop(self);
    }
}

impl Drop for AttachStream {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Detach);
    }
}

/// Who the handshake says is attaching.
#[derive(Debug, Clone)]
struct Who {
    client_id: String,
    libghostty_build: String,
}

fn spawn_with(
    rt: &tokio::runtime::Handle,
    dialer: Arc<dyn Dialer>,
    who: Who,
    session: watch::Receiver<Option<SessionFacts>>,
    req: AttachRequest,
) -> (AttachStream, mpsc::UnboundedReceiver<AttachEvent>) {
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (events, events_rx) = mpsc::unbounded_channel();
    let attach = Attach {
        tab_id: req.tab_id,
        who,
        grid: Grid {
            cols: req.cols,
            rows: req.rows,
            cell_w_px: req.cell_w_px,
            cell_h_px: req.cell_h_px,
        },
        take: req.take,
        resume: None,
        unconfirmed: None,
        outbox: Outbox::new(OUTBOX_CAP_BYTES),
    };
    rt.spawn(run(attach, dialer, session, commands_rx, events));
    (AttachStream { commands }, events_rx)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Grid {
    cols: u16,
    rows: u16,
    cell_w_px: u16,
    cell_h_px: u16,
}

/// Where a stream can be picked back up: the identity the accepted reply
/// stated, the next seq the consumer has not been handed, and the grid
/// its terminal is at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumePoint {
    session_id: String,
    server_epoch: u64,
    tab_generation: u64,
    next_seq: u64,
    grid: Grid,
}

/// The attach's state across streams.
struct Attach {
    tab_id: i64,
    who: Who,
    /// The advisory grid the next handshake states.
    grid: Grid,
    /// The caller's explicit take, until an attach is accepted with it;
    /// every re-dial after that is automatic.
    take: bool,
    resume: Option<ResumePoint>,
    /// A dropped snapshot stream's point, waiting on a late
    /// [`AttachStream::hydrated`] for the snapshot it finished.
    unconfirmed: Option<ResumePoint>,
    outbox: Outbox,
}

impl Attach {
    fn handshake(&self, session: &SessionFacts) -> AttachHandshake {
        let terms = AttachHandshakeTerms {
            session_id: session.session_id.clone(),
            kinds: CLIENT_PAYLOAD_KINDS
                .iter()
                .map(|kind| AttachPayloadKind::from(*kind))
                .collect(),
            cols: self.grid.cols,
            rows: self.grid.rows,
            cell_w_px: self.grid.cell_w_px,
            cell_h_px: self.grid.cell_h_px,
            libghostty_build: self.who.libghostty_build.clone(),
            take: self.take,
            client_id: self.who.client_id.clone(),
        };
        match &self.resume {
            Some(point) => AttachHandshake::resume(
                self.tab_id,
                terms,
                point.next_seq,
                point.server_epoch,
                point.tab_generation,
            ),
            None => AttachHandshake::snapshot(self.tab_id, terms),
        }
    }

    /// Drop resume points another session's stream left: its epoch can
    /// never match this one's.
    fn forget_other_sessions(&mut self, session_id: &str) {
        let other = |point: &Option<ResumePoint>| {
            point
                .as_ref()
                .is_some_and(|point| point.session_id != session_id)
        };
        if other(&self.resume) {
            self.resume = None;
        }
        if other(&self.unconfirmed) {
            self.unconfirmed = None;
        }
    }

    /// A command that arrived with no stream to write it to. `false`
    /// for a detach, which the caller acts on.
    fn absorb(&mut self, command: Command) -> bool {
        match command {
            Command::Input(bytes, take) => self.queue(Outgoing::Input(bytes, take)),
            Command::Resize(grid, focused) => {
                self.grid = grid;
                // An unfocused resize only moves the advisory grid, which
                // the next handshake states anyway.
                if focused {
                    self.queue(Outgoing::Resize(grid));
                }
            }
            Command::Take => self.queue(Outgoing::Take),
            Command::ForgetResume => {
                self.resume = None;
                self.unconfirmed = None;
            }
            Command::Hydrated => {
                if let Some(point) = self.unconfirmed.take() {
                    self.resume = Some(point);
                }
            }
            Command::Detach => return false,
        }
        true
    }

    fn queue(&mut self, item: Outgoing) {
        let dropped = self.outbox.push(item);
        if dropped > 0 {
            tracing::warn!(
                tab = self.tab_id,
                dropped,
                "the attach is reconnecting and its outbox is full; dropped the oldest input"
            );
        }
    }
}

/// Something written to the session once a stream is up.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outgoing {
    Input(Vec<u8>, InputTake),
    /// Always focused: an unfocused one is folded into the handshake.
    Resize(Grid),
    Take,
}

impl Outgoing {
    fn weight(&self) -> usize {
        match self {
            Outgoing::Input(bytes, _) => bytes.len(),
            Outgoing::Resize(_) => roost_ipc::dataframe::RESIZE_PAYLOAD_LEN,
            Outgoing::Take => 1,
        }
    }
}

/// What waits for the next stream, bounded by weight.
#[derive(Debug)]
struct Outbox {
    items: VecDeque<Outgoing>,
    weight: usize,
    cap: usize,
}

impl Outbox {
    fn new(cap: usize) -> Self {
        Self {
            items: VecDeque::new(),
            weight: 0,
            cap,
        }
    }

    /// Queue `item`, dropping the oldest until the outbox is within its
    /// cap again — `item` itself, when it alone is over. Returns how
    /// many were dropped.
    fn push(&mut self, item: Outgoing) -> usize {
        self.weight += item.weight();
        self.items.push_back(item);
        let mut dropped = 0;
        while self.weight > self.cap {
            let Some(oldest) = self.items.pop_front() else {
                break;
            };
            self.weight -= oldest.weight();
            dropped += 1;
        }
        dropped
    }

    fn drain(&mut self) -> impl Iterator<Item = Outgoing> + '_ {
        self.weight = 0;
        self.items.drain(..)
    }
}

/// How far a stream's resume point can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resumable {
    /// A snapshot stream whose consumer has not said it holds all of it.
    Pending,
    Armed,
    /// A grid moved, or the stream was not trustworthy. Never re-armed:
    /// a resume across a grid change replays output for a width the
    /// terminal is no longer at.
    Lost,
}

/// One accepted stream: the checks every server frame passes through.
#[derive(Debug)]
struct Stream {
    kind: PayloadKind,
    session_id: String,
    server_epoch: u64,
    tab_generation: u64,
    resumed: bool,
    /// The seq the next `PTY` frame must carry.
    next_seq: u64,
    /// The tab's grid as the consumer's terminal last heard it.
    grid: Grid,
    resumable: Resumable,
    vt_finished: bool,
}

/// What one server frame means.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Emit(AttachEvent),
    Quiet,
    Exited(i32),
    /// Re-dial. `keep_resume`: the frames so far were sound, so the
    /// point they reached is still one to resume from.
    Fault {
        reason: String,
        keep_resume: bool,
    },
    Ended(String),
}

impl Stream {
    fn new(accepted: &AttachAccepted, kind: PayloadKind, session_id: &str) -> Self {
        let resumed = accepted.mode == AttachMode::Resume;
        Self {
            kind,
            session_id: session_id.to_string(),
            server_epoch: accepted.server_epoch,
            tab_generation: accepted.tab_generation,
            resumed,
            next_seq: accepted.seq.saturating_add(1),
            grid: accepted_grid(accepted),
            resumable: if resumed {
                Resumable::Armed
            } else {
                Resumable::Pending
            },
            vt_finished: false,
        }
    }

    fn point(&self) -> ResumePoint {
        ResumePoint {
            session_id: self.session_id.clone(),
            server_epoch: self.server_epoch,
            tab_generation: self.tab_generation,
            next_seq: self.next_seq,
            grid: self.grid,
        }
    }

    fn hydrated(&mut self) {
        if self.resumable == Resumable::Pending {
            self.resumable = Resumable::Armed;
        }
    }

    fn lose_resume(&mut self) {
        self.resumable = Resumable::Lost;
    }

    fn on_frame(&mut self, frame: ServerFrame) -> Step {
        let fault = |reason: String| Step::Fault {
            reason,
            keep_resume: false,
        };
        match frame {
            ServerFrame::Snap(bytes) => {
                if self.resumed {
                    return fault("a SNAP frame on a resumed stream".into());
                }
                if self.kind == PayloadKind::Vt {
                    if self.vt_finished && !bytes.is_empty() {
                        return fault("a SNAP frame after the vt terminator".into());
                    }
                    self.vt_finished |= bytes.is_empty();
                } else if bytes.is_empty() {
                    return Step::Quiet;
                }
                Step::Emit(AttachEvent::Snap(bytes))
            }
            ServerFrame::Pty { seq, bytes } => {
                if seq != self.next_seq {
                    return fault(format!("PTY seq {seq} where {} was next", self.next_seq));
                }
                self.next_seq = self.next_seq.saturating_add(1);
                Step::Emit(AttachEvent::Pty(bytes))
            }
            ServerFrame::Grid(grid) => self.on_grid(grid),
            ServerFrame::Exit { final_seq, code } => {
                if final_seq != self.next_seq {
                    tracing::debug!(final_seq, next = self.next_seq, "EXIT behind missing PTY");
                }
                Step::Exited(code)
            }
            ServerFrame::Error(error) => match ServerCode::from(&error) {
                ServerCode::ShuttingDown => {
                    Step::Ended(format!("the session is shutting down: {}", error.message))
                }
                code @ (ServerCode::Desync | ServerCode::Overflow) => Step::Fault {
                    reason: format!("{}: {}", code.as_str(), error.message),
                    keep_resume: true,
                },
                code => fault(format!("{}: {}", code.as_str(), error.message)),
            },
        }
    }

    /// A `GRID` applies after `PTY` frame `seq`, so it must name the one
    /// just handed over.
    fn on_grid(&mut self, grid: GridFrame) -> Step {
        if grid.seq.saturating_add(1) != self.next_seq {
            return Step::Fault {
                reason: format!(
                    "GRID placed after seq {} where {} was the last",
                    grid.seq,
                    self.next_seq.saturating_sub(1)
                ),
                keep_resume: false,
            };
        }
        self.grid = Grid {
            cols: grid.cols,
            rows: grid.rows,
            cell_w_px: grid.cell_w_px,
            cell_h_px: grid.cell_h_px,
        };
        self.lose_resume();
        Step::Emit(AttachEvent::Grid {
            cols: grid.cols,
            rows: grid.rows,
            cell_w_px: grid.cell_w_px,
            cell_h_px: grid.cell_h_px,
            owner: grid.owner,
        })
    }

    /// Hand the attach what this stream leaves behind for the next one.
    fn retire(self, attach: &mut Attach) {
        let point = self.point();
        (attach.resume, attach.unconfirmed) = match self.resumable {
            Resumable::Armed => (Some(point), None),
            Resumable::Pending => (None, Some(point)),
            Resumable::Lost => (None, None),
        };
    }
}

fn accepted_grid(accepted: &AttachAccepted) -> Grid {
    Grid {
        cols: accepted.snapshot_cols,
        rows: accepted.snapshot_rows,
        cell_w_px: accepted.snapshot_cell_w_px,
        cell_h_px: accepted.snapshot_cell_h_px,
    }
}

/// How one attempt ended.
#[derive(Debug)]
enum Outcome {
    Detached,
    Ended(String),
    Exited,
    /// Re-dial after the backoff. `live`: a stream was accepted first.
    Retry {
        reason: String,
        live: bool,
    },
    /// Re-dial once the host has a different session: this one is not
    /// it, or the transport needs the host connection to recover first.
    NewSession {
        reason: String,
        live: bool,
    },
}

async fn run(
    mut attach: Attach,
    dialer: Arc<dyn Dialer>,
    mut session: watch::Receiver<Option<SessionFacts>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<AttachEvent>,
) {
    let emit = |event: AttachEvent| {
        let _ = events.send(event);
    };
    let mut backoff = Backoff::new(BACKOFF_BASE, BACKOFF_CAP);
    let mut announced = false;
    let mut await_new_session = false;
    let reason = loop {
        if std::mem::take(&mut await_new_session) {
            match idle(&mut attach, &mut commands, session.changed()).await {
                None => break "detached".to_string(),
                Some(Err(_)) => break "the host connection is gone".to_string(),
                Some(Ok(())) => {}
            }
        }
        let facts = loop {
            if let Some(facts) = session.borrow_and_update().clone() {
                break Ok(facts);
            }
            match idle(&mut attach, &mut commands, session.changed()).await {
                None => break Err("detached"),
                Some(Err(_)) => break Err("the host connection is gone"),
                Some(Ok(())) => {}
            }
        };
        let facts = match facts {
            Ok(facts) => facts,
            Err(reason) => break reason.to_string(),
        };
        attach.forget_other_sessions(&facts.session_id);
        let (reason, live, new_session) =
            match attempt(&mut attach, &*dialer, &facts, &mut commands, &emit).await {
                Outcome::Detached => break "detached".to_string(),
                Outcome::Ended(reason) => break reason,
                Outcome::Exited => return,
                Outcome::Retry { reason, live } => (reason, live, false),
                Outcome::NewSession { reason, live } => (reason, live, true),
            };
        tracing::debug!(tab = attach.tab_id, %reason, "attach dropped; re-dialing");
        if live {
            backoff.reset();
            announced = false;
        }
        if !announced {
            emit(AttachEvent::Reconnecting { reason });
            announced = true;
        }
        if new_session {
            await_new_session = true;
            continue;
        }
        let delay = backoff.next_delay(jitter());
        if idle(&mut attach, &mut commands, tokio::time::sleep(delay))
            .await
            .is_none()
        {
            break "detached".to_string();
        }
    };
    emit(AttachEvent::Ended { reason });
}

/// Drive `work` while holding every command that arrives for the next
/// stream. `None` when the caller detached first.
async fn idle<T>(
    attach: &mut Attach,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            out = &mut work => return Some(out),
            command = commands.recv() => {
                if !command.is_some_and(|command| attach.absorb(command)) {
                    return None;
                }
            }
        }
    }
}

/// Resolves when the caller detaches, holding every other command for
/// the stream this handshake is opening.
async fn until_detached(attach: &mut Attach, commands: &mut mpsc::UnboundedReceiver<Command>) {
    while let Some(command) = commands.recv().await {
        if !attach.absorb(command) {
            return;
        }
    }
}

/// One dial: the handshake, then the stream until it ends.
async fn attempt(
    attach: &mut Attach,
    dialer: &dyn Dialer,
    facts: &SessionFacts,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    emit: &impl Fn(AttachEvent),
) -> Outcome {
    let handshake = attach.handshake(facts);
    let offered = attach.resume.clone();
    let retry = |reason: String| Outcome::Retry {
        reason,
        live: false,
    };
    let duplex = match idle(attach, commands, tokio::time::timeout(leg(), dialer.dial())).await {
        None => return Outcome::Detached,
        Some(Err(_)) => return retry(format!("dialing {} timed out", dialer.describe())),
        Some(Ok(Err(error))) if error.retryable() => return retry(error.to_string()),
        Some(Ok(Err(error))) => {
            return Outcome::NewSession {
                reason: error.to_string(),
                live: false,
            }
        }
        Some(Ok(Ok(duplex))) => duplex,
    };
    let shaken = tokio::time::timeout(
        leg(),
        DataConnection::<BoxRead, BoxWrite>::handshake_unless(
            duplex.read,
            duplex.write,
            &handshake,
            until_detached(attach, commands),
            |_| {},
        ),
    )
    .await;
    let (accepted, conn) = match shaken {
        Err(_) => return retry("the attach handshake timed out".into()),
        Ok(Ok(None)) => return Outcome::Detached,
        Ok(Ok(Some(accepted))) => accepted,
        Ok(Err(error)) => return refused(&error),
    };
    let (reader, mut writer) = conn.into_split();
    let kind = match accepted.kind.as_str() {
        AttachPayloadKind::GHOSTTY_SNAPSHOT => PayloadKind::GhosttySnapshot,
        AttachPayloadKind::VT => PayloadKind::Vt,
        other => {
            let _ = writer.send_detach().await;
            return Outcome::Ended(format!(
                "the session served a {other} payload, which this client never offered"
            ));
        }
    };
    if accepted.mode == AttachMode::Resume {
        let fault = match (&offered, &attach.resume) {
            (None, _) => Some("the session resumed a stream this attach never offered"),
            (Some(_), None) => Some("the resume point was given up while the handshake was out"),
            (Some(point), Some(_)) if point.grid != accepted_grid(&accepted) => {
                Some("the session resumed at a grid the terminal is not at")
            }
            _ => None,
        };
        if let Some(reason) = fault {
            attach.resume = None;
            let _ = writer.send_detach().await;
            return retry(reason.into());
        }
    }
    attach.take = false;
    attach.resume = None;
    attach.unconfirmed = None;
    emit(AttachEvent::Accepted {
        kind,
        mode: accepted.mode,
        cols: accepted.snapshot_cols,
        rows: accepted.snapshot_rows,
        cell_w_px: accepted.snapshot_cell_w_px,
        cell_h_px: accepted.snapshot_cell_h_px,
        owner: accepted.owner.clone(),
    });
    let mut stream = Stream::new(&accepted, kind, &facts.session_id);
    let outcome = live(attach, &mut stream, reader, writer, commands, emit).await;
    stream.retire(attach);
    match outcome {
        Outcome::Retry { reason, .. } => Outcome::Retry { reason, live: true },
        Outcome::NewSession { reason, .. } => Outcome::NewSession { reason, live: true },
        other => other,
    }
}

/// What a refused handshake means. The session refusing by name is
/// final for a tab that is gone and for terms no retry changes; a
/// session that is not the one the host is connected to waits for the
/// host to reconnect; the rest is worth another dial.
fn refused(error: &ClientError) -> Outcome {
    let reason = error.to_string();
    match error.server_code() {
        Some(
            ServerCode::NotFound
            | ServerCode::ShuttingDown
            | ServerCode::BuildMismatch
            | ServerCode::UnsupportedKind
            | ServerCode::ProtocolMismatch
            | ServerCode::InvalidParam
            | ServerCode::ParseError
            | ServerCode::NotSupported,
        ) => Outcome::Ended(reason),
        Some(ServerCode::SessionMismatch) => Outcome::NewSession {
            reason,
            live: false,
        },
        _ => Outcome::Retry {
            reason,
            live: false,
        },
    }
}

/// An accepted stream: flush what waited for it, then frames one way and
/// commands the other until it ends.
///
/// Only the reader decides that the stream is over. A failed write parks
/// the writer and later commands queue for the next stream: the session
/// labels its closes, and ending on the write would lose the label.
async fn live(
    attach: &mut Attach,
    stream: &mut Stream,
    mut reader: DataFrameReader<BoxRead>,
    mut writer: DataWriter<BoxWrite>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    emit: &impl Fn(AttachEvent),
) -> Outcome {
    let mut writer_ok = true;
    for item in attach.outbox.drain() {
        if writer_ok {
            writer_ok = write(&mut writer, item, stream).await;
        }
    }
    loop {
        tokio::select! {
            frame = reader.next_frame() => {
                let frame = match frame {
                    Ok(Some(frame)) => frame,
                    Ok(None) => return Outcome::Retry {
                        reason: "the data connection closed".into(),
                        live: true,
                    },
                    Err(error) => return Outcome::Retry {
                        reason: format!("the data connection failed: {error}"),
                        live: true,
                    },
                };
                let step = match ServerFrame::decode(frame) {
                    Ok(frame) => stream.on_frame(frame),
                    Err(error) => Step::Fault {
                        reason: error.to_string(),
                        keep_resume: false,
                    },
                };
                match step {
                    Step::Emit(event) => emit(event),
                    Step::Quiet => {}
                    Step::Exited(code) => {
                        emit(AttachEvent::Exit { code });
                        return Outcome::Exited;
                    }
                    Step::Fault { reason, keep_resume } => {
                        if !keep_resume {
                            stream.lose_resume();
                        }
                        return Outcome::Retry { reason, live: true };
                    }
                    Step::Ended(reason) => return Outcome::Ended(reason),
                }
            }
            command = commands.recv() => {
                let command = match command {
                    None | Some(Command::Detach) => {
                        if writer_ok {
                            let _ = writer.send_detach().await;
                        }
                        return Outcome::Detached;
                    }
                    Some(Command::ForgetResume) => {
                        stream.lose_resume();
                        continue;
                    }
                    Some(Command::Hydrated) => {
                        stream.hydrated();
                        continue;
                    }
                    Some(command) => command,
                };
                if !writer_ok {
                    attach.absorb(command);
                    continue;
                }
                let item = match command {
                    Command::Input(bytes, take) => Outgoing::Input(bytes, take),
                    Command::Resize(grid, focused) => {
                        attach.grid = grid;
                        stream.lose_resume();
                        writer_ok = writer
                            .send_resize(grid.cols, grid.rows, grid.cell_w_px, grid.cell_h_px, focused)
                            .await
                            .is_ok();
                        continue;
                    }
                    Command::Take => Outgoing::Take,
                    Command::ForgetResume | Command::Hydrated | Command::Detach => continue,
                };
                writer_ok = write(&mut writer, item, stream).await;
            }
        }
    }
}

/// Write one outgoing item. `false` when the write failed.
async fn write(writer: &mut DataWriter<BoxWrite>, item: Outgoing, stream: &mut Stream) -> bool {
    let written = match item {
        Outgoing::Input(bytes, take) => writer.send_input(&bytes, take).await,
        Outgoing::Resize(grid) => {
            stream.lose_resume();
            writer
                .send_resize(grid.cols, grid.rows, grid.cell_w_px, grid.cell_h_px, true)
                .await
        }
        Outgoing::Take => writer.send_take().await,
    };
    if let Err(error) = &written {
        tracing::debug!(%error, "data connection write failed; the reader decides what is next");
    }
    written.is_ok()
}

#[cfg(test)]
mod tests {
    use roost_ipc::dataframe::{
        write_data_frame, DataFrame, FRAME_INPUT, FRAME_PTY, FRAME_RESIZE, INPUT_TAKE_BEFORE,
        MAX_DATA_FRAME_BYTES, PREAMBLE,
    };
    use roost_ipc::framing::{write_frame, FrameReader};
    use roost_ipc::messages::{AttachHandshakeReply, ResponseError};

    use super::*;
    use crate::dial::{DialError, Duplex};

    const GRID: Grid = Grid {
        cols: 80,
        rows: 24,
        cell_w_px: 9,
        cell_h_px: 18,
    };

    fn accepted(mode: AttachMode, seq: u64) -> AttachAccepted {
        AttachAccepted {
            kind: AttachPayloadKind::from(AttachPayloadKind::VT),
            mode,
            seq,
            server_epoch: 7,
            tab_generation: 2,
            snapshot_cols: GRID.cols,
            snapshot_rows: GRID.rows,
            snapshot_cell_w_px: GRID.cell_w_px,
            snapshot_cell_h_px: GRID.cell_h_px,
            owner: None,
        }
    }

    fn stream(mode: AttachMode, seq: u64) -> Stream {
        Stream::new(&accepted(mode, seq), PayloadKind::Vt, "s1")
    }

    fn pty(seq: u64) -> ServerFrame {
        ServerFrame::Pty {
            seq,
            bytes: b"x".to_vec(),
        }
    }

    fn grid_at(seq: u64) -> ServerFrame {
        ServerFrame::Grid(GridFrame {
            seq,
            cols: 100,
            rows: 30,
            cell_w_px: 9,
            cell_h_px: 18,
            owner: None,
        })
    }

    fn faulted(step: &Step) -> bool {
        matches!(step, Step::Fault { .. })
    }

    #[test]
    fn pty_seqs_must_run_on_from_the_fence() {
        let mut s = stream(AttachMode::Snapshot, 41);
        assert_eq!(
            s.on_frame(pty(42)),
            Step::Emit(AttachEvent::Pty(b"x".to_vec()))
        );
        assert!(faulted(&s.on_frame(pty(44))), "a gap");
        let mut s = stream(AttachMode::Snapshot, 41);
        s.on_frame(pty(42));
        assert!(faulted(&s.on_frame(pty(42))), "a duplicate");
    }

    #[test]
    fn a_grid_lands_only_after_the_last_pty_handed_over() {
        let mut s = stream(AttachMode::Snapshot, 41);
        assert!(matches!(
            s.on_frame(grid_at(41)),
            Step::Emit(AttachEvent::Grid { cols: 100, .. })
        ));
        s.on_frame(pty(42));
        assert!(faulted(&s.on_frame(grid_at(41))), "behind the last PTY");
        assert!(
            faulted(&stream(AttachMode::Snapshot, 41).on_frame(grid_at(42))),
            "ahead"
        );
    }

    #[test]
    fn the_vt_terminator_ends_the_payload_and_nothing_follows_it() {
        let mut s = stream(AttachMode::Snapshot, 0);
        assert_eq!(
            s.on_frame(ServerFrame::Snap(b"a".to_vec())),
            Step::Emit(AttachEvent::Snap(b"a".to_vec()))
        );
        assert_eq!(
            s.on_frame(ServerFrame::Snap(Vec::new())),
            Step::Emit(AttachEvent::Snap(Vec::new()))
        );
        assert!(faulted(&s.on_frame(ServerFrame::Snap(b"b".to_vec()))));
        assert!(
            faulted(&stream(AttachMode::Resume, 0).on_frame(ServerFrame::Snap(b"a".to_vec()))),
            "a resume sends no snapshot"
        );
        let mut ghostty = Stream::new(
            &accepted(AttachMode::Snapshot, 0),
            PayloadKind::GhosttySnapshot,
            "s1",
        );
        assert_eq!(ghostty.on_frame(ServerFrame::Snap(Vec::new())), Step::Quiet);
    }

    #[test]
    fn error_frames_split_into_resume_retry_and_end() {
        let error = |code: &str| {
            ServerFrame::Error(ResponseError {
                code: code.into(),
                message: "m".into(),
            })
        };
        let mut s = stream(AttachMode::Snapshot, 0);
        assert!(matches!(
            s.on_frame(error("desync")),
            Step::Fault {
                keep_resume: true,
                ..
            }
        ));
        assert!(matches!(
            s.on_frame(error("overflow")),
            Step::Fault {
                keep_resume: true,
                ..
            }
        ));
        assert!(matches!(
            s.on_frame(error("protocol-error")),
            Step::Fault {
                keep_resume: false,
                ..
            }
        ));
        assert!(matches!(s.on_frame(error("shutting-down")), Step::Ended(_)));
    }

    fn fresh_attach() -> Attach {
        Attach {
            tab_id: 5,
            who: Who {
                client_id: "c".into(),
                libghostty_build: "b".into(),
            },
            grid: GRID,
            take: true,
            resume: None,
            unconfirmed: None,
            outbox: Outbox::new(OUTBOX_CAP_BYTES),
        }
    }

    #[test]
    fn a_snapshot_stream_resumes_only_once_hydrated_and_never_after_a_grid() {
        let retired = |s: Stream| {
            let mut attach = fresh_attach();
            s.retire(&mut attach);
            attach
        };

        let mut s = stream(AttachMode::Snapshot, 41);
        s.on_frame(pty(42));
        let attach = retired(s);
        assert_eq!(attach.resume, None, "not hydrated: no resume");
        assert_eq!(attach.unconfirmed.as_ref().map(|p| p.next_seq), Some(43));

        let mut s = stream(AttachMode::Snapshot, 41);
        s.hydrated();
        s.on_frame(pty(42));
        let point = retired(s).resume.expect("hydrated: resumable");
        assert_eq!(
            (point.server_epoch, point.tab_generation, point.next_seq),
            (7, 2, 43)
        );

        let mut s = stream(AttachMode::Snapshot, 41);
        s.hydrated();
        s.on_frame(grid_at(41));
        s.hydrated();
        let attach = retired(s);
        assert_eq!(
            (attach.resume, attach.unconfirmed),
            (None, None),
            "a grid change is final"
        );

        let mut s = stream(AttachMode::Resume, 41);
        s.lose_resume();
        assert_eq!(retired(s).resume, None, "a forgotten or resized stream");
        assert!(
            retired(stream(AttachMode::Resume, 41)).resume.is_some(),
            "a resume stays resumable"
        );
    }

    #[test]
    fn another_sessions_points_are_never_offered() {
        let mut attach = fresh_attach();
        stream(AttachMode::Resume, 9).retire(&mut attach);
        attach.forget_other_sessions("s1");
        assert!(attach.resume.is_some(), "the same session keeps its point");
        attach.forget_other_sessions("s2");
        assert_eq!(attach.resume, None);
        stream(AttachMode::Snapshot, 9).retire(&mut attach);
        attach.forget_other_sessions("s2");
        assert!(attach.absorb(Command::Hydrated));
        assert_eq!(attach.resume, None, "nor a dropped snapshot's");
    }

    #[test]
    fn a_late_hydrated_confirms_the_dropped_streams_point() {
        let mut attach = fresh_attach();
        stream(AttachMode::Snapshot, 9).retire(&mut attach);
        assert!(attach.absorb(Command::Hydrated));
        assert_eq!(attach.resume.map(|p| p.next_seq), Some(10));
    }

    #[test]
    fn the_handshake_offers_the_resume_triple_only_when_it_holds_one() {
        let facts = SessionFacts {
            session_id: "s1".into(),
            libghostty_build: "b".into(),
            payload_kinds: vec![],
            app_version: "0".into(),
        };
        let mut attach = fresh_attach();
        let line = attach.handshake(&facts);
        assert_eq!((line.resume_from_seq, line.terms.take), (None, true));
        assert_eq!(
            line.terms.kinds,
            vec![
                AttachPayloadKind::from("ghostty-snapshot"),
                AttachPayloadKind::from("vt")
            ]
        );
        let mut s = stream(AttachMode::Resume, 99);
        s.on_frame(pty(100));
        s.retire(&mut attach);
        let line = attach.handshake(&facts);
        assert_eq!(
            (line.resume_from_seq, line.server_epoch, line.tab_generation),
            (Some(101), Some(7), Some(2))
        );
    }

    #[test]
    fn the_outbox_drops_the_oldest_past_its_cap() {
        let mut outbox = Outbox::new(10);
        assert_eq!(
            outbox.push(Outgoing::Input(vec![1; 4], InputTake::Before)),
            0
        );
        assert_eq!(
            outbox.push(Outgoing::Input(vec![2; 4], InputTake::Before)),
            0
        );
        assert_eq!(
            outbox.push(Outgoing::Input(vec![3; 4], InputTake::Before)),
            1
        );
        assert_eq!(outbox.weight, 8);
        assert_eq!(
            outbox.push(Outgoing::Input(vec![4; 11], InputTake::None)),
            3,
            "too big alone"
        );
        assert_eq!((outbox.items.len(), outbox.weight), (0, 0));
        outbox.push(Outgoing::Take);
        assert_eq!(outbox.drain().collect::<Vec<_>>(), vec![Outgoing::Take]);
        assert_eq!(outbox.weight, 0);
    }

    #[test]
    fn an_unfocused_resize_while_reconnecting_only_moves_the_handshake() {
        let mut attach = fresh_attach();
        let wider = Grid { cols: 120, ..GRID };
        attach.absorb(Command::Resize(wider, false));
        assert!(attach.outbox.items.is_empty());
        assert_eq!(attach.grid, wider);
        attach.absorb(Command::Resize(GRID, true));
        assert_eq!(attach.outbox.items.back(), Some(&Outgoing::Resize(GRID)));
    }

    /// A dialer whose every dial is one end of an in-memory pipe, the
    /// other end handed to the test.
    struct PipeDialer(mpsc::UnboundedSender<tokio::io::DuplexStream>);

    impl Dialer for PipeDialer {
        fn dial(&self) -> futures::future::BoxFuture<'_, Result<Duplex, DialError>> {
            let (ours, theirs) = tokio::io::duplex(4 * 1024 * 1024);
            let sent = self.0.send(theirs);
            Box::pin(async move {
                sent.map_err(|_| DialError::Io(std::io::ErrorKind::NotConnected.into()))?;
                let (read, write) = tokio::io::split(ours);
                Ok(Duplex {
                    read: Box::new(read),
                    write: Box::new(write),
                })
            })
        }

        fn describe(&self) -> String {
            "pipe".into()
        }
    }

    type ServerRead = tokio::io::ReadHalf<tokio::io::DuplexStream>;
    type ServerWrite = tokio::io::WriteHalf<tokio::io::DuplexStream>;

    /// Accept one dial as the session would: read the handshake line,
    /// answer `reply`, write the preamble, and hand back the binary
    /// halves.
    async fn accept(
        dials: &mut mpsc::UnboundedReceiver<tokio::io::DuplexStream>,
        reply: AttachAccepted,
    ) -> (serde_json::Value, DataFrameReader<ServerRead>, ServerWrite) {
        let conn = dials.recv().await.expect("a dial");
        let (read, mut write) = tokio::io::split(conn);
        let mut lines = FrameReader::new(read);
        let line = lines.read_line().await.expect("read").expect("a handshake");
        let reply = serde_json::to_vec(&AttachHandshakeReply::Accepted(reply)).unwrap();
        write_frame(&mut write, &reply).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut write, &PREAMBLE)
            .await
            .unwrap();
        let (read, residue) = lines.into_parts();
        (
            serde_json::from_slice(&line).unwrap(),
            DataFrameReader::new(read, residue),
            write,
        )
    }

    async fn next_event(events: &mut mpsc::UnboundedReceiver<AttachEvent>) -> AttachEvent {
        tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("an event in time")
            .expect("the channel open")
    }

    async fn next_frame(reader: &mut DataFrameReader<ServerRead>) -> DataFrame {
        tokio::time::timeout(Duration::from_secs(10), reader.next_frame())
            .await
            .expect("a frame in time")
            .expect("read")
            .expect("not EOF")
    }

    fn facts() -> watch::Sender<Option<SessionFacts>> {
        watch::Sender::new(Some(SessionFacts {
            session_id: "s1".into(),
            libghostty_build: "b".into(),
            payload_kinds: vec!["vt".into()],
            app_version: "0".into(),
        }))
    }

    /// Input typed while the stream is down waits for the next one and
    /// goes out in order behind its handshake, the re-dial resumes from
    /// where the hydrated stream stopped, and a paste past the frame cap
    /// is split with its take on the first frame only.
    #[tokio::test]
    async fn input_held_across_a_drop_flushes_in_order_after_the_resume() {
        let (dials_tx, mut dials) = mpsc::unbounded_channel();
        let session = facts();
        let req = AttachRequest {
            tab_id: 5,
            cols: GRID.cols,
            rows: GRID.rows,
            cell_w_px: GRID.cell_w_px,
            cell_h_px: GRID.cell_h_px,
            take: true,
        };
        let who = Who {
            client_id: "c".into(),
            libghostty_build: "b".into(),
        };
        let (stream, mut events) = spawn_with(
            &tokio::runtime::Handle::current(),
            Arc::new(PipeDialer(dials_tx)),
            who,
            session.subscribe(),
            req,
        );

        let (line, first_reader, mut write) =
            accept(&mut dials, accepted(AttachMode::Snapshot, 3)).await;
        assert_eq!(
            (line["take"].as_bool(), line.get("resume_from_seq")),
            (Some(true), None)
        );
        assert!(matches!(
            next_event(&mut events).await,
            AttachEvent::Accepted {
                mode: AttachMode::Snapshot,
                ..
            }
        ));
        write_data_frame(&mut write, roost_ipc::dataframe::FRAME_SNAP, &[])
            .await
            .unwrap();
        assert_eq!(next_event(&mut events).await, AttachEvent::Snap(Vec::new()));
        stream.hydrated();
        let mut seq4 = 4u64.to_le_bytes().to_vec();
        seq4.push(b'a');
        write_data_frame(&mut write, FRAME_PTY, &seq4)
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            AttachEvent::Pty(b"a".to_vec())
        );
        drop((first_reader, write));
        assert!(matches!(
            next_event(&mut events).await,
            AttachEvent::Reconnecting { .. }
        ));

        stream.input(b"k".to_vec(), InputTake::None);
        stream.input(b"ls\r".to_vec(), InputTake::Before);
        stream.resize(GRID.cols, GRID.rows, GRID.cell_w_px, GRID.cell_h_px, true);

        let (line, mut reader, _write) = accept(&mut dials, accepted(AttachMode::Resume, 4)).await;
        assert_eq!(
            line["take"].as_bool(),
            Some(false),
            "a re-dial is automatic"
        );
        assert_eq!(
            (
                line["resume_from_seq"].as_u64(),
                line["server_epoch"].as_u64()
            ),
            (Some(5), Some(7))
        );
        assert!(matches!(
            next_event(&mut events).await,
            AttachEvent::Accepted {
                mode: AttachMode::Resume,
                ..
            }
        ));
        let mut read_back = Vec::new();
        for _ in 0..3 {
            let frame = next_frame(&mut reader).await;
            read_back.push((frame.frame_type, frame.payload));
        }
        assert_eq!(
            read_back,
            vec![
                (FRAME_INPUT, vec![0, b'k']),
                (FRAME_INPUT, vec![INPUT_TAKE_BEFORE, b'l', b's', b'\r']),
                (FRAME_RESIZE, vec![80, 0, 24, 0, 9, 0, 18, 0, 1]),
            ]
        );

        let paste = vec![b'p'; MAX_DATA_FRAME_BYTES + 10];
        stream.input(paste.clone(), InputTake::Before);
        let head = next_frame(&mut reader).await;
        assert_eq!(
            (head.frame_type, head.payload[0]),
            (FRAME_INPUT, INPUT_TAKE_BEFORE)
        );
        let tail = next_frame(&mut reader).await;
        assert_eq!((tail.frame_type, tail.payload[0]), (FRAME_INPUT, 0));
        assert_eq!(head.payload.len() + tail.payload.len() - 2, paste.len());
        stream.detach();
        assert_eq!(
            next_frame(&mut reader).await.frame_type,
            roost_ipc::dataframe::FRAME_DETACH
        );
        assert_eq!(
            next_event(&mut events).await,
            AttachEvent::Ended {
                reason: "detached".into()
            }
        );
    }
}
