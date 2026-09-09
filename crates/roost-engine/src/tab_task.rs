//! The per-tab server-VT task: one authority per tab for output, input,
//! resize, snapshot, replies and exit (host sessions, plan 036 D2/D3;
//! `discovery/host-sessions-architecture.md` §3).
//!
//! Without this module a tab's bytes go straight from the blocking PTY
//! reader onto a lossy `broadcast` and whoever is listening owns the
//! terminal. That is fine when the only listener is a UI with its own
//! `Terminal`; it is not fine when the *server* holds the authoritative
//! terminal, because a lagged subscriber would leave that terminal
//! missing bytes forever. So the reader feeds a **bounded** channel and
//! this task drains it synchronously:
//!
//! ```text
//! pty_reader_loop ─ mpsc(32) ─► tab task ─┬─► server Terminal (vt_write)
//!                                         ├─► OSC scan → workspace
//!                                         ├─► reply drain → PTY writer
//!                                         ├─► tee broadcast (seq'd)
//!                                         └─► replay ring (2 MiB)
//! ```
//!
//! The bound is the design, not a limit to raise: when the task falls
//! behind, the blocking read stalls, the kernel PTY buffer fills and the
//! child blocks on `write` — terminal flow control, for free. Unbounded
//! would turn a runaway child into unbounded memory in a process meant to
//! run for weeks.
//!
//! # Availability vs. behavior
//!
//! Everything here is behind the `server-vt` cargo feature, but a
//! workspace build unifies features across the graph — a UI binary can
//! link this code. What switches the pipeline on is the runtime opt-in
//! [`crate::PtySupervisor::enable_server_vt`], which only `roost-session`
//! calls. With the flag off, `spawn` builds no tab task and the reader
//! feeds the publisher exactly as it always has.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use portable_pty::PtySize;
use roost_ipc::messages::{AttachPayloadKind, ClipboardEffectTarget, CLIPBOARD_EFFECT_MAX_BYTES};
use roost_vt::{Cell, Colors, CursorInfo, RenderState, RenderedRow, Terminal, TerminalOptions};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};
use tracing::{debug, warn};

use crate::ipc::{DumpData, ResolvedCellData, ResolvedCellsData};
use crate::osc::{ClipboardTarget, OscAction, OscColorSnapshot, OscColorState, OscRgb, OscRouter};
use crate::pty::{Geometry, PtyOutputEvent, WriterCmd};
use crate::workspace::TabEffectKind;

/// Scrollback the server Terminal retains, matching both UIs' policy.
pub const SERVER_VT_SCROLLBACK: usize = 2000;
/// Byte cap on the replay-safe VT continuation libghostty retains for an
/// unfinished sequence. Non-zero from construction because
/// `Terminal::snapshot` only works when tracking was already on when the
/// input that left the parser mid-sequence arrived.
pub const SERVER_VT_CONTINUATION_MAX: usize = 1024 * 1024;
/// Depth of the reader → tab-task byte channel, in chunks (4 KiB each).
pub const TAB_CHANNEL_CHUNKS: usize = 32;
/// Depth of the tab task's command channel.
pub const TAB_CMD_CAPACITY: usize = 64;
/// Byte cap on a tab's replay ring; oldest records are evicted first.
pub const REPLAY_RING_BYTES: usize = 2 * 1024 * 1024;
/// Byte cap on what the task holds for the PTY writer when the writer
/// channel is full. Covers terminal replies AND client input: they share
/// one FIFO so the child sees them in the order the task produced them.
pub const REPLY_PENDING_MAX: usize = 64 * 1024;
/// How many tabs may be encoding a snapshot at once, session-wide.
pub const MAX_CONCURRENT_SNAPSHOTS: usize = 4;
/// How often the task retries a PTY write it could not hand off because
/// the writer channel was full.
const PENDING_FLUSH_RETRY: std::time::Duration = std::time::Duration::from_millis(10);
/// How long an acked resize waits for the child's `TIOCSWINSZ`.
///
/// Bounded because the winsize shares one FIFO with client input (#80),
/// and a child that has stopped reading can hold that FIFO open
/// indefinitely — an unbounded wait would turn a wedged shell into a
/// `tab.attach` that never answers. Generous because everything short of
/// that case is one `ioctl` away.
const WINSIZE_ACK_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
/// Why a parked `vt` encode gives up: nothing more will be ingested, so
/// the parser can never leave the sequence it is stuck inside.
const EXITED_MID_SEQUENCE: &str =
    "the tab exited while its parser was inside an unfinished sequence too long to carry";

/// The headless default theme. libghostty leaves fg/bg unset until
/// something pushes them, and both the OSC color seed and the dump's
/// default-cell colors read them back — so the tab task pushes these at
/// construction and the terminal stays the one source of truth. White on
/// black states "no theme" honestly rather than inventing one.
const HEADLESS_FG: roost_vt::ColorRgb = roost_vt::ColorRgb {
    r: 0xff,
    g: 0xff,
    b: 0xff,
};
const HEADLESS_BG: roost_vt::ColorRgb = roost_vt::ColorRgb { r: 0, g: 0, b: 0 };

/// The workspace-facing seam the tab task applies OSC transitions and row
/// closes through.
///
/// A trait rather than a `LocalClient` for two reasons: the client holds
/// an `Arc<PtySupervisor>`, so storing one *on* the supervisor would make
/// the pair an unbreakable `Arc` cycle; and a test can observe the calls
/// without standing up a persisted workspace.
pub trait ServerVtWorkspace: Send + Sync + 'static {
    /// Apply one `OscAction::Workspace` — same routing a UI does.
    fn apply_osc(&self, tab_id: i64, command: u32, payload: &str);
    /// The tab's PTY is gone; drop its workspace row.
    fn close_row(&self, tab_id: i64);
    /// Publish one client-local effect (plan 037 §3.6) — a bell or an
    /// OSC 52 clipboard write. A session has no view of its own, so
    /// this is the only thing it can do with one: hand it to whoever is
    /// attached, on the events stream, in commit order with the state
    /// changes around it. A clipboard write's text is user content: it
    /// goes on the wire and into no log line.
    fn tab_effect(&self, tab_id: i64, effect: TabEffectKind);
}

impl ServerVtWorkspace for crate::Workspace {
    fn apply_osc(&self, tab_id: i64, command: u32, payload: &str) {
        crate::application::apply_osc(self, tab_id, command, payload);
    }

    fn close_row(&self, tab_id: i64) {
        if let Err(error) = crate::Workspace::close_tab(self, tab_id) {
            debug!(tab_id, %error, "tab row was already gone at PTY exit");
        }
    }

    fn tab_effect(&self, tab_id: i64, effect: TabEffectKind) {
        self.publish_tab_effect(tab_id, effect);
    }
}

/// Runtime configuration for [`crate::PtySupervisor::enable_server_vt`].
pub struct ServerVtConfig {
    workspace: Arc<dyn ServerVtWorkspace>,
    capture_pty_input: bool,
}

impl ServerVtConfig {
    pub fn new(workspace: Arc<dyn ServerVtWorkspace>) -> Self {
        Self {
            workspace,
            capture_pty_input: false,
        }
    }

    /// Retain every byte the task queues toward the PTY writer so
    /// `tab.capture_pty_input` can read it back. Off by default: the
    /// buffer only ever grows, so it is a test-mode affordance, gated by
    /// the caller's own `ROOST_TEST_MODE` check rather than by reading
    /// the environment from inside the engine.
    #[must_use]
    pub fn with_input_capture(mut self, capture: bool) -> Self {
        self.capture_pty_input = capture;
        self
    }
}

/// Supervisor-wide server-VT state: the identity every tab's stream is
/// scoped by, plus the encode concurrency bound.
pub(crate) struct ServerVtState {
    workspace: Arc<dyn ServerVtWorkspace>,
    capture_pty_input: bool,
    /// One random value per `enable_server_vt` call. A restarted server
    /// mints a fresh one, which is what makes a stale client stream
    /// unresumable by construction rather than by policy (D6).
    server_epoch: u64,
    next_generation: AtomicU64,
    snapshot_permits: Arc<Semaphore>,
    /// The last theme `session.set_theme` seeded (or `None` while no
    /// client has stated one — the headless white-on-black default
    /// stands), with the generation it was stored at. Session-wide
    /// rather than per-tab because that is what makes it total: a tab
    /// opened after the theme landed is seeded with it at construction,
    /// so there is no second class of tab rendering the client's colors
    /// wrong until the next reseed. The generation is what makes
    /// last-writer-wins real under concurrency: every apply site keeps
    /// the highest generation it has seen and ignores older ones, so
    /// interleaved fan-outs converge on the newest theme instead of on
    /// whichever send happened to land last.
    theme: Mutex<(u64, Option<OscColorSnapshot>)>,
}

impl ServerVtState {
    pub(crate) fn new(config: ServerVtConfig) -> Self {
        Self {
            workspace: config.workspace,
            capture_pty_input: config.capture_pty_input,
            server_epoch: random_epoch(),
            next_generation: AtomicU64::new(1),
            snapshot_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SNAPSHOTS)),
            theme: Mutex::new((0, None)),
        }
    }

    pub(crate) fn server_epoch(&self) -> u64 {
        self.server_epoch
    }

    /// Record the session-wide theme every tab opened from now on is
    /// seeded with, and mint the generation the fan-out carries. Last
    /// writer wins — which writer is last is decided here, under this
    /// lock, not by whose sends happen to land last.
    pub(crate) fn set_theme(&self, seed: OscColorSnapshot) -> u64 {
        let mut theme = self.theme.lock().unwrap_or_else(PoisonError::into_inner);
        theme.0 += 1;
        theme.1 = Some(seed);
        theme.0
    }

    pub(crate) fn theme(&self) -> (u64, Option<OscColorSnapshot>) {
        self.theme
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// 63 bits, not 64: the epoch rides JSON as a bare number, and the Mac
/// mirror's dynamic decode path (`AnyCodable`) tries `Int64` before
/// falling back to lossy `Double` — a top-bit-set epoch would round-trip
/// imprecisely there and break the resume identity check it exists for.
/// 63 random bits collide as never as 64 do.
fn random_epoch() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the OS random source must be available");
    u64::from_le_bytes(bytes) & (i64::MAX as u64)
}

/// A tab's snapshot plus the identity and fence it was taken at. The
/// first PTY frame a client sees after this snapshot carries `seq + 1`.
#[derive(Debug)]
pub struct SnapshotAt {
    pub seq: u64,
    pub server_epoch: u64,
    pub tab_generation: u64,
    pub bytes: Vec<u8>,
    /// The grid the payload was encoded at, read at the encode itself so
    /// it describes what the bytes actually say — an unfocused attach
    /// reports it back, because that snapshot is at the tab's size and
    /// not at the size the client asked for.
    pub cols: u16,
    pub rows: u16,
}

/// The atomic resume handoff (D6): the ring records a client missed and a
/// tee subscription taken in the same instant, so no event can fall
/// between the two.
#[derive(Debug)]
pub struct ResumeAt {
    pub slice: Vec<(u64, Vec<u8>)>,
    pub receiver: broadcast::Receiver<PtyOutputEvent>,
    pub last_assigned: u64,
    /// Set once the tab has exited, so a late resume still ends in EXIT.
    pub stored_exit: Option<(u64, i32)>,
    /// The tab's grid at the handoff, read on the task in the same turn
    /// the slice was cut. A resuming client replays these records into
    /// the terminal it kept, and any other client may have resized the
    /// tab while it was away — so the reply names the size the records
    /// were written for rather than leaving it to guess.
    pub cols: u16,
    pub rows: u16,
}

/// The child half of a resize, still in flight. See
/// [`WriterCmd::Resize`](crate::pty::WriterCmd).
type WinsizeAck = oneshot::Receiver<Result<(), String>>;

/// Wait out the child half of a resize and report what happened to it.
///
/// A dropped sender is **not** a failure: it means the PTY writer task
/// is gone, so there is no child left to tell — which is the state an
/// exited tab sits in while it still serves snapshots, and failing an
/// attach to one would be a worse answer than the truth.
async fn await_winsize(tab_id: i64, ack: WinsizeAck) -> Result<(), TabError> {
    match tokio::time::timeout(WINSIZE_ACK_BUDGET, ack).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(TabError::WinsizeFailed(error)),
        Ok(Err(_dropped)) => {
            debug!(tab_id, "no pty writer left to resize; the child is gone");
            Ok(())
        }
        Err(_elapsed) => Err(TabError::WinsizeFailed(
            "the child did not take the new size within the budget".into(),
        )),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TabError {
    #[error("the tab task is gone")]
    Gone,
    #[error("the child's window size could not be set: {0}")]
    WinsizeFailed(String),
    #[error("snapshot encode failed: {0}")]
    SnapshotFailed(String),
    #[error("reading the tab's screen failed: {0}")]
    Render(String),
    #[error(
        "resume from seq {from_seq} is outside the replay ring \
         (front {front}, last assigned {last_assigned})"
    )]
    RingMiss {
        from_seq: u64,
        front: u64,
        last_assigned: u64,
    },
}

impl From<roost_vt::Error> for TabError {
    fn from(error: roost_vt::Error) -> Self {
        TabError::Render(error.to_string())
    }
}

/// One command on a tab task's channel. Every reply is a `Result` so a
/// caller distinguishes "the tab task answered no" from "the tab is
/// gone" without inventing a sentinel.
pub enum TabCmd {
    /// Client keystrokes / `tab.write`.
    Input {
        data: Vec<u8>,
        /// The sending connection's declared geometry, applied ahead of
        /// the bytes when it differs from the tab's — the PTY is sized
        /// by the last geometry-bearing interaction, and typing is one
        /// (plan 057, R15). The resize rides this same command rather
        /// than a separate send, so it cannot arrive after the keys it
        /// precedes.
        ///
        /// `None` for a control-plane `tab.write`: it carries no
        /// geometry because there is no viewport behind it — a script
        /// that pastes a line is not a client whose window has a size.
        geometry: Option<Geometry>,
    },
    /// Client RESIZE / `tab.resize`.
    ///
    /// `ack` fires once the server terminal AND the PTY winsize have
    /// both been given the new geometry — and reports the failure of
    /// either half, including the child's, which happens on the PTY
    /// writer's own task. `tab.attach` waits on it so the snapshot it
    /// mints a ticket for cannot be encoded at the old size; every other
    /// caller passes `None` and stays fire-and-forget.
    Resize {
        geometry: Geometry,
        ack: Option<oneshot::Sender<Result<(), TabError>>>,
    },
    /// `session.set_theme` — re-seed this tab's terminal (and its color
    /// tracker) with the attached client's palette. Fire-and-forget:
    /// the op's reply reports how many tabs it was sent to, and a tab
    /// that dies mid-flight is one the client is about to lose anyway.
    /// The generation is the store's; the task applies a seed only when
    /// it is newer than the last one it took, so a stale fan-out racing
    /// a fresh one can never roll a terminal's colors back.
    SetTheme(OscColorSnapshot, u64),
    /// Encode this tab's terminal as the attach payload `kind`, fenced
    /// at the last assigned PTY seq. The kind is the one `tab.attach`
    /// negotiated and rides on the attach ticket, so the encode and the
    /// handshake reply cannot disagree about what was produced.
    Snapshot {
        kind: AttachPayloadKind,
        reply: oneshot::Sender<Result<SnapshotAt, TabError>>,
    },
    Resume {
        from_seq: u64,
        reply: oneshot::Sender<Result<ResumeAt, TabError>>,
    },
    Dump {
        /// History rows to return above the viewport, already clamped
        /// by the handler.
        scrollback: u32,
        reply: oneshot::Sender<Result<DumpData, TabError>>,
    },
    DumpResolved(oneshot::Sender<Result<ResolvedCellsData, TabError>>),
    /// Test-mode byte injection — the same pipeline a real chunk takes.
    FeedBytes(Vec<u8>),
    /// Test-mode: everything the task has queued toward the PTY writer
    /// since the last drain (client input AND terminal replies).
    /// `drain` consumes the buffer; otherwise it is copied and left in
    /// place, matching `tab.capture_pty_input`'s peek semantics on the
    /// UI path.
    CaptureInput {
        drain: bool,
        reply: oneshot::Sender<Vec<u8>>,
    },
}

/// What the reap task tells the tab task about the child's death.
pub(crate) enum ExitSignal {
    /// The child was reaped; here is its code. The task does not publish
    /// yet — the reader may still have bytes queued.
    Status(i32),
    /// The reader did not reach EOF within the grace window, so a
    /// descendant is holding the slave fd open. Publish now.
    Deadline(i32),
}

/// The channels `spawn` wires a tab task up with.
pub(crate) struct TabPipe {
    pub(crate) bytes_tx: mpsc::Sender<Vec<u8>>,
    pub(crate) exit_tx: mpsc::UnboundedSender<ExitSignal>,
    pub(crate) cmd_tx: mpsc::Sender<TabCmd>,
    pub(crate) tab_generation: u64,
    /// The theme generation the terminal was built from, so the spawn's
    /// promotion can tell whether `session.set_theme` landed while this
    /// tab was in flight — a window the fan-out's sessions snapshot
    /// cannot see (plan 037 §3.6).
    pub(crate) theme_generation: u64,
}

/// The server-side terminal for one tab, built **before** the child is
/// spawned so `spawn`'s "the spawn command is the last fallible step"
/// invariant survives: a terminal that cannot be constructed fails
/// `tab.open` with no child to reap.
pub(crate) struct TabVt {
    state: Arc<ServerVtState>,
    terminal: Terminal,
    replies: Arc<Mutex<Vec<u8>>>,
    router: OscRouter,
    colors: OscColorState,
    render: RenderState,
    /// The last geometry applied to this terminal and its child. All
    /// four numbers, so a client at the same grid with different cell
    /// metrics is still seen as a change.
    geometry: Geometry,
    tab_generation: u64,
    /// Highest theme generation this terminal has taken — seeded at
    /// build, advanced by `TabCmd::SetTheme`, never rolled back.
    theme_generation: u64,
}

impl TabVt {
    pub(crate) fn new(
        state: &Arc<ServerVtState>,
        cols: u16,
        rows: u16,
    ) -> Result<Self, roost_vt::Error> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: SERVER_VT_SCROLLBACK,
            continuation_max_bytes: SERVER_VT_CONTINUATION_MAX,
        })?;
        // A tab opened after `session.set_theme` starts on the client's
        // colors; one opened before any theme landed starts on the
        // headless default. Either way it happens before the tracker
        // reads the terminal back below, so both start from one set of
        // values (plan 037 §3.6).
        let (theme_generation, theme) = state.theme();
        match theme {
            Some(seed) => seed_theme(&mut terminal, &seed)?,
            None => {
                terminal.set_color_foreground(HEADLESS_FG)?;
                terminal.set_color_background(HEADLESS_BG)?;
                terminal.set_color_cursor(HEADLESS_FG)?;
            }
        }
        let replies = Arc::new(Mutex::new(Vec::new()));
        // Installed before the first byte: a device query in the very
        // first chunk must be answered like any other.
        terminal.set_write_pty_buffer(Arc::clone(&replies))?;
        let colors = OscColorState::new(color_seed(&terminal)?);
        let render = RenderState::new()?;
        Ok(Self {
            state: Arc::clone(state),
            terminal,
            replies,
            router: OscRouter::new(),
            colors,
            render,
            // Cell metrics start unset: nothing has stated any yet, and
            // the first interaction that does differs from this and is
            // applied.
            geometry: Geometry {
                cols,
                rows,
                cell_w: 0,
                cell_h: 0,
            },
            tab_generation: state.next_generation.fetch_add(1, Ordering::SeqCst),
            theme_generation,
        })
    }

    /// Start the task and return the channels it listens on.
    pub(crate) fn start(
        self,
        tab_id: i64,
        tee: broadcast::Sender<PtyOutputEvent>,
        writer: mpsc::Sender<WriterCmd>,
    ) -> TabPipe {
        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(TAB_CHANNEL_CHUNKS);
        let (exit_tx, exit_rx) = mpsc::unbounded_channel::<ExitSignal>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<TabCmd>(TAB_CMD_CAPACITY);
        let tab_generation = self.tab_generation;
        let capture = self.state.capture_pty_input;
        let task = TabTask {
            tab_id,
            vt: self,
            tee,
            writer,
            writer_gone: false,
            next_seq: 1,
            last_byte_seq: 0,
            ring: VecDeque::new(),
            ring_bytes: 0,
            pending: VecDeque::new(),
            pending_bytes: 0,
            capture,
            captured: Vec::new(),
            stored_exit: None,
            deferred_snapshots: Vec::new(),
            deferred_at: 0,
            ingested: 0,
        };
        let theme_generation = task.vt.theme_generation;
        tokio::spawn(task.run(bytes_rx, exit_rx, cmd_rx));
        TabPipe {
            bytes_tx,
            exit_tx,
            cmd_tx,
            tab_generation,
            theme_generation,
        }
    }
}

/// Push a theme's colors into a terminal — the server-side twin of the
/// iced UI's `apply_theme_candidate` (`terminal_tab.rs`), and the same
/// four FFI calls in the same order.
///
/// Only the terminal: the caller moves [`OscColorState`] separately,
/// because construction seeds it by reading the terminal back while a
/// reseed re-seeds the tracker it already has (which keeps whatever the
/// program set for itself — see [`OscColorState::reseed`]).
fn seed_theme(terminal: &mut Terminal, seed: &OscColorSnapshot) -> Result<(), roost_vt::Error> {
    terminal.set_color_foreground(vt_rgb(seed.foreground))?;
    terminal.set_color_background(vt_rgb(seed.background))?;
    terminal.set_color_cursor(vt_rgb(seed.cursor))?;
    let palette: [roost_vt::ColorRgb; 256] = (*seed.palette).map(vt_rgb);
    terminal.set_color_palette(&palette)
}

/// The tracker's color spelling → libghostty's. Named, because
/// [`color_seed`] converts the other way and an anonymous `rgb` closure
/// in each read identically at a glance.
fn vt_rgb(c: OscRgb) -> roost_vt::ColorRgb {
    roost_vt::ColorRgb::new(c.0, c.1, c.2)
}

/// Seed the OSC color tracker from the terminal itself, so the tracker
/// and the terminal start from one set of values and are moved from
/// there by the same bytes.
fn color_seed(terminal: &Terminal) -> Result<OscColorSnapshot, roost_vt::Error> {
    let rgb = |color: roost_vt::ColorRgb| -> OscRgb { (color.r, color.g, color.b) };
    let live = terminal.live_colors()?;
    let palette = terminal.live_palette()?;
    Ok(OscColorSnapshot::new(
        rgb(live.foreground),
        rgb(live.background),
        rgb(live.cursor.unwrap_or(live.foreground)),
        palette.map(rgb),
    ))
}

struct TabTask {
    tab_id: i64,
    vt: TabVt,
    tee: broadcast::Sender<PtyOutputEvent>,
    writer: mpsc::Sender<WriterCmd>,
    writer_gone: bool,
    next_seq: u64,
    /// The last seq an actual PTY record got. Deliberately not
    /// `next_seq - 1`: `Exit` consumes an ordinal too, and a resume
    /// window that counted it would call `final_seq` a byte record.
    last_byte_seq: u64,
    ring: VecDeque<(u64, Vec<u8>)>,
    ring_bytes: usize,
    pending: VecDeque<WriterCmd>,
    pending_bytes: usize,
    capture: bool,
    captured: Vec<u8>,
    /// `Exit`'s ordinal and the child's code, set once `publish_exit`
    /// has run. It is also what closes the wire: the VT keeps eating
    /// bytes afterwards (a descendant can still be writing) so it stays
    /// internally consistent, but nothing more is teed or ringed — the
    /// row is gone.
    stored_exit: Option<(u64, i32)>,
    /// Attach requests whose `vt` encode found the parser mid-sequence
    /// with libghostty's retained continuation unavailable — an
    /// unfinished sequence longer than [`SERVER_VT_CONTINUATION_MAX`].
    /// There is nothing encodable at that instant: a payload without the
    /// cut bytes would leave the client's parser in a different state
    /// than the server's, and the first live PTY frame would complete
    /// the sequence on one side only.
    ///
    /// So the request waits rather than failing. It is retried after the
    /// next chunk and answered at the first one that yields bytes, with
    /// the fence seq moving to that chunk. Two things end the wait: the
    /// tab's exit, after which no chunk can arrive at all
    /// ([`Self::publish_exit`]), and otherwise the attach side's own
    /// time budget.
    deferred_snapshots: Vec<oneshot::Sender<Result<SnapshotAt, TabError>>>,
    /// [`Self::ingested`] as of the last deferred-encode attempt, so a
    /// retry is even asked its cheap question only when a chunk has
    /// actually arrived to change the answer.
    deferred_at: u64,
    /// Chunks this task has put through [`Self::ingest`], counted rather
    /// than derived from `last_byte_seq` because a chunk arriving after
    /// the wire closed is assigned no seq and still moves the parser.
    ingested: u64,
}

impl TabTask {
    async fn run(
        mut self,
        mut bytes_rx: mpsc::Receiver<Vec<u8>>,
        mut exit_rx: mpsc::UnboundedReceiver<ExitSignal>,
        mut cmd_rx: mpsc::Receiver<TabCmd>,
    ) {
        let mut exit_code: Option<i32> = None;
        let mut bytes_open = true;
        let mut cmd_open = true;
        let mut exit_open = true;
        // Built once, not per pass: `select!` evaluates the expression of
        // a DISABLED branch too, so a `sleep(..)` here would construct
        // and drop a timer on every chunk and every command. `Interval`
        // ticking is allocation-free.
        let mut retry = tokio::time::interval(PENDING_FLUSH_RETRY);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The command channel closing is NOT the end: the reap task
        // removes the session — and with it the only long-lived `TabCmd`
        // sender — *before* it hands over the exit status, so a task that
        // stopped there would never publish its tab's `Exit`.
        while bytes_open || cmd_open {
            // Anything owed to the child goes out before more work is
            // taken on, so a writer channel that drained since the last
            // pass is used immediately.
            self.flush_writer();
            // Here and not at the end of `ingest`, which is synchronous
            // while the encode's permit is an await.
            self.retry_deferred_snapshots().await;
            // Deliberately NOT `biased`: a fixed poll order would let a
            // saturated command channel starve PTY ingestion (stalling
            // the child) or the reverse; random polling gives both sides
            // progress under sustained load.
            tokio::select! {
                signal = exit_rx.recv(), if exit_open => match signal {
                    Some(ExitSignal::Status(code)) => exit_code = Some(code),
                    Some(ExitSignal::Deadline(code)) => {
                        // A descendant is holding the slave fd, so the
                        // reader will not EOF for who knows how long.
                        // Drain what it has already queued, then close
                        // the wire — the order the forwarders rely on.
                        // Bounded: a descendant still writing refills
                        // the channel as fast as this drains it, and an
                        // unbounded loop would never publish. Anything
                        // past the bound arrives through the ordinary
                        // branch below and still reaches the VT
                        // (post-exit bytes are never teed).
                        for _ in 0..TAB_CHANNEL_CHUNKS {
                            match bytes_rx.try_recv() {
                                Ok(chunk) => self.ingest(chunk),
                                Err(_) => break,
                            }
                        }
                        self.publish_exit(code);
                    }
                    None => exit_open = false,
                },
                cmd = cmd_rx.recv(), if cmd_open => match cmd {
                    Some(cmd) => self.handle(cmd).await,
                    None => cmd_open = false,
                },
                chunk = bytes_rx.recv(), if bytes_open => match chunk {
                    Some(chunk) => self.ingest(chunk),
                    None => {
                        bytes_open = false;
                        if !self.wire_closed() {
                            let code = match exit_code {
                                Some(code) => code,
                                // A `&TabTask` cannot cross an await
                                // (the terminal handle is `!Sync`), so
                                // the wait is a free function.
                                None => await_status(self.tab_id, &mut exit_rx).await,
                            };
                            self.publish_exit(code);
                        }
                    }
                },
                // A writer channel that was full when the last reply was
                // queued frees up silently — nothing wakes this task. On
                // a quiet tab that would strand the reply forever, so
                // retry while anything is owed.
                _ = retry.tick(), if !self.pending.is_empty() => {}
            }
        }
        debug!(tab_id = self.tab_id, "server-vt tab task ended");
    }

    /// Once `Exit` is out the wire is closed: no more seqs, tee sends or
    /// ring records, even though the VT keeps parsing.
    fn wire_closed(&self) -> bool {
        self.stored_exit.is_some()
    }

    /// Encode the server terminal as one attach payload. `Ok(None)` is
    /// `vt`'s wait state only — see [`Self::deferred_snapshots`]; the
    /// error is already a message, because the only thing done with it
    /// is to hand it to whoever asked.
    fn encode(&mut self, kind: &AttachPayloadKind) -> Result<Option<Vec<u8>>, String> {
        match kind.as_str() {
            AttachPayloadKind::VT => roost_vt::vt_snapshot(&self.vt.terminal, &mut self.vt.render)
                .map_err(|error| error.to_string()),
            AttachPayloadKind::GHOSTTY_SNAPSHOT => self
                .vt
                .terminal
                .snapshot()
                .map(Some)
                .map_err(|error| error.to_string()),
            // Named rather than defaulted: encoding an unknown kind as
            // GHOSTSNP would put libghostty's binary state on the wire
            // under another name, past the build check that kind exists
            // to enforce — the corrupt screen `build-mismatch` prevents.
            other => Err(format!("this build cannot encode a {other:?} payload")),
        }
    }

    fn snapshot_at(&self, seq: u64, bytes: Vec<u8>) -> SnapshotAt {
        SnapshotAt {
            seq,
            server_epoch: self.vt.state.server_epoch,
            tab_generation: self.vt.tab_generation,
            bytes,
            cols: self.vt.geometry.cols,
            rows: self.vt.geometry.rows,
        }
    }

    /// Park an encode that had nothing to carry — unless the wire is
    /// already closed, in which case no chunk can ever arrive to change
    /// the answer and the wait would only burn the attach's whole time
    /// budget before failing anyway.
    fn defer_snapshot(&mut self, reply: oneshot::Sender<Result<SnapshotAt, TabError>>) {
        if self.wire_closed() {
            let _ = reply.send(Err(TabError::SnapshotFailed(EXITED_MID_SEQUENCE.into())));
            return;
        }
        self.deferred_at = self.ingested;
        self.deferred_snapshots.push(reply);
    }

    fn fail_deferred_snapshots(&mut self, why: &str) {
        for reply in std::mem::take(&mut self.deferred_snapshots) {
            let _ = reply.send(Err(TabError::SnapshotFailed(why.to_string())));
        }
    }

    /// Re-attempt every deferred encode, once a chunk has moved the
    /// parser. Always as `vt`, because that is the only kind that can
    /// defer. The fence moves with the retry: `last_byte_seq` is read
    /// now, so the payload and the seq a client streams from describe
    /// the same instant.
    async fn retry_deferred_snapshots(&mut self) {
        self.deferred_snapshots.retain(|reply| !reply.is_closed());
        if self.deferred_snapshots.is_empty() || self.ingested == self.deferred_at {
            return;
        }
        self.deferred_at = self.ingested;
        // The cheap question first, and before the permit: a tab
        // streaming one long unterminated sequence gets here on every
        // chunk, and taking the global encode permit to compose nothing
        // would stall this tab's own ingestion — and every other tab's
        // encode — once per chunk.
        match roost_vt::vt_carryable(&self.vt.terminal) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => return self.fail_deferred_snapshots(&error.to_string()),
        }
        let seq = self.last_byte_seq;
        let _permit = Arc::clone(&self.vt.state.snapshot_permits)
            .acquire_owned()
            .await;
        match self.encode(&AttachPayloadKind::from(AttachPayloadKind::VT)) {
            Ok(None) => {}
            Ok(Some(bytes)) => {
                for reply in std::mem::take(&mut self.deferred_snapshots) {
                    let _ = reply.send(Ok(self.snapshot_at(seq, bytes.clone())));
                }
            }
            Err(why) => self.fail_deferred_snapshots(&why),
        }
    }

    /// One chunk through the whole pipeline, in the order §3 pins.
    fn ingest(&mut self, data: Vec<u8>) {
        self.ingested += 1;
        // 1. seq — the tab task is the single authority (D3).
        let seq = (!self.wire_closed()).then(|| {
            let seq = self.next_seq;
            self.next_seq += 1;
            self.last_byte_seq = seq;
            seq
        });
        // 2. the authoritative terminal.
        self.vt.terminal.vt_write(&data);
        // 3. OSC scan, applied in wire order.
        let actions = self.vt.router.feed_stateful(&data, &mut self.vt.colors);
        for action in actions {
            self.apply_action(action);
        }
        // One `bell` per chunk that carried at least one BEL, not one
        // per byte: a program that rings twenty times in a burst wants
        // the client's attention once, and each effect is its own
        // commit on the events stream.
        if self.vt.router.take_bells() > 0 {
            self.vt
                .state
                .workspace
                .tab_effect(self.tab_id, TabEffectKind::Bell);
        }
        // 4. reply drain — required after EVERY vt_write.
        self.take_replies();
        // 5 + 6. tee, then the replay ring.
        if let Some(seq) = seq {
            let _ = self.tee.send(PtyOutputEvent::Bytes {
                seq,
                data: data.clone(),
            });
            self.push_ring(seq, data);
        }
    }

    fn apply_action(&self, action: OscAction) {
        match action {
            OscAction::Workspace { command, payload } => {
                self.vt
                    .state
                    .workspace
                    .apply_osc(self.tab_id, command, &payload);
            }
            // Client-local effects. A session has no view, so the only
            // thing it can do with one is hand it to whoever is
            // attached — the `tab.effect` envelope (plan 037 §3.6).
            // Nothing is replayed to a client that attaches later: a
            // clipboard is wanted as it is then, not as it was.
            OscAction::ClipboardWrite { target, text } => {
                // The scanner already refuses a truncated body; this is
                // the smaller policy bound on what a session will fan
                // out. Logged by size — the payload is user content and
                // never reaches a log line.
                if text.len() > CLIPBOARD_EFFECT_MAX_BYTES {
                    debug!(
                        tab_id = self.tab_id,
                        bytes = text.len(),
                        cap = CLIPBOARD_EFFECT_MAX_BYTES,
                        "dropped an oversized OSC clipboard write"
                    );
                    return;
                }
                self.vt.state.workspace.tab_effect(
                    self.tab_id,
                    TabEffectKind::ClipboardWrite {
                        text,
                        target: match target {
                            ClipboardTarget::System => ClipboardEffectTarget::System,
                            ClipboardTarget::Selection => ClipboardEffectTarget::Selection,
                        },
                    },
                );
            }
            OscAction::PointerShape(shape) => {
                debug!(
                    tab_id = self.tab_id,
                    %shape,
                    "dropped an OSC pointer shape: no client effects channel yet"
                );
            }
        }
    }

    /// Move whatever libghostty emitted into the pending queue. The lock
    /// is never held across a `vt_write` / `resize` — the trampoline
    /// takes the same mutex from inside those calls.
    fn take_replies(&mut self) {
        let bytes = {
            let mut guard = self
                .vt
                .replies
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if guard.is_empty() {
                return;
            }
            std::mem::take(&mut *guard)
        };
        self.queue_input(bytes);
    }

    /// Queue bytes for the child. Never blocks: coupling output
    /// processing to input drain would let a child that spews queries and
    /// never reads its input deadlock its own tab.
    fn queue_input(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if self.capture {
            self.captured.extend_from_slice(&bytes);
        }
        self.pending_bytes += bytes.len();
        self.pending.push_back(WriterCmd::Input(bytes));
        // Past the cap the OLDEST INPUT goes — a child that has not read
        // this much of its input has abandoned the answers. Never the
        // newest (one oversized blob would otherwise evict itself, so
        // the queue can exceed the cap by that one entry, itself bounded
        // by the wire's own op/frame limits), and never a `Resize` —
        // dropping one would leave the PTY at stale dimensions forever.
        while self.pending_bytes > REPLY_PENDING_MAX {
            let Some(oldest_input) = self
                .pending
                .iter()
                .position(|cmd| matches!(cmd, WriterCmd::Input(_)))
            else {
                break;
            };
            if oldest_input + 1 == self.pending.len() {
                break;
            }
            if let Some(WriterCmd::Input(dropped)) = self.pending.remove(oldest_input) {
                self.pending_bytes -= dropped.len();
                warn!(
                    tab_id = self.tab_id,
                    dropped = dropped.len(),
                    "dropped queued PTY writes past the cap; the child is not reading its input"
                );
            }
        }
        self.flush_writer();
    }

    /// Queue the child's winsize, handing back the receiver when the
    /// caller wants to hear whether the `ioctl` landed.
    ///
    /// A writer that is already gone gets neither the command nor an
    /// ack. [`Self::flush_writer`] is a no-op from the moment
    /// `writer_gone` is set, and `pending` is cleared only at that same
    /// instant — so a command queued afterwards would sit there forever
    /// with its `oneshot::Sender` inside it, neither sent nor dropped,
    /// which is the one shape [`await_winsize`] cannot answer promptly:
    /// it would spend the whole budget and then report a failure. There
    /// is no child left to tell, and no ack is exactly how the caller
    /// spells the success [`await_winsize`] promises for that case.
    fn queue_resize(&mut self, size: PtySize, acked: bool) -> Option<WinsizeAck> {
        if self.writer_gone {
            return None;
        }
        let (ack, rx) = if acked {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        self.pending.push_back(WriterCmd::Resize(size, ack));
        self.flush_writer();
        rx
    }

    /// Size the tab: the server terminal first, then the child's
    /// winsize. The single place either half moves — `TabCmd::Resize`
    /// and a geometry-bearing `TabCmd::Input` both land here — so the
    /// geometry recorded on [`TabVt`] cannot drift from what libghostty
    /// and the child were told.
    ///
    /// Geometry that has not changed is not re-applied: two clients
    /// alternating keystrokes at the same size would otherwise pay for a
    /// terminal resize per frame. Which is exactly why a **failed**
    /// resize must record nothing: with the short-circuit above, a
    /// geometry stored before libghostty accepted it would make every
    /// retry of that same size a silent no-op, and the tab would report
    /// a grid it never took for the rest of its life.
    ///
    /// `acked` asks for the child half's verdict; see [`WinsizeAck`].
    fn apply_geometry(
        &mut self,
        geometry: Geometry,
        acked: bool,
    ) -> Result<Option<WinsizeAck>, TabError> {
        if geometry == self.vt.geometry {
            return Ok(None);
        }
        let Geometry {
            cols,
            rows,
            cell_w,
            cell_h,
        } = geometry;
        let applied = self
            .vt
            .terminal
            .resize(cols, rows, cell_w, cell_h)
            .map_err(|error| {
                warn!(tab_id = self.tab_id, %error, "server terminal resize failed");
                TabError::from(error)
            });
        // The child is told nothing the terminal refused, and nothing is
        // recorded either: the two halves move together or not at all.
        // Queued ahead of the drain below so the winsize `ioctl` still
        // precedes the in-band size report the resize just emitted — a
        // program that reads the report and then asks `TIOCGWINSZ` must
        // not be told two different sizes.
        let queued = if applied.is_ok() {
            self.vt.geometry = geometry;
            // Pixel geometry stays 0 on the PTY winsize, matching
            // `PtySupervisor::resize`; libghostty gets the real cell
            // metrics above, which is what its size reports read.
            self.queue_resize(
                PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                acked,
            )
        } else {
            None
        };
        // Mode-2048 in-band size reports fire inside `resize`, outside
        // any `vt_write` — draining only after writes would silently
        // drop them.
        self.take_replies();
        applied?;
        Ok(queued)
    }

    fn flush_writer(&mut self) {
        if self.writer_gone {
            return;
        }
        while let Some(cmd) = self.pending.pop_front() {
            let len = writer_cmd_len(&cmd);
            match self.writer.try_send(cmd) {
                Ok(()) => self.pending_bytes -= len,
                Err(TrySendError::Full(cmd)) => {
                    self.pending.push_front(cmd);
                    return;
                }
                Err(TrySendError::Closed(_)) => {
                    // The PTY writer task is gone; nothing queued can
                    // ever land. Drop it rather than growing forever.
                    self.writer_gone = true;
                    self.pending.clear();
                    self.pending_bytes = 0;
                    return;
                }
            }
        }
    }

    fn push_ring(&mut self, seq: u64, data: Vec<u8>) {
        self.ring_bytes += data.len();
        self.ring.push_back((seq, data));
        while self.ring_bytes > REPLAY_RING_BYTES && self.ring.len() > 1 {
            if let Some((_, evicted)) = self.ring.pop_front() {
                self.ring_bytes -= evicted.len();
            }
        }
    }

    fn publish_exit(&mut self, code: i32) {
        // Idempotent: the EOF path and the reap task's deadline signal
        // can race (the reader drops `bytes_tx` before `_reader_alive`),
        // and a second `Exit` on the tee would break every forwarder's
        // "EXIT is final" contract.
        if self.wire_closed() {
            return;
        }
        let final_seq = self.next_seq;
        self.next_seq += 1;
        // Setting this closes the wire — see `wire_closed`.
        self.stored_exit = Some((final_seq, code));
        let _ = self.tee.send(PtyOutputEvent::Exit {
            seq: final_seq,
            code,
        });
        // The wire is closed, so no chunk can arrive to make a parked
        // encode carryable. Failing them here is what turns an attach
        // that can never be satisfied into a prompt `snapshot-failed`
        // instead of one that sits out the client's whole attach budget.
        self.fail_deferred_snapshots(EXITED_MID_SEQUENCE);
        self.vt.state.workspace.close_row(self.tab_id);
    }

    async fn handle(&mut self, cmd: TabCmd) {
        match cmd {
            TabCmd::Input { data, geometry } => {
                // A refused geometry is warned about inside and the
                // bytes still go: an INPUT frame is unacknowledged, so
                // there is nobody to answer, and dropping a client's
                // keystrokes over a resize the terminal disliked would
                // be the worse failure.
                if let Some(geometry) = geometry {
                    let _ = self.apply_geometry(geometry, false);
                }
                self.queue_input(data);
            }
            TabCmd::Resize { geometry, ack } => {
                let applied = self.apply_geometry(geometry, ack.is_some());
                // Answered only once BOTH halves are applied: a waiter
                // that resumed at the terminal resize alone could still
                // snapshot a tab whose child had not been told.
                if let Some(ack) = ack {
                    match applied {
                        // Off the task, because the child half is
                        // another task's FIFO: a tab that blocked here
                        // would stop rendering its own output behind a
                        // child that stopped reading its input.
                        Ok(Some(winsize)) => {
                            let tab_id = self.tab_id;
                            tokio::spawn(async move {
                                let _ = ack.send(await_winsize(tab_id, winsize).await);
                            });
                        }
                        // Nothing to wait for: either the geometry was
                        // already this one, so both halves are at it, or
                        // the PTY writer is gone and there is no child
                        // half left to tell. An exited tab still serves
                        // attaches, so the second is a success too.
                        Ok(None) => {
                            let _ = ack.send(Ok(()));
                        }
                        Err(error) => {
                            let _ = ack.send(Err(error));
                        }
                    }
                }
            }
            TabCmd::SetTheme(seed, generation) => {
                // Monotonic: a stale fan-out racing a fresh one (or the
                // promotion's catch-up send) must not roll colors back.
                if generation <= self.vt.theme_generation {
                    return;
                }
                self.vt.theme_generation = generation;
                if let Err(error) = seed_theme(&mut self.vt.terminal, &seed) {
                    warn!(tab_id = self.tab_id, %error, "server terminal theme reseed failed");
                    return;
                }
                // The tracker moves with the terminal, in the same
                // command, so nothing on the drain side can read a
                // theme the terminal has not taken. Program-set colors
                // survive both — that is `reseed`'s contract.
                self.vt.colors.reseed(seed);
                // `set_color_*` is not a `vt_write`, but the drain
                // contract is "after every call that can emit", and a
                // reply left sitting in the buffer would ride out on
                // whatever wrote next.
                self.take_replies();
            }
            TabCmd::Snapshot { kind, reply } => {
                let seq = self.last_byte_seq;
                // Bounds aggregate encode memory across tabs. Held only
                // around the encode, which is synchronous by C-API
                // contract and single-digit ms at this scrollback.
                let _permit = Arc::clone(&self.vt.state.snapshot_permits)
                    .acquire_owned()
                    .await;
                match self.encode(&kind) {
                    Ok(Some(bytes)) => {
                        let _ = reply.send(Ok(self.snapshot_at(seq, bytes)));
                    }
                    // Not a failure: the parser is mid-sequence with no
                    // retained continuation, so nothing encodable exists
                    // yet. See [`Self::deferred_snapshots`].
                    Ok(None) => self.defer_snapshot(reply),
                    Err(why) => {
                        let _ = reply.send(Err(TabError::SnapshotFailed(why)));
                    }
                }
            }
            TabCmd::Resume { from_seq, reply } => {
                let _ = reply.send(self.resume(from_seq));
            }
            TabCmd::Dump { scrollback, reply } => {
                let _ = reply.send(self.dump(scrollback));
            }
            TabCmd::DumpResolved(reply) => {
                let _ = reply.send(self.dump_resolved());
            }
            TabCmd::FeedBytes(data) => self.ingest(data),
            TabCmd::CaptureInput { drain, reply } => {
                let data = if drain {
                    std::mem::take(&mut self.captured)
                } else {
                    self.captured.clone()
                };
                let _ = reply.send(data);
            }
        }
    }

    /// The whole handoff runs here, on the task, so nothing can be teed
    /// between reading the ring and subscribing.
    fn resume(&self, from_seq: u64) -> Result<ResumeAt, TabError> {
        let last_assigned = self.last_byte_seq;
        // An empty ring still admits `last_assigned + 1`: the client
        // missed nothing, and an empty slice is the honest answer.
        let front = self
            .ring
            .front()
            .map(|(seq, _)| *seq)
            .unwrap_or(last_assigned + 1);
        if from_seq == 0 || from_seq > last_assigned + 1 || from_seq < front {
            return Err(TabError::RingMiss {
                from_seq,
                front,
                last_assigned,
            });
        }
        let slice = self
            .ring
            .iter()
            .filter(|(seq, _)| *seq >= from_seq)
            .map(|(seq, data)| (*seq, data.clone()))
            .collect();
        Ok(ResumeAt {
            slice,
            receiver: self.tee.subscribe(),
            last_assigned,
            stored_exit: self.stored_exit,
            cols: self.vt.geometry.cols,
            rows: self.vt.geometry.rows,
        })
    }

    /// Walk the viewport through the same densifier the UIs render from,
    /// so a headless dump and a UI dump cannot disagree.
    fn render_grid(&mut self) -> Result<(Vec<RenderedRow>, Colors, Option<CursorInfo>), TabError> {
        let vt = &mut self.vt;
        vt.render.update(&vt.terminal)?;
        let colors = vt.render.colors()?;
        let cursor = vt.render.cursor();
        // Every row, every time: a dump is on demand, not per frame, so
        // there is no cache for the dirty flags to protect.
        vt.render.mark_full()?;
        let defaults = (colors.foreground, colors.background);
        let cols = vt.geometry.cols;
        let mut grid: Vec<RenderedRow> = (0..usize::from(vt.geometry.rows))
            .map(|_| RenderedRow::default())
            .collect();
        vt.render.walk_dirty(&vt.terminal, |row, cells: &[Cell]| {
            if let Some(slot) = grid.get_mut(row as usize) {
                *slot = RenderedRow::build(cells, defaults, cols);
            }
        })?;
        Ok((grid, colors, cursor))
    }

    fn dump(&mut self, scrollback: u32) -> Result<DumpData, TabError> {
        let (grid, _, cursor) = self.render_grid()?;
        let scrollback_rows = roost_vt::scrollback_rows(&self.vt.terminal)?;
        let scrollback_text = roost_vt::scrollback_text(&self.vt.terminal, scrollback)?;
        Ok(DumpData {
            cols: u32::from(self.vt.geometry.cols),
            rows: u32::from(self.vt.geometry.rows),
            cursor: cursor
                .filter(|cursor| cursor.visible)
                .map(|cursor| (cursor.row, cursor.col, cursor.visible)),
            rows_text: grid.into_iter().map(|row| row.text).collect(),
            scrollback_rows,
            scrollback_text,
        })
    }

    fn dump_resolved(&mut self) -> Result<ResolvedCellsData, TabError> {
        let (grid, colors, _) = self.render_grid()?;
        let (cols, rows) = (self.vt.geometry.cols, self.vt.geometry.rows);
        let mut cells = Vec::with_capacity(usize::from(cols) * usize::from(rows));
        for row in 0..u32::from(rows) {
            // `RenderedRow::cells` is sparse but ascending by column, so
            // one forward cursor densifies the row without a per-row map.
            let sparse = grid.get(row as usize).map_or(&[][..], |r| &r.cells[..]);
            let mut next = 0;
            for col in 0..cols {
                let cell = sparse.get(next).filter(|cell| cell.col == col);
                if cell.is_some() {
                    next += 1;
                }
                let foreground = cell.map_or(colors.foreground, |cell| cell.foreground);
                let background = cell.map_or(colors.background, |cell| cell.background);
                cells.push(ResolvedCellData {
                    row,
                    col,
                    text: cell.map_or_else(|| " ".into(), |cell| cell.text.clone()),
                    fg: (foreground.r, foreground.g, foreground.b),
                    bg: (background.r, background.g, background.b),
                    has_explicit_bg: cell.is_some_and(|cell| cell.explicit_background),
                    bold: cell.is_some_and(|cell| cell.bold),
                    italic: cell.is_some_and(|cell| cell.italic),
                    inverse: cell.is_some_and(|cell| cell.inverse),
                });
            }
        }
        Ok(ResolvedCellsData { cols, rows, cells })
    }
}

/// The reader reached EOF before the reap task handed over a status.
/// Normally it arrives within microseconds (the reap task's `waitpid`
/// is already blocked when the child dies). A child that closed its
/// PTY fds but keeps RUNNING never gets one until it actually dies —
/// and the default path waits for the real status in that shape too,
/// so waiting here is parity, not a hang: publishing a made-up code
/// would close the row on a live process. The reap task holds a
/// sender for as long as the child exists, so a closed channel means
/// the reaper is gone — only then is `-1` reported.
async fn await_status(tab_id: i64, exit_rx: &mut mpsc::UnboundedReceiver<ExitSignal>) -> i32 {
    match exit_rx.recv().await {
        Some(ExitSignal::Status(code) | ExitSignal::Deadline(code)) => code,
        None => {
            warn!(
                tab_id,
                "pty reached EOF and the reap task is gone with no status; reporting -1"
            );
            -1
        }
    }
}

fn writer_cmd_len(cmd: &WriterCmd) -> usize {
    match cmd {
        WriterCmd::Input(data) => data.len(),
        WriterCmd::Resize(..) => 0,
    }
}
