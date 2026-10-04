//! Toolkit-neutral PTY supervision: spawn a shell, surface the master fd as async
//! streams of bytes, bridge writes/resizes back.
//!
//! Copied + adapted from `crates/roost-core/src/pty.rs` at M3 of
//! the daemon-removal refactor. Adaptations vs the daemon original:
//!
//! * Tab id type stays `i64` (matches the roost-ipc wire id range).
//! * `ROOST_TAB_ID` + `ROOST_SOCKET` env vars are injected into the
//!   child process so external tooling can dial back to this tab —
//!   the earlier daemon original did not do this. The acceptance
//!   criterion in the plan explicitly calls these out.
//! * Output goes to a per-tab broadcast channel rather than a
//!   single-consumer mpsc, so the UI's renderer and any future
//!   in-process subscriber can fan out. The legacy daemon's
//!   single-stream consumer is the `tokio::sync::broadcast`'s only
//!   subscriber for now, but the design pre-bakes the multi-sub
//!   path that M3+ doesn't need yet.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::Context;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, PtySize};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tracing::{debug, error, info, warn};

/// Depth of a tab's output fan-out. A consumer that falls this far
/// behind gets `RecvError::Lagged` and the skipped bytes are gone for
/// good — a *second*, independent way a tab's output can be truncated,
/// unrelated to the exit ordering fixed in #255 (that one is about
/// ordering, this one about capacity). Deliberately left alone:
/// closing it means resizing or redesigning the channel, and the only
/// production consumer (`session.rs`) forwards straight onto an
/// unbounded mpsc, so it can only lag if the UI's drain stalls. When it
/// does, `session.rs` reports it as `TabOutput::Error` rather than
/// silently swallowing it.
const PTY_OUTPUT_BROADCAST_CAPACITY: usize = 256;
const PTY_INPUT_CHANNEL_CAPACITY: usize = 64;
const PTY_OUTPUT_CHUNK_SIZE: usize = 4096;
/// Grace period after SIGHUP before `close()` escalates to SIGKILL.
/// Matches the Mac side's 20×10ms teardown window in
/// `PtySupervisor.swift`.
const KILL_GRACE: Duration = Duration::from_millis(200);
/// How often the reap fallback polls when `waitid(WNOWAIT)` is
/// unavailable. No supported target is expected to take that path; the
/// interval exists so a surprise degrades to CPU, never to a `close()`
/// blocked behind a reaper.
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How long each side of the exit handshake waits on the other before
/// publishing `Exit` itself (#255). Long enough that the normal path —
/// the reader hitting EOF within microseconds of the child being
/// reaped — always wins; short enough that a tab whose reader never
/// EOFs still reports its exit promptly.
const EXIT_PUBLISH_GRACE: Duration = Duration::from_millis(250);
/// How often [`PtySupervisor::shutdown_all`] re-reads the session map
/// while waiting for children to be reaped. The lifecycle channel wakes
/// the wait sooner in the common case; this bounds how long a lagged or
/// missed event can delay it.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// How long [`PtySupervisor::shutdown_all`] waits for a straggler to
/// leave the session map after escalating it to SIGKILL. A SIGKILL'd
/// child is reaped as soon as the kernel and the per-spawn wait task can
/// run; anything still there afterwards is reported as abandoned rather
/// than waited on indefinitely.
const SHUTDOWN_KILL_TAIL: Duration = Duration::from_millis(500);
/// How often the password poller samples every PTY's line discipline —
/// Ghostty's interval for the same heuristic.
const PASSWORD_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Depth of the supervisor's lifecycle channel. A subscriber this far
/// behind gets `Lagged`.
const LIFECYCLE_CAPACITY: usize = 64;

/// What a subscriber gets back from `PtySupervisor::subscribe`.
///
/// Every event carries `seq`, its ordinal within the spawn that
/// produced it: 1 for the first event, +1 for each event after,
/// `Exit` included (its own ordinal, not a copy of the last `Bytes`).
/// A respawned tab is a new session with a fresh counter. Seqs are
/// assigned in send order (see [`OutputPublisher`]), so a subscriber
/// can detect a gap left by a `Lagged` and, later, resume a host
/// session from the last seq it saw.
#[derive(Debug, Clone)]
pub enum PtyOutputEvent {
    /// PTY emitted `data`. Bytes are owned to make `broadcast`
    /// cheap (each subscriber Clones the `Arc<Vec<u8>>`-equivalent
    /// internal repr; here we use plain `Vec<u8>` since
    /// per-frame chunks are small and the broadcast clone is cheap
    /// enough at the workloads roost runs).
    Bytes { seq: u64, data: Vec<u8> },
    /// PTY child exited with this status. Published by the reader task
    /// after the last `Bytes` it read, so a consumer that stops here
    /// has the tab's complete output (#255). The one exception is the
    /// bounded fallback described on `PtySupervisor::spawn`: a reader
    /// that never reaches EOF gets `Exit` published out from under it
    /// on a deadline, and `Bytes` with higher seqs can still follow.
    Exit { seq: u64, code: i32 },
}

impl PtyOutputEvent {
    /// This event's ordinal within its spawn, whichever variant it is.
    /// A consumer watching for a `Lagged` gap cares about the number,
    /// not the variant, so it should not have to match on one.
    pub fn seq(&self) -> u64 {
        match self {
            Self::Bytes { seq, .. } | Self::Exit { seq, .. } => *seq,
        }
    }
}

/// The size one interaction claims for a tab: the grid AND the cell
/// metrics, compared as a whole. libghostty's mode-2048 in-band size
/// reports quote the pixel dimensions, so the same grid at a different
/// cell size is a different viewport and has to be applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
}

/// A tab's output channel plus its sequence counter.
///
/// The counter bump and the `send` have to happen together under one
/// lock: with a bare atomic, a producer that reserved seq N could stall
/// before sending while the other producer broadcast N+1, so
/// subscribers would see seqs out of order. Two producers race here on
/// the deadline path — the reader loop and the reap task's backstop —
/// which is exactly when that matters. `broadcast::Sender::send` is
/// synchronous and never blocks on subscribers, so the critical section
/// is a few nanoseconds and holds no `.await`.
struct OutputPublisher {
    tx: broadcast::Sender<PtyOutputEvent>,
    next_seq: Mutex<u64>,
}

impl OutputPublisher {
    fn new() -> Self {
        let (tx, _drop_rx) = broadcast::channel::<PtyOutputEvent>(PTY_OUTPUT_BROADCAST_CAPACITY);
        Self {
            tx,
            next_seq: Mutex::new(1),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<PtyOutputEvent> {
        self.tx.subscribe()
    }

    /// The raw fan-out sender, for the server-VT tab task. That task
    /// owns seq assignment for its tab (plan 036 D3), so it sends
    /// already-numbered events and this publisher's own counter stays
    /// untouched — two numbering authorities on one channel is exactly
    /// the gap/duplicate bug the seq contract exists to rule out.
    #[cfg(feature = "server-vt")]
    fn sender(&self) -> broadcast::Sender<PtyOutputEvent> {
        self.tx.clone()
    }

    fn send_bytes(&self, data: Vec<u8>) {
        self.publish(|seq| PtyOutputEvent::Bytes { seq, data });
    }

    fn send_exit(&self, code: i32) {
        self.publish(|seq| PtyOutputEvent::Exit { seq, code });
    }

    fn publish(&self, event: impl FnOnce(u64) -> PtyOutputEvent) {
        let mut next = self.next_seq.lock().unwrap();
        let seq = *next;
        *next += 1;
        let _ = self.tx.send(event(seq));
    }
}

/// Supervisor-level lifecycle events. The supervisor knows nothing of
/// the workspace; whoever owns both applies these to it.
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    TabExited {
        tab_id: i64,
        status: i32,
    },
    /// Where a tab's PTY stands on a password prompt: a spawn's first
    /// sample, or a change ([`PtySupervisor::start_password_poller`]).
    /// `incarnation` names the spawn the sample was taken from, which an
    /// owner compares with [`PtySupervisor::incarnation`] before applying
    /// it — the sample is read with no lock held, so the tab id may name a
    /// later spawn by the time it arrives.
    PasswordInput {
        tab_id: i64,
        incarnation: u64,
        password: bool,
    },
    /// Every live tab's sample at once, `(tab_id, incarnation, password)`
    /// — the poller's answer to [`PtySupervisor::republish_password_input`].
    /// One message whatever the tab count, so an owner resyncing after a
    /// lag cannot overflow the channel again with the resync itself.
    PasswordSnapshot {
        entries: Vec<(i64, u64, bool)>,
    },
}

/// What [`PtySupervisor::shutdown_all`] did with every tab that was live
/// when it started.
///
/// The three vectors partition that id set — each id appears in exactly
/// one of them, and each is sorted:
///
/// * `reaped` — the child was reaped without shutdown having to signal
///   it past the hangup: SIGHUP, or the per-tab SIGKILL watchdog, was
///   enough. A straggler that dies between the deadline and the
///   escalation belongs here too — the SIGKILL was never sent.
/// * `killed` — still live at the deadline, so `shutdown_all` delivered
///   a direct SIGKILL to it under its [`ReapLatch`], and it left the map
///   after that. "Delivered under the latch" is the whole claim: the
///   child had not been reaped when the signal went out, but it may
///   already have been a zombie the signal did nothing to.
/// * `abandoned` — still in the session map after the post-SIGKILL tail.
///   Its child may yet be reaped by its wait task; shutdown stopped
///   waiting.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    pub reaped: Vec<i64>,
    pub killed: Vec<i64>,
    pub abandoned: Vec<i64>,
}

/// Serialises signalling a child against reaping it, so no signal can
/// reach a recycled pid. The reap task observes the exit with
/// `waitid(WNOWAIT)` — the zombie keeps the pid — then `reap`s under the
/// lock; a signaller runs its `kill(2)` under the same lock and only
/// while the child is unreaped. So the pid is released to the kernel
/// with signalling already closed, rather than between a liveness check
/// and the signal that follows it.
///
/// A leaf lock: never taken while `sessions`, `pending` or `killer` is
/// held, and never held across anything that can block — a `kill(2)`, a
/// `wait()` that `WNOWAIT` has already proved immediate, or a reader's
/// liveness check ([`ReapLatch::read`]) — never the read itself (see
/// [`native_cwds_of`]). Holding it across a blocking wait would park
/// every `close()`, watchdog and shutdown escalation behind the reaper.
/// That rules out logging under it too: Roost's log appender is
/// synchronous, so a write inside a closure here queues the same three
/// callers behind a file write. A signaller captures what it needs —
/// `errno` included, read straight after the failing syscall, before a
/// log macro's own callsite work can clobber it — and logs after the
/// closure returns.
///
/// Prerequisite: this process is the child's only reaper. Nothing else
/// waits on it, and the kernel does not auto-reap it — Roost sets no
/// `SIGCHLD` disposition at all, neither `SIG_IGN` nor `SA_NOCLDWAIT`
/// (there is no `SIGCHLD` handling anywhere in the Rust tree). Whoever
/// else reaped it would release the pid with nobody's latch held, and no
/// guarantee of this kind could hold; auto-reap is only one way that
/// happens.
struct ReapLatch(Mutex<bool>);

impl ReapLatch {
    fn new() -> Self {
        Self(Mutex::new(false))
    }

    /// Run `signal` under the latch unless the child is already reaped.
    /// Answers whether it ran.
    fn signal(&self, signal: impl FnOnce()) -> bool {
        self.read(signal).is_some()
    }

    /// Run `read` under the latch unless the child is already reaped,
    /// and answer what it read. The child cannot be reaped while the
    /// latch is held, so its pid names this child for the whole read.
    /// Only for a body that cannot block and does not log.
    fn read<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        // Poison-tolerant, here and below: a panic under the latch must
        // not turn every later `close()` into a panic of its own.
        let closed = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (!*closed).then(read)
    }

    /// Close signalling and run `reap` under the latch. Only for a body
    /// that cannot block: a `wait()` that `WNOWAIT` has proved immediate,
    /// or a call that never waits at all.
    fn reap<T>(&self, reap: impl FnOnce() -> T) -> T {
        let mut closed = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *closed = true;
        reap()
    }

    /// Run a non-blocking `poll` under the latch and, unless it answers
    /// "still running", close signalling in the same critical section —
    /// so the pid that poll just released was already unreachable to
    /// every signaller. A failed poll closes the latch as well: losing
    /// ownership of the child (`ECHILD`) is as final as reaping it, and
    /// reporting the failure with the latch reopened would let a
    /// signaller reach a pid the kernel has already said is gone.
    fn reap_if_exited<T>(
        &self,
        poll: impl FnOnce() -> std::io::Result<Option<T>>,
    ) -> std::io::Result<Option<T>> {
        let mut closed = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let polled = poll();
        if !matches!(polled, Ok(None)) {
            *closed = true;
        }
        polled
    }
}

/// Wait for `pid` to exit without reaping it. Leaving the child a zombie
/// is the point: the pid stays reserved until a later `wait()` releases
/// it, which is the window [`ReapLatch`] closes signalling inside.
///
/// Darwin requires one of `WEXITED|WSTOPPED|WCONTINUED`; `WEXITED`
/// satisfies that and is what we want on both targets. `si_code` is read
/// because a successful `waitid` is not by itself a terminal status: a
/// child that made itself traced (`PTRACE_TRACEME` names *us* as its
/// tracer) reports its ptrace stops here too, and a `wait()` on a
/// stopped child blocks — which the caller would run under the latch.
/// So only `CLD_EXITED` / `CLD_KILLED` / `CLD_DUMPED` answer `Ok`, and
/// anything else keeps waiting — which is what the name promises anyway.
/// Returning on a stop would be worse than useless: it would hand the
/// caller to a `try_wait` that reads that same stop as an exit.
fn exited_without_reaping(pid: u32) -> std::io::Result<()> {
    loop {
        // SAFETY: `waitid` only writes the `siginfo_t` we hand it, and
        // `WNOWAIT` leaves the child unreaped either way.
        let (rc, si_code) = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let rc = libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            );
            (rc, info.si_code)
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if matches!(
            si_code,
            libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
        ) {
            return Ok(());
        }
        // Reported, but not dead: keep waiting rather than answering, and
        // sleep so a stop that `WNOWAIT` leaves in place cannot spin. The
        // poll fallback is no refuge here — `try_wait` reads a stop as an
        // exit — so a non-terminal report must never leave this loop.
        std::thread::sleep(REAP_POLL_INTERVAL);
    }
}

/// Reap by polling, for when `waitid(WNOWAIT)` is unavailable and the
/// zombie cannot be pinned across a `wait()`. The latch is held for each
/// non-blocking poll and never across the sleep, so a signaller waits on
/// a poll at worst — see [`ReapLatch`] for why a reaper must never be
/// something a `close()` can queue behind.
fn reap_by_polling<T>(
    latch: &ReapLatch,
    mut poll: impl FnMut() -> std::io::Result<Option<T>>,
) -> std::io::Result<T> {
    loop {
        if let Some(status) = latch.reap_if_exited(&mut poll)? {
            return Ok(status);
        }
        std::thread::sleep(REAP_POLL_INTERVAL);
    }
}

/// One tab's teardown handles, snapshotted out of the session map so
/// `shutdown_all` can hang every tab up without holding its lock.
struct Victim {
    tab_id: i64,
    /// A fresh `Mutex` around the session's cloned killer, so the
    /// snapshot feeds the unchanged `terminate_child` — which takes the
    /// session's own — rather than growing a second teardown path.
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: Option<u32>,
    /// The child's reap latch, shared with its session and its reap
    /// task. A victim snapshotted after the reap signals nothing,
    /// because [`ReapLatch::signal`] refuses — which is why `snapshot`
    /// needs no liveness rule of its own.
    latch: Arc<ReapLatch>,
}

impl Victim {
    /// Copy a live session's teardown handles without disturbing the
    /// entry. Leaving the entry in place is the point: while shutdown is
    /// running, "still in the map" is what it means by "not yet reaped".
    fn snapshot(tab_id: i64, session: &Session) -> Self {
        Self {
            tab_id,
            killer: Mutex::new(session.killer.lock().unwrap().clone_killer()),
            pid: session.pid,
            latch: session.latch.clone(),
        }
    }

    /// SIGHUP plus the per-tab SIGKILL watchdog, exactly as `close()`
    /// does it.
    fn hangup(&self) {
        terminate_child(&self.killer, self.pid, self.latch.clone(), self.tab_id);
    }

    fn terminate(self) {
        self.hangup();
    }

    /// SIGKILL this victim under its latch. Answers whether the signal
    /// both ran — the latch was still open — and was accepted by the
    /// kernel.
    fn sigkill(&self) -> bool {
        let Some(pid) = self.pid else { return false };
        // Written only inside the closure, so a `false` covers both "the
        // latch was already closed" and "the kernel refused".
        let mut landed = false;
        let mut failure = None;
        self.latch.signal(|| {
            // SAFETY: a pid we spawned, held unreaped by the latch for
            // the length of this closure.
            let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            if rc == 0 {
                landed = true;
            } else {
                failure = Some(std::io::Error::last_os_error());
            }
        });
        if let Some(err) = failure {
            // ESRCH is the child winning the race; anything else is a
            // real failure. Either way we did not kill it, so it is not
            // reported as `killed`.
            debug!(
                tab_id = self.tab_id,
                ?err,
                "pty shutdown SIGKILL did not land"
            );
        }
        landed
    }
}

/// A command on a tab's single writer channel. Input and resize share
/// one FIFO so they reach the PTY in submission order end-to-end —
/// the writer loop applies them in the exact order they were sent (#80).
pub(crate) enum WriterCmd {
    Input(Vec<u8>),
    /// Set the child's winsize, and — when a caller is waiting on the
    /// answer — say whether the `ioctl` landed.
    ///
    /// The ack exists because `TabCmd::Resize`'s own reply promises both
    /// halves were applied, and this is the half that happens on another
    /// task: without it a failed `TIOCSWINSZ` was logged and swallowed,
    /// and the waiter resumed as though the child had been told.
    /// `None` for the fire-and-forget callers, which is all of them but
    /// a focused attach.
    Resize(PtySize, Option<oneshot::Sender<Result<(), String>>>),
}

pub struct PtySupervisor {
    sessions: Arc<Mutex<HashMap<i64, Session>>>,
    /// Tab ids whose `spawn()` is in flight — the PTY has not yet
    /// been created but the slot is reserved so a concurrent
    /// `spawn(tab_id, ...)` rejects with `DuplicateTab` instead of
    /// racing the first one. Cleaned up on every `spawn()` exit
    /// path via `SlotGuard`.
    pending: Mutex<HashSet<i64>>,
    /// Latched by [`PtySupervisor::shutdown_all`] and never cleared. Two
    /// things read it: `spawn` refuses outright, and `close` stops
    /// removing session entries (removal is shutdown's reaped oracle;
    /// see [`PtySupervisor::close`]). Written under the `sessions` +
    /// `pending` locks, so a `spawn` either reserved its slot before the
    /// latch (and shutdown waits for it) or sees the latch and rejects.
    shutting_down: AtomicBool,
    /// Latched under the `sessions` lock in the same critical section
    /// that snapshots the victims. A `spawn` that reserved its slot
    /// before the latch but promotes after the snapshot would install a
    /// session nobody is left to tear down, so promotion rechecks this
    /// and kills its own child instead.
    sweep_started: AtomicBool,
    /// Serializes [`PtySupervisor::shutdown_all`]. Two concurrent
    /// teardowns would escalate from independent, stale snapshots — the
    /// second signalling pids the first already saw reaped. The loser
    /// waits and then runs against what the winner left, which is
    /// normally nothing.
    shutdown_gate: tokio::sync::Mutex<()>,
    /// One broadcast channel for supervisor-level events: the exits
    /// `shutdown_all` wakes on, and the password poller's reports, which
    /// the owner applies to its workspace.
    lifecycle: broadcast::Sender<SupervisorEvent>,
    /// Numbers every spawn, so a report about one PTY can never be
    /// mistaken for a later spawn that reuses its tab id.
    next_incarnation: AtomicU64,
    /// What the password poller reaches this supervisor through.
    password_watch: Arc<PasswordWatch>,
    /// Set by [`PtySupervisor::enable_server_vt`] before the first
    /// spawn. `None` — the default, and what every UI build sees even
    /// when feature unification compiles the code in — means the reader
    /// feeds the publisher directly, exactly as it always has.
    #[cfg(feature = "server-vt")]
    server_vt: std::sync::OnceLock<Arc<crate::tab_task::ServerVtState>>,
}

struct Session {
    /// Unified input+resize command channel — one FIFO, so commands
    /// reach the PTY in submission order through the writer loop.
    cmd_tx: mpsc::Sender<WriterCmd>,
    output: Arc<OutputPublisher>,
    /// Sendable kill handle obtained from
    /// `portable_pty::Child::clone_killer` before the child was
    /// moved into the wait task. `close()` invokes this to actively
    /// terminate the child rather than waiting for it to exit on
    /// its own (the legacy daemon's `close()` only dropped the
    /// sender side, which would leave long-running shells alive
    /// indefinitely until app exit).
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// Child pid, captured before the child moved into the wait
    /// task. `close()` uses it to SIGKILL-escalate if SIGHUP is
    /// ignored.
    pid: Option<u32>,
    /// Serialises this child's teardown signals against its reap; see
    /// [`ReapLatch`]. Shared with the reap task, with the SIGKILL
    /// watchdog, and with any `Victim` snapshotted off this session.
    latch: Arc<ReapLatch>,
    /// A receiver subscribed before the reader task started, held for
    /// the UI's first attach. The attach can be arbitrarily late (a
    /// main-loop hop for in-process opens, a TabOpened event for IPC
    /// opens); a fast command's first bytes land in this receiver's
    /// buffer instead of vanishing before a late `subscribe_output`.
    /// `take_initial_receiver` hands it out exactly once.
    initial_rx: Option<broadcast::Receiver<PtyOutputEvent>>,
    /// The server-VT tab task's command channel and this spawn's
    /// `tab_generation` — the second half of the identity a resuming
    /// client must match (plan 036 D6). One field, so a channel without
    /// its generation is not representable.
    ///
    /// Holding the sender here is also what keeps the task alive: the
    /// task ends when every sender is gone, and the reap task removes
    /// this session before anyone can observe the exit.
    #[cfg(feature = "server-vt")]
    tab_task: Option<(mpsc::Sender<crate::tab_task::TabCmd>, u64)>,
    /// This spawn's number, unique for the supervisor's life.
    incarnation: u64,
    /// The password poller's CLOEXEC dup of the master, held weakly. The
    /// writer task owns it beside the PTY's other master handles, so it
    /// closes exactly when they do. A session can outlive them: a reap
    /// still waiting on the child holds the map, and a dup held here
    /// would keep the master open, so a child that only ends on the
    /// hang-up the last master close sends would never end — and the
    /// reap would wait on it forever. A tick upgrades it for one
    /// `tcgetattr` at a time, so a recycled fd number can never redirect
    /// a read.
    password_fd: Weak<OwnedFd>,
}

/// The password poller's handle on its supervisor (plan 074 §D2).
///
/// The supervisor holds the only strong reference, so a poller that can
/// no longer upgrade its `Weak` knows the supervisor is gone — and the
/// drop wakes an idle poller so it finds out.
struct PasswordWatch {
    sessions: Arc<Mutex<HashMap<i64, Session>>>,
    lifecycle: broadcast::Sender<SupervisorEvent>,
    /// Woken by a spawn (an idle poller has something to sample), by
    /// `shutdown_all`, and by this watch's drop. Held by the poller
    /// directly, because waiting on it through the `Weak` would keep the
    /// watch alive for as long as the poller sleeps.
    wake: Arc<Notify>,
    started: AtomicBool,
    /// Latched by `shutdown_all`: the poller has nothing left to do.
    stopped: AtomicBool,
    /// Set by [`PtySupervisor::republish_password_input`]: the poller's
    /// next tick sends a [`SupervisorEvent::PasswordSnapshot`].
    republish: AtomicBool,
    /// How many times the poller has gone to sleep with nothing to
    /// sample, so a test can know it is asleep before it spawns.
    #[cfg(test)]
    idle_waits: AtomicU64,
}

impl PasswordWatch {
    /// Every promoted session's sampling handle. A spawn still in
    /// `pending` has no session yet, so it is skipped by construction.
    fn probes(&self) -> Vec<PasswordProbe> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(tab_id, session)| PasswordProbe {
                tab_id: *tab_id,
                incarnation: session.incarnation,
                fd: Weak::clone(&session.password_fd),
            })
            .collect()
    }
}

impl Drop for PasswordWatch {
    fn drop(&mut self) {
        self.wake.notify_one();
    }
}

/// One session, as the password poller samples it.
struct PasswordProbe {
    tab_id: i64,
    incarnation: u64,
    fd: Weak<OwnedFd>,
}

/// Whether the PTY behind `fd` is at a password prompt: canonical (line)
/// mode with echo off, Ghostty's heuristic. A master reports the slave's
/// line discipline on both Linux and macOS.
fn at_password_prompt(fd: BorrowedFd<'_>) -> std::io::Result<bool> {
    // SAFETY: `tcgetattr` only writes the `termios` it is handed, and
    // `fd` is borrowed from an open descriptor for the length of the call.
    let termios = unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd.as_raw_fd(), &mut termios) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        termios
    };
    Ok(termios.c_lflag & libc::ICANON != 0 && termios.c_lflag & libc::ECHO == 0)
}

/// The password poller's change detection, apart from its timer so a
/// test can drive it one tick at a time.
#[derive(Default)]
struct PasswordSampler {
    /// Each live tab's last reported spawn and value. A spawn's first
    /// sample is always reported, `false` included: it is what clears a
    /// flag its predecessor under the same tab id left raised, whose own
    /// clear an owner drops as stale once the replacement exists.
    reported: HashMap<i64, (u64, bool)>,
    /// Incarnations whose failing `tcgetattr` has already been logged.
    failures_logged: HashSet<u64>,
}

impl PasswordSampler {
    /// Sample every probe and answer what changed. A tab no probe names
    /// any more is forgotten without a report: its exit is the owner's
    /// to clear, through [`SupervisorEvent::TabExited`].
    fn tick(&mut self, probes: &[PasswordProbe]) -> Vec<SupervisorEvent> {
        let reported = std::mem::take(&mut self.reported);
        let mut changes = Vec::new();
        for probe in probes {
            let Some(password) = self.sample(probe) else {
                continue;
            };
            let sample = (probe.incarnation, password);
            if reported.get(&probe.tab_id) != Some(&sample) {
                changes.push(SupervisorEvent::PasswordInput {
                    tab_id: probe.tab_id,
                    incarnation: probe.incarnation,
                    password: sample.1,
                });
            }
            self.reported.insert(probe.tab_id, sample);
        }
        self.forget_failures_of_the_gone(probes);
        changes
    }

    /// Sample every probe and answer all of it as one
    /// [`SupervisorEvent::PasswordSnapshot`], recording each sample as
    /// reported.
    fn snapshot(&mut self, probes: &[PasswordProbe]) -> Vec<SupervisorEvent> {
        self.reported.clear();
        let mut entries = Vec::with_capacity(probes.len());
        for probe in probes {
            let Some(password) = self.sample(probe) else {
                continue;
            };
            self.reported
                .insert(probe.tab_id, (probe.incarnation, password));
            entries.push((probe.tab_id, probe.incarnation, password));
        }
        self.forget_failures_of_the_gone(probes);
        vec![SupervisorEvent::PasswordSnapshot { entries }]
    }

    /// `None` once the PTY's master handles are gone: there is nothing
    /// left to sample, and the session is on its way out of the map.
    fn sample(&mut self, probe: &PasswordProbe) -> Option<bool> {
        let fd = probe.fd.upgrade()?;
        Some(match at_password_prompt(fd.as_fd()) {
            Ok(password) => password,
            // A child that has exited leaves a master `tcgetattr` can fail
            // on (EIO). Nobody types a password into that.
            Err(error) => {
                if self.failures_logged.insert(probe.incarnation) {
                    debug!(
                        tab_id = probe.tab_id,
                        incarnation = probe.incarnation,
                        %error,
                        "tcgetattr on the pty master failed; reading it as no password prompt"
                    );
                }
                false
            }
        })
    }

    fn forget_failures_of_the_gone(&mut self, probes: &[PasswordProbe]) {
        self.failures_logged
            .retain(|incarnation| probes.iter().any(|probe| probe.incarnation == *incarnation));
    }
}

/// The password poller's body: one tick every [`PASSWORD_POLL_INTERVAL`]
/// while any session exists, asleep on `wake` while none does.
///
/// The sessions lock is held only to take the probes; every `tcgetattr`
/// runs after it is released, and the probes go when the tick ends.
async fn poll_password_input(watch: Weak<PasswordWatch>, wake: Arc<Notify>) {
    let mut sampler = PasswordSampler::default();
    loop {
        let Some(live) = watch.upgrade() else { return };
        if live.stopped.load(Ordering::SeqCst) {
            return;
        }
        let republish = live.republish.swap(false, Ordering::SeqCst);
        let probes = live.probes();
        let reports = if republish {
            sampler.snapshot(&probes)
        } else {
            sampler.tick(&probes)
        };
        let idle = probes.is_empty();
        drop(probes);
        for report in reports {
            let _ = live.lifecycle.send(report);
        }
        #[cfg(test)]
        if idle {
            live.idle_waits.fetch_add(1, Ordering::SeqCst);
        }
        drop(live);
        tokio::select! {
            () = tokio::time::sleep(PASSWORD_POLL_INTERVAL), if !idle => {}
            () = wake.notified() => {}
        }
    }
}

impl Default for PtySupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl PtySupervisor {
    pub fn new() -> Self {
        Self::with_lifecycle_capacity(LIFECYCLE_CAPACITY)
    }

    /// [`Self::new`] with the lifecycle channel's depth stated, so a test
    /// can overflow it with a handful of PTYs rather than more than a
    /// process's default fd limit allows.
    fn with_lifecycle_capacity(capacity: usize) -> Self {
        let (lifecycle, _rx) = broadcast::channel(capacity);
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let password_watch = Arc::new(PasswordWatch {
            sessions: Arc::clone(&sessions),
            lifecycle: lifecycle.clone(),
            wake: Arc::new(Notify::new()),
            started: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            republish: AtomicBool::new(false),
            #[cfg(test)]
            idle_waits: AtomicU64::new(0),
        });
        Self {
            sessions,
            pending: Mutex::new(HashSet::new()),
            shutting_down: AtomicBool::new(false),
            sweep_started: AtomicBool::new(false),
            shutdown_gate: tokio::sync::Mutex::new(()),
            lifecycle,
            next_incarnation: AtomicU64::new(1),
            password_watch,
            #[cfg(feature = "server-vt")]
            server_vt: std::sync::OnceLock::new(),
        }
    }

    /// Start sampling every PTY for a password prompt (plan 074 §D2),
    /// reporting each spawn's first sample and every change after it as a
    /// [`SupervisorEvent::PasswordInput`].
    ///
    /// The owner calls this, from inside a Tokio runtime — never `new`,
    /// which has none to spawn on. Idempotent: the first call answers the
    /// poller's handle, every later one `None`. The poller sleeps while
    /// no session exists, never keeps this supervisor alive, and ends
    /// when it is dropped or [`Self::shutdown_all`] begins.
    pub fn start_password_poller(&self) -> Option<tokio::task::JoinHandle<()>> {
        if self.password_watch.started.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some(tokio::spawn(poll_password_input(
            Arc::downgrade(&self.password_watch),
            Arc::clone(&self.password_watch.wake),
        )))
    }

    /// The spawn `tab_id` names right now, or `None` when it has no live
    /// PTY. What a [`SupervisorEvent::PasswordInput`] is checked against.
    pub fn incarnation(&self, tab_id: i64) -> Option<u64> {
        self.sessions
            .lock()
            .unwrap()
            .get(&tab_id)
            .map(|session| session.incarnation)
    }

    /// Have the password poller's next regular tick send a
    /// [`SupervisorEvent::PasswordSnapshot`] of every live tab — for an
    /// owner that fell behind the lifecycle channel and has to resync
    /// rather than replay. The snapshot carries incarnations and becomes
    /// the poller's own record of what it said, so nothing is sampled
    /// behind its back. Not a wake: a resync that lags again asks again,
    /// and the poller's cadence is what keeps that from spinning.
    pub fn republish_password_input(&self) {
        self.password_watch.republish.store(true, Ordering::SeqCst);
    }

    /// Turn the server-VT pipeline on for every tab this supervisor
    /// spawns from here on. Must be called before the first `spawn`;
    /// only `roost-session` calls it (plan 036 D1).
    ///
    /// Errors if it was already enabled: a second call would mint a
    /// second `server_epoch`, and two epochs on one supervisor would
    /// make a resuming client's identity check meaningless.
    #[cfg(feature = "server-vt")]
    pub fn enable_server_vt(&self, config: crate::tab_task::ServerVtConfig) -> anyhow::Result<()> {
        self.server_vt
            .set(Arc::new(crate::tab_task::ServerVtState::new(config)))
            .map_err(|_| anyhow::anyhow!("server-vt is already enabled on this supervisor"))
    }

    /// The random per-supervisor epoch tab streams are scoped by, or
    /// `None` when server-VT is off.
    #[cfg(feature = "server-vt")]
    pub fn server_epoch(&self) -> Option<u64> {
        self.server_vt.get().map(|state| state.server_epoch())
    }

    /// A live tab's server-VT command channel **and** the generation
    /// that channel belongs to, read under one lock — `None` when the
    /// tab has no live PTY or server-VT is off.
    ///
    /// One acquisition is the point: a respawn between two reads would
    /// otherwise let a caller resize one pipeline, admit an attach
    /// against a second's generation, and snapshot a third. Every
    /// caller that needs both must come through here.
    #[cfg(feature = "server-vt")]
    pub fn tab_task_handle(
        &self,
        tab_id: i64,
    ) -> Option<(mpsc::Sender<crate::tab_task::TabCmd>, u64)> {
        self.sessions
            .lock()
            .unwrap()
            .get(&tab_id)
            .and_then(|session| session.tab_task.as_ref())
            .map(|(cmd_tx, generation)| (cmd_tx.clone(), *generation))
    }

    /// A live tab's server-VT command channel, or `None` when the tab
    /// has no live PTY or server-VT is off.
    #[cfg(feature = "server-vt")]
    pub fn tab_commands(&self, tab_id: i64) -> Option<mpsc::Sender<crate::tab_task::TabCmd>> {
        self.tab_task_handle(tab_id).map(|(cmd_tx, _)| cmd_tx)
    }

    /// `session.set_theme`: record the session-wide theme and reseed
    /// every live tab, returning how many took it.
    ///
    /// Stored before the fan-out, because the pair is a race otherwise:
    /// a tab spawned between the store and the fan-out builds its
    /// terminal from the stored theme, and one spawned just before it
    /// is in the snapshot. A tab can therefore be seeded twice, which is
    /// idempotent, but never zero times.
    ///
    /// A tab whose task died between the snapshot and its send is not an
    /// error — its client is about to see `tab.closed` — so it is left
    /// out of the count rather than failing the op.
    ///
    /// `None` when server-VT is off — a UI supervisor has no terminals
    /// of its own to recolor.
    #[cfg(feature = "server-vt")]
    pub async fn set_theme(&self, seed: &crate::osc::OscColorSnapshot) -> Option<u32> {
        let state = self.server_vt.get()?;
        let generation = state.set_theme(seed.clone());
        // The guard is scoped so it is released before the first await:
        // this is a `std::sync::Mutex`, and the spawn path takes it.
        let tabs: Vec<mpsc::Sender<crate::tab_task::TabCmd>> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter_map(|session| session.tab_task.as_ref())
            .map(|(cmd_tx, _)| cmd_tx.clone())
            .collect();
        let mut reseeded: u32 = 0;
        for commands in tabs {
            if commands
                .send(crate::tab_task::TabCmd::SetTheme(seed.clone(), generation))
                .await
                .is_ok()
            {
                reseeded = reseeded.saturating_add(1);
            }
        }
        Some(reseeded)
    }

    /// See [`crate::tab_task::AttachPause`]. A no-op in every build a
    /// test did not configure a seam into.
    #[cfg(feature = "server-vt")]
    pub(crate) async fn pause_attach_admission(&self) {
        if let Some(state) = self.server_vt.get() {
            state.pause_admission().await;
        }
    }

    /// Which pipeline this tab id names **right now**, or `None` when it
    /// names none — a tab that exited, or one this supervisor never had.
    ///
    /// The attach forwarder reads the two apart: a tab with no pipeline
    /// at all is one whose stream ends in `EXIT`, while a tab naming a
    /// *different* generation than the one that served a hand-off is a
    /// second terminal wearing the first's id.
    #[cfg(feature = "server-vt")]
    pub fn tab_generation(&self, tab_id: i64) -> Option<u64> {
        self.tab_task_handle(tab_id)
            .map(|(_, generation)| generation)
    }

    /// Subscribe to supervisor-level lifecycle events
    /// (tab-exited, etc.). Subscribers that fall behind get a
    /// `Lagged` and should re-snapshot from the workspace.
    pub fn subscribe_lifecycle(&self) -> broadcast::Receiver<SupervisorEvent> {
        self.lifecycle.subscribe()
    }

    /// Subscribe to the byte+exit stream for a single tab. Returns
    /// `None` if the tab has no live PTY.
    pub fn subscribe_output(&self, tab_id: i64) -> Option<broadcast::Receiver<PtyOutputEvent>> {
        self.sessions
            .lock()
            .unwrap()
            .get(&tab_id)
            .map(|s| s.output.subscribe())
    }

    /// The receiver `spawn` subscribed before the reader task started,
    /// handed out exactly once — the UI's first attach consumes it so
    /// output emitted before the attach is preserved. `None` if the
    /// tab has no live PTY or the receiver was already taken (a
    /// reattach falls back to [`Self::subscribe_output`]).
    pub fn take_initial_receiver(
        &self,
        tab_id: i64,
    ) -> Option<broadcast::Receiver<PtyOutputEvent>> {
        self.sessions
            .lock()
            .unwrap()
            .get_mut(&tab_id)
            .and_then(|s| s.initial_rx.take())
    }

    /// Best-effort native read of the cwd of the tab's foreground job:
    /// the first of [`Self::native_cwds`]. `None` if the tab has no
    /// live PTY or every read fails.
    pub fn foreground_cwd(&self, tab_id: i64) -> Option<String> {
        self.native_cwds(tab_id).into_iter().next()
    }

    /// Best-effort native reads of the tab's cwd, best first — what a
    /// new tab opened from it inherits, since a new tab spawns a LOCAL
    /// shell. First the cwd of the foreground process group's leader,
    /// the job the shell is running: a nested shell, `nix develop`, a
    /// command. Then the direct child's, the shell Roost started, which
    /// is also the leader while the shell sits at its prompt. Empty if
    /// the tab has no live PTY or every read fails.
    ///
    /// A leader that changes directory moves the answer with it — `git`
    /// under its pager reads as the repository root. Intended: that is
    /// where the job is, and kitty answers the same way.
    pub fn native_cwds(&self, tab_id: i64) -> Vec<String> {
        let child = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(&tab_id)
                .and_then(|s| s.pid.map(|pid| (pid, s.latch.clone())))
        };
        child
            .map(|(pid, latch)| native_cwds_of(pid, &latch))
            .unwrap_or_default()
    }

    /// Spawn a shell for `tab_id`.
    ///
    /// Returns a `broadcast::Receiver` subscribed *before* the PTY
    /// reader task starts producing — early subscribers cannot lose
    /// initial output. Late subscribers can still call
    /// [`Self::subscribe_output`].
    ///
    /// `socket_path` is the absolute path to the IPC socket, injected
    /// into the child as `ROOST_SOCKET` so `roostctl` invoked from
    /// inside the tab dials the right UI.
    ///
    /// Exit ordering (#255): the reader task publishes `Exit`, after
    /// the last `Bytes` it read. One producer makes "every byte, then
    /// the exit" structural instead of a race between the reader and
    /// the reap task — the shape that used to drop a shell's final
    /// output. The reap task hands the status over and then waits for
    /// the reader to finish, but only for `EXIT_PUBLISH_GRACE`: a
    /// reader can legitimately never reach EOF (a background
    /// descendant holding the slave fd keeps the master readable
    /// forever), and an unbounded wait would mean the tab never
    /// reports its exit. Past the deadline the reap task publishes
    /// `Exit` itself, and bytes may still follow it — the one
    /// documented exception to the ordering guarantee. Whichever side
    /// gets there first, `publish_exit_once` makes it exactly one
    /// `Exit`.
    ///
    /// Session lifetime: the session is installed before the reap task
    /// starts, and the reap task removes it before it reports the exit
    /// on either channel. So a session always has a waiter that will
    /// take it back out, and by the time a consumer sees `Exit` (or
    /// `TabExited`) the tab is already gone from the map.
    ///
    /// Server-VT (plan 036, [`Self::enable_server_vt`]) reshapes both
    /// halves of that: the reader feeds a bounded channel instead of the
    /// publisher, and BOTH exit producers route through the tab task, so
    /// the deadline path becomes drain-then-`Exit` and nothing is teed
    /// after it. The flag-off path — every UI build — is untouched.
    ///
    /// Errors:
    /// * [`PtyError::DuplicateTab`] — `tab_id` already has a live
    ///   session. Caller must `close()` the prior session first.
    pub fn spawn(
        &self,
        tab_id: i64,
        cwd: &str,
        argv: &[String],
        cols: u16,
        rows: u16,
        socket_path: &std::path::Path,
    ) -> anyhow::Result<broadcast::Receiver<PtyOutputEvent>> {
        self.spawn_with(tab_id, cwd, argv, cols, rows, socket_path, || {})
    }

    /// [`Self::spawn`], plus a seam for the one window nothing else can
    /// reach: `before_promote` runs after the child exists and before
    /// the promotion re-checks `pending`, so a test can land a `close()`
    /// inside it deterministically. It is invoked **outside every
    /// lock** — a hook holding `sessions` or `pending` would deadlock
    /// the very `close()` it exists to let in.
    // `spawn`'s own list is already at clippy's bar; the seam is the
    // one over it.
    #[allow(clippy::too_many_arguments)]
    fn spawn_with(
        &self,
        tab_id: i64,
        cwd: &str,
        argv: &[String],
        cols: u16,
        rows: u16,
        socket_path: &std::path::Path,
        before_promote: impl FnOnce(),
    ) -> anyhow::Result<broadcast::Receiver<PtyOutputEvent>> {
        // Reserve the slot atomically. Two concurrent
        // `spawn(tab_id, ...)` calls used to be racy: the first
        // would `contains_key` and the second would do the same
        // before either could `insert`, then both PTYs would
        // create and the second `insert` would orphan the first.
        //
        // Strategy: hold a `pending` set alongside `sessions` and
        // atomically check both before reserving the slot in
        // `pending`. We build the PTY without the lock held (the
        // operations involve OS calls and tokio spawns that don't
        // belong under a Mutex), then promote the slot from
        // `pending` to `sessions` once everything is built. A
        // `SlotGuard` removes the pending entry on any early
        // exit. `subscribe_output` returns None while the slot is
        // pending (no Session exists yet) — that's the same
        // behavior as "tab doesn't exist yet."
        //
        // CR on PR #78 specifically flagged that the previous
        // placeholder-Session approach leaked a stale broadcast
        // sender to subscribers who raced the swap. The
        // pending-set design has no such hazard because the
        // Session entry only ever exists with its REAL channels.
        {
            let sessions = self.sessions.lock().unwrap();
            let mut pending = self.pending.lock().unwrap();
            // Same critical section `shutdown_all` latches the flag in,
            // so there is no window where a spawn passes the check and
            // then reserves a slot the shutdown sweep has already read.
            if self.shutting_down.load(Ordering::SeqCst) {
                return Err(PtyError::ShuttingDown(tab_id).into());
            }
            if sessions.contains_key(&tab_id) || pending.contains(&tab_id) {
                return Err(PtyError::DuplicateTab(tab_id).into());
            }
            pending.insert(tab_id);
        }
        struct SlotGuard<'a> {
            sup: &'a PtySupervisor,
            tab_id: i64,
            armed: bool,
        }
        impl Drop for SlotGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    let _ = self.sup.pending.lock().unwrap().remove(&self.tab_id);
                }
            }
        }
        let mut slot = SlotGuard {
            sup: self,
            tab_id,
            armed: true,
        };

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        // Acquire everything derived from the master BEFORE spawning
        // the child, so `spawn_command` stays the last fallible step.
        // If any of these fail, the PTY tears down with no child to
        // orphan. Doing them *after* the spawn (as before) could return
        // an error while a live shell had no wait task installed — that
        // PTY would escape supervisor control entirely (#80).
        //
        // The master is O_NONBLOCK from here on. Every dup below shares
        // that (one open file description); the slave is its own, so
        // the child's stdio stays blocking. It is set first, before the
        // dups, so nothing ever holds a blocking view of the master: on
        // Linux a master `write(2)` parked on a full slave input buffer
        // is never released — not by the child dying, not by the slave
        // closing (#409) — so no write may ever block.
        let master_fd = pair.master.as_raw_fd().context("master pty has no fd")?;
        set_nonblocking(master_fd).context("master O_NONBLOCK")?;
        let reader_handle = dup_master(master_fd).context("dup master for reader")?;
        let password_fd = Arc::new(OwnedFd::from(
            dup_master(master_fd).context("dup master for the password poller")?,
        ));
        let password_watch_fd = Arc::downgrade(&password_fd);
        let writer = AsyncFd::with_interest(
            dup_master(master_fd).context("dup master for writer")?,
            Interest::WRITABLE,
        )
        .context("register master writer with the reactor")?;
        // Kept for its drop, never written to: portable-pty sends `\n`
        // + VEOF to the child when this handle drops, the only EOF a
        // stdin-reading child gets if a supervisor is dropped without
        // `close()`.
        let eof_on_drop = pair.master.take_writer().context("master.take_writer")?;

        // The server Terminal is built HERE, before the child: it is
        // fallible, and `spawn_command` has to stay the last fallible
        // step so a failure leaves no live shell behind (plan 036 D2).
        #[cfg(feature = "server-vt")]
        let tab_vt = match self.server_vt.get() {
            Some(state) => Some(
                crate::tab_task::TabVt::new(state, cols, rows)
                    .context("build the server terminal")?,
            ),
            None => None,
        };

        let cmd = build_command(cwd, argv, tab_id, socket_path);
        let mut child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        // Sendable killer handle taken before the child moves into
        // the wait task — `close()` uses it to actively terminate
        // the shell rather than waiting for it to notice the
        // dropped input channel.
        let killer = child.clone_killer();
        // Captured before the child moves into the wait task so
        // `close()` can SIGKILL-escalate by pid if SIGHUP is ignored.
        let pid = child.process_id();
        // Shared with the wait task: the reap closes it, and every
        // signaller for this child runs under it (see `ReapLatch`).
        let latch = Arc::new(ReapLatch::new());

        // Drop the slave end now that the shell has it.
        drop(pair.slave);

        let output = Arc::new(OutputPublisher::new());
        // Subscribe BEFORE we spawn the reader task. Returning this
        // to the caller guarantees no Bytes/Exit event between
        // spawn and caller-subscribe can be lost.
        let early_rx = output.subscribe();
        // Second pre-reader subscription, stashed in the Session for
        // the UI's first attach (see `Session::initial_rx`).
        let initial_rx = output.subscribe();
        // One command channel for input + resize so they apply to the
        // PTY in submission order (#80).
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WriterCmd>(PTY_INPUT_CHANNEL_CAPACITY);

        let master = pair.master;

        // The two halves of the exit handshake (#255). `status_*`
        // carries the reaped status to the reader so it can publish
        // `Exit` after its final `Bytes`; `reader_alive_*` carries
        // nothing — the reap task's wait ends on the reader task
        // dropping its sender, which is precisely "the reader is
        // done". `exit_published` keeps the two sides to one `Exit`.
        let (status_tx, status_rx) = std::sync::mpsc::channel::<i32>();
        let (reader_alive_tx, reader_alive_rx) = std::sync::mpsc::channel::<()>();
        let exit_published = Arc::new(AtomicBool::new(false));

        // Server-VT: the tab task becomes the single authority for this
        // tab's seq, tee, replies and exit. Started before the reader so
        // no byte can be read before there is somewhere to put it.
        #[cfg(feature = "server-vt")]
        let tab_pipe = tab_vt.map(|vt| vt.start(tab_id, output.sender(), cmd_tx.clone()));
        #[cfg(feature = "server-vt")]
        let reader_bytes_tx = tab_pipe.as_ref().map(|pipe| pipe.bytes_tx.clone());
        #[cfg(feature = "server-vt")]
        let reap_exit_tx = tab_pipe.as_ref().map(|pipe| pipe.exit_tx.clone());

        // Reader: blocking read off the master fd, push to broadcast,
        // then publish the exit.
        tokio::task::spawn_blocking({
            let output = output.clone();
            let exit_published = exit_published.clone();
            move || {
                let _reader_alive = reader_alive_tx;
                #[cfg(feature = "server-vt")]
                {
                    if let Some(bytes_tx) = reader_bytes_tx {
                        // The tab task owns the exit under server-VT:
                        // dropping `bytes_tx` with this closure IS the
                        // EOF signal, and the reap task hands over the
                        // status.
                        //
                        // `blocking_send` on a bounded channel is the
                        // point (architecture §3): when the tab task
                        // falls behind, this read stalls, the kernel PTY
                        // buffer fills and the child blocks on `write`.
                        // That backpressure is what lets the
                        // authoritative terminal never miss a byte.
                        pty_reader_loop(reader_handle, tab_id, |chunk| {
                            let sent = bytes_tx.blocking_send(chunk).is_ok();
                            if !sent {
                                debug!(tab_id, "server-vt tab task is gone; stopping reader");
                            }
                            sent
                        });
                        return;
                    }
                }
                pty_reader_loop(reader_handle, tab_id, |chunk| {
                    output.send_bytes(chunk);
                    true
                });
                // EOF: everything the PTY produced is on the channel,
                // so `Exit` published from here can only follow it.
                // The status normally lands within microseconds (the
                // reap task's `waitpid` is already blocked when the
                // child dies); if it doesn't, the reap task publishes
                // once it does, still after this EOF.
                match status_rx.recv_timeout(EXIT_PUBLISH_GRACE) {
                    Ok(status) => {
                        publish_exit_once(&output, &exit_published, status);
                    }
                    Err(_) => debug!(
                        tab_id,
                        "pty reader finished before the child was reaped; reap task publishes Exit"
                    ),
                }
            }
        });

        // Writer + resizer: a single ordered loop over the unified
        // command stream, so a resize never reorders relative to the
        // input bytes submitted around it (and keystrokes never
        // reorder relative to each other). A write that cannot make
        // progress waits on the reactor, not in a syscall: a child that
        // stops reading costs this task nothing, and the slave's
        // hang-up ends the wait (#409).
        tokio::spawn(async move {
            let _eof_on_drop = eof_on_drop;
            let _password_fd = password_fd;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    WriterCmd::Input(data) => {
                        if let Err(err) = write_all_nonblocking(&writer, &data).await {
                            warn!(tab_id, ?err, "pty write failed");
                            break;
                        }
                    }
                    WriterCmd::Resize(size, ack) => {
                        let applied = master.resize(size).map_err(|err| {
                            warn!(tab_id, ?err, "pty resize failed");
                            err.to_string()
                        });
                        if let Some(ack) = ack {
                            let _ = ack.send(applied);
                        }
                    }
                }
            }
            debug!(tab_id, "pty input loop ended");
        });

        let output_for_exit = output.clone();
        let lifecycle_tx = self.lifecycle.clone();
        let sessions_for_reap = self.sessions.clone();
        let latch_for_wait = latch.clone();
        let exit_published_for_wait = exit_published.clone();

        let session = Session {
            cmd_tx,
            output,
            killer: Mutex::new(killer),
            pid,
            latch,
            initial_rx: Some(initial_rx),
            #[cfg(feature = "server-vt")]
            tab_task: tab_pipe
                .as_ref()
                .map(|pipe| (pipe.cmd_tx.clone(), pipe.tab_generation)),
            incarnation: self.next_incarnation.fetch_add(1, Ordering::Relaxed),
            password_fd: password_watch_fd,
        };
        before_promote();
        // Promote the slot from pending → sessions atomically, BEFORE
        // the reap task exists (see below).
        //
        // If `close(tab_id)` ran while we were building the PTY it
        // removed our entry from `pending` as a cancellation signal.
        // Detect that here and hand the session back instead of
        // installing it; the caller-visible teardown happens after the
        // reap task is started, so the child is still reaped.
        //
        // `shutdown_all` is the second way this promotion can be too
        // late. Its bounded wait for `pending` to drain can expire while
        // this spawn is still building, and once it has snapshotted the
        // victims nothing will ever walk this tab: installing here would
        // orphan a live child. So the same "hand it back and kill it"
        // path covers both, with the error naming which one it was.
        let unwanted: Option<(Session, PtyError)> = {
            let mut sessions = self.sessions.lock().unwrap();
            let mut pending = self.pending.lock().unwrap();
            if !pending.remove(&tab_id) {
                Some((session, PtyError::Cancelled(tab_id)))
            } else if self.sweep_started.load(Ordering::SeqCst) {
                Some((session, PtyError::ShuttingDown(tab_id)))
            } else {
                sessions.insert(tab_id, session);
                None
            }
        };
        // Either branch consumed the pending entry (ours, or the one
        // `close()` already took), so the guard has nothing left to do.
        slot.armed = false;
        if unwanted.is_none() {
            self.password_watch.wake.notify_one();
        }

        // A `session.set_theme` that ran between this tab's terminal
        // build and its promotion snapshotted a sessions map this tab
        // was not in yet — the one window the fan-out cannot see. The
        // generation check makes the catch-up idempotent, and the task's
        // own monotonic check makes it safe against a racing fresh
        // fan-out. `try_send` on a freshly minted channel cannot
        // meaningfully be full; a dropped catch-up is repaired by the
        // next set_theme like any other missed reseed.
        #[cfg(feature = "server-vt")]
        if unwanted.is_none() {
            if let (Some(pipe), Some(state)) = (tab_pipe.as_ref(), self.server_vt.get()) {
                let (generation, seed) = state.theme();
                if generation > pipe.theme_generation {
                    if let Some(seed) = seed {
                        let _ = pipe
                            .cmd_tx
                            .try_send(crate::tab_task::TabCmd::SetTheme(seed, generation));
                    }
                }
            }
        }

        // Wait for the child to exit; hand the status to the reader
        // task (which publishes it onto the output channel) and send
        // it on the lifecycle channel so both per-tab consumers and
        // the workspace converge.
        //
        // Started only now that the promotion has run, because this
        // task's identity-checked removal is the ONLY thing that ever
        // takes the session back out. Starting it earlier meant a
        // child that exited during the promotion window got reaped
        // first: the removal found no session and removed nothing,
        // then the promotion installed a session whose child was
        // already dead — `has()` kept answering yes and `write()` kept
        // accepting input for a PTY nobody was reading. Ordering it
        // after the insert makes "a reaped child leaves no session"
        // structural. It also means no `Exit` can be published before
        // the session is reachable: the reader only publishes once
        // this task hands it a status.
        tokio::task::spawn_blocking(move || {
            // Two branches because only one of them can prove the
            // `wait()` immediate (see `ReapLatch`): with a pid, `waitid`
            // pins the exit as a zombie first; without one, or if
            // `waitid` fails outright, polling is what keeps the latch
            // off a blocking wait.
            let waited = match pid.map(exited_without_reaping) {
                Some(Ok(())) => latch_for_wait.reap(|| child.wait()),
                other => {
                    debug!(
                        tab_id,
                        ?other,
                        "waitid(WNOWAIT) unavailable; reaping by poll"
                    );
                    reap_by_polling(&latch_for_wait, || child.try_wait())
                }
            };
            let (status, exit) = match waited {
                Ok(exit) => (exit.exit_code() as i32, Some(exit)),
                Err(err) => {
                    error!(tab_id, ?err, "child.wait failed");
                    (-1, None)
                }
            };
            // The reap above closed the latch in the same critical
            // section, so a concurrent `close()` SIGKILL watchdog has
            // already stood down. Drop the dead session next, so later
            // writes get `NotFound` instead of silently succeeding
            // against a closed PTY — and only then tell anyone the
            // child exited. Removing ahead of both the status handoff
            // and `TabExited` means the tab is already unreachable by
            // the time either channel reports the exit, so a consumer
            // reacting to `Exit` can never find a live session for a
            // dead child.
            {
                // Only remove the session if THIS waiter still owns it.
                // `close()` frees the slot synchronously, so the same
                // tab_id can be re-spawned before a stale waiter fires;
                // matching the per-spawn latch identity prevents
                // evicting a newer live session (#80). Taken after the
                // reap returned, never inside it — the latch is a leaf
                // lock. Scoped so the deadline wait below never holds
                // the sessions lock.
                let mut sessions = sessions_for_reap.lock().unwrap();
                let owns = sessions
                    .get(&tab_id)
                    .map(|s| Arc::ptr_eq(&s.latch, &latch_for_wait))
                    .unwrap_or(false);
                if owns {
                    sessions.remove(&tab_id);
                }
            }
            let _ = status_tx.send(status);
            let _ = lifecycle_tx.send(SupervisorEvent::TabExited { tab_id, status });
            // Logged only now: a subscriber that blocks or panics must
            // not stand between the reap and the handoffs above.
            if let Some(exit) = exit {
                debug!(tab_id, ?pid, %exit, "pty child reaped");
            }
            #[cfg(feature = "server-vt")]
            {
                if let Some(exit_tx) = reap_exit_tx {
                    // Hand the code over at once — the tab task holds it
                    // until the reader's queue is drained — then, only if
                    // the reader is STILL alive past the grace, tell it to
                    // publish anyway. `Err(Disconnected)` means the reader
                    // finished, which is the EOF path, not the deadline.
                    let _ = exit_tx.send(crate::tab_task::ExitSignal::Status(status));
                    let deadline_hit = matches!(
                        reader_alive_rx.recv_timeout(EXIT_PUBLISH_GRACE),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    );
                    if deadline_hit {
                        debug!(
                            tab_id,
                            "pty reader had not finished; the tab task publishes Exit on the \
                             deadline path"
                        );
                        let _ = exit_tx.send(crate::tab_task::ExitSignal::Deadline(status));
                    }
                    return;
                }
            }
            // Backstop for a reader that never reaches EOF (#255).
            // Ends as soon as the reader task drops its sender —
            // by then it has published `Exit` and this is a no-op —
            // or on the deadline, when publishing here is the only
            // way the tab ever reports its exit.
            let _ = reader_alive_rx.recv_timeout(EXIT_PUBLISH_GRACE);
            if publish_exit_once(&output_for_exit, &exit_published_for_wait, status) {
                debug!(
                    tab_id,
                    "pty reader had not finished; published Exit on the deadline path"
                );
            }
        });

        if let Some((session, err)) = unwanted {
            // Cancelled by close(), or too late for shutdown's sweep.
            // Kill the child rather than returning a usable receiver:
            // `terminate_child` sends SIGHUP (SIGKILL on the watchdog),
            // and the reap task started above reaps whatever the signal
            // lands on. Dropping `session` drops the input/resize
            // channels, so the writer task exits too.
            terminate_child(&session.killer, session.pid, session.latch.clone(), tab_id);
            drop(session);
            return Err(err.into());
        }

        Ok(early_rx)
    }

    /// Under server-VT the tab task is the one authority ordering input
    /// against terminal replies (architecture §3), so a tab that has one
    /// routes through it; writing straight to the writer channel would
    /// interleave keystrokes with in-flight query replies.
    pub async fn write(&self, tab_id: i64, data: Vec<u8>) -> Result<(), PtyError> {
        #[cfg(feature = "server-vt")]
        let task_tx = {
            let sessions = self.sessions.lock().unwrap();
            let session = sessions.get(&tab_id).ok_or(PtyError::NotFound(tab_id))?;
            session.tab_task.as_ref().map(|(tx, _)| tx.clone())
        };
        #[cfg(feature = "server-vt")]
        if let Some(tx) = task_tx {
            return tx
                .send(crate::tab_task::TabCmd::Input {
                    data,
                    geometry: None,
                })
                .await
                .map_err(|_| PtyError::Closed(tab_id));
        }
        let tx = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(&tab_id)
                .map(|s| s.cmd_tx.clone())
                .ok_or(PtyError::NotFound(tab_id))?
        };
        tx.send(WriterCmd::Input(data))
            .await
            .map_err(|_| PtyError::Closed(tab_id))?;
        Ok(())
    }

    /// Same authority rule as [`Self::write`]: a server-VT tab's resize
    /// must move the authoritative Terminal (and drain its mode-2048
    /// report) before `TIOCSWINSZ`, which only the tab task can order.
    ///
    /// It sends [`TabCmd::ResizeGrid`](crate::tab_task::TabCmd::ResizeGrid)
    /// rather than a full geometry: `tab.resize` states two numbers, and
    /// there is no viewport at this layer to take the cell metrics from.
    pub async fn resize(&self, tab_id: i64, cols: u16, rows: u16) -> Result<(), PtyError> {
        #[cfg(feature = "server-vt")]
        let task_tx = {
            let sessions = self.sessions.lock().unwrap();
            let session = sessions.get(&tab_id).ok_or(PtyError::NotFound(tab_id))?;
            session.tab_task.as_ref().map(|(tx, _)| tx.clone())
        };
        #[cfg(feature = "server-vt")]
        if let Some(tx) = task_tx {
            return tx
                .send(crate::tab_task::TabCmd::ResizeGrid { cols, rows })
                .await
                .map_err(|_| PtyError::Closed(tab_id));
        }
        let tx = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(&tab_id)
                .map(|s| s.cmd_tx.clone())
                .ok_or(PtyError::NotFound(tab_id))?
        };
        tx.send(WriterCmd::Resize(
            PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
            None,
        ))
        .await
        .map_err(|_| PtyError::Closed(tab_id))?;
        Ok(())
    }

    pub fn close(&self, tab_id: i64) {
        // Take the session out under the lock; release the lock
        // before invoking the killer to keep the critical section
        // short and to avoid any chance of re-entering the lock
        // from the killer impl. The waiter task spawned at
        // `spawn()` time reaps the child via `child.wait()` once
        // the kill signal lands.
        //
        // Also cancel any in-flight spawn for the same tab_id by
        // removing the entry from `pending`. spawn() re-checks
        // pending at promotion time; if the slot is gone it kills
        // the freshly-spawned child rather than installing it.
        // CR-flagged on PR #78 (`0555dd42` → `653e080`).
        //
        // The one exception is a shutdown in progress. `shutdown_all`
        // reads "entry gone from the map" as "child reaped", so removing
        // a still-running child's entry here would have it counted as
        // reaped. While `shutting_down` is set, close() sends the same
        // hangup but leaves the entry for the waiter to remove — which
        // is what shutdown's own sweep does.
        let (session, victim, was_pending) = {
            let mut sessions = self.sessions.lock().unwrap();
            let mut pending = self.pending.lock().unwrap();
            let was_pending = pending.remove(&tab_id);
            if self.shutting_down.load(Ordering::SeqCst) {
                let victim = sessions
                    .get(&tab_id)
                    .map(|session| Victim::snapshot(tab_id, session));
                (None, victim, was_pending)
            } else {
                (sessions.remove(&tab_id), None, was_pending)
            }
        };
        if let Some(session) = session {
            terminate_child(&session.killer, session.pid, session.latch.clone(), tab_id);
        } else if let Some(victim) = victim {
            victim.terminate();
        } else if was_pending {
            debug!(tab_id, "close() cancelled in-flight spawn");
        }
    }

    pub fn has(&self, tab_id: i64) -> bool {
        self.sessions.lock().unwrap().contains_key(&tab_id)
    }

    /// Tear every live PTY down and report what happened to each.
    ///
    /// Permanently latches the supervisor closed to new spawns, waits
    /// out any spawn already in flight (so its child is supervised
    /// rather than orphaned), then hangs every live tab up through the
    /// same `terminate_child` path `close()` uses — SIGHUP plus the
    /// per-tab [`KILL_GRACE`] SIGKILL watchdog, unchanged.
    ///
    /// Completion is keyed on the **session map**, not on lifecycle
    /// events: the per-spawn wait task removes a tab's entry when
    /// `child.wait()` returns, so an id leaving the map is proof its
    /// child was reaped. `SupervisorEvent`s only wake the wait early.
    /// That split is what makes the report correct past the lifecycle
    /// channel's capacity — more simultaneous exits than that makes
    /// subscribers lag, and a lost wake can only cost latency (bounded
    /// by [`SHUTDOWN_POLL_INTERVAL`]), never accuracy.
    ///
    /// Unlike `close()`, the session is left in the map for its wait
    /// task to remove; removing it here would erase the very signal the
    /// wait is keyed on. `close()` follows the same rule once the latch
    /// is set, and a `spawn` that promotes after the victim snapshot
    /// kills its own child rather than installing an unswept session.
    ///
    /// `deadline` is the soft budget for that cooperative phase. Tabs
    /// still present when it expires get a direct SIGKILL and one
    /// further [`SHUTDOWN_KILL_TAIL`] to leave the map. Nothing here
    /// ever calls `waitpid`: the per-spawn wait task is the only reaper,
    /// and racing it would consume the status it is blocked on.
    ///
    /// Concurrent calls are serialized rather than run in parallel: a
    /// second teardown escalating from its own stale snapshot could
    /// signal a pid the first already watched get reaped. The second
    /// caller waits for the first and then reports on what is left,
    /// normally nothing.
    pub async fn shutdown_all(&self, deadline: Duration) -> ShutdownReport {
        let _gate = self.shutdown_gate.lock().await;
        let start = Instant::now();
        {
            let _sessions = self.sessions.lock().unwrap();
            let _pending = self.pending.lock().unwrap();
            self.shutting_down.store(true, Ordering::SeqCst);
        }
        self.password_watch.stopped.store(true, Ordering::SeqCst);
        self.password_watch.wake.notify_one();

        // Spawns that reserved their slot before the latch still have to
        // finish: they either install a session (which the sweep below
        // then hangs up) or fail and drop the reservation. Bounded by
        // the same deadline so a wedged spawn cannot stall teardown —
        // one that promotes after the snapshot below tears itself down.
        loop {
            let settled = self.pending.lock().unwrap().is_empty();
            if settled || start.elapsed() >= deadline {
                break;
            }
            tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
        }

        let mut lifecycle = self.lifecycle.subscribe();
        // Latch `sweep_started` and read the map in one critical
        // section: a spawn promoting between the two would install a
        // session this sweep never saw and no one would tear it down.
        // The signalling itself happens with the lock released —
        // `terminate_child` makes syscalls and spawns a watchdog thread
        // per tab, and the wait tasks it is about to wake need this same
        // lock to remove their sessions.
        let mut victims: Vec<Victim> = {
            let sessions = self.sessions.lock().unwrap();
            self.sweep_started.store(true, Ordering::SeqCst);
            sessions
                .iter()
                .map(|(tab_id, session)| Victim::snapshot(*tab_id, session))
                .collect()
        };
        // Sorted once, out of the map's arbitrary order, so both the
        // target list and the escalation walk below stay in id order —
        // the report's vectors are documented sorted.
        victims.sort_unstable_by_key(|victim| victim.tab_id);
        let targets: Vec<i64> = victims.iter().map(|v| v.tab_id).collect();
        // The victims outlive the cooperative phase: their latches are
        // what the escalation below signals under.
        for victim in &victims {
            victim.hangup();
        }

        let mut remaining = targets.clone();
        self.await_removals(&mut remaining, &mut lifecycle, start + deadline)
            .await;
        let stragglers = remaining.clone();
        let mut signalled: Vec<i64> = Vec::new();
        if !stragglers.is_empty() {
            for victim in &victims {
                if !stragglers.contains(&victim.tab_id) {
                    continue;
                }
                // Re-read the map immediately before signalling: an
                // entry that has since gone means the waiter reaped that
                // child, so it is not a straggler after all. This is the
                // *reported* oracle only — what keeps the signal itself
                // off a recycled pid is the latch `sigkill` runs under.
                if !self.sessions.lock().unwrap().contains_key(&victim.tab_id) {
                    continue;
                }
                if victim.sigkill() {
                    signalled.push(victim.tab_id);
                }
            }
            self.await_removals(
                &mut remaining,
                &mut lifecycle,
                Instant::now() + SHUTDOWN_KILL_TAIL,
            )
            .await;
        }

        let abandoned = remaining;
        let killed: Vec<i64> = signalled
            .into_iter()
            .filter(|id| !abandoned.contains(id))
            .collect();
        let report = ShutdownReport {
            reaped: targets
                .into_iter()
                .filter(|id| !killed.contains(id) && !abandoned.contains(id))
                .collect(),
            killed,
            abandoned,
        };
        if !report.abandoned.is_empty() {
            warn!(
                abandoned = ?report.abandoned,
                "pty shutdown gave up on children that outlived SIGKILL"
            );
        }
        info!(
            reaped = report.reaped.len(),
            killed = report.killed.len(),
            abandoned = report.abandoned.len(),
            elapsed = ?start.elapsed(),
            "pty shutdown complete"
        );
        report
    }

    /// Drop ids from `remaining` as their sessions leave the map, until
    /// none are left or `deadline` passes. Lifecycle events are only a
    /// wake source — the map is the oracle — so a `Lagged` receiver
    /// costs at most one poll interval.
    async fn await_removals(
        &self,
        remaining: &mut Vec<i64>,
        lifecycle: &mut broadcast::Receiver<SupervisorEvent>,
        deadline: Instant,
    ) {
        loop {
            {
                let sessions = self.sessions.lock().unwrap();
                remaining.retain(|tab_id| sessions.contains_key(tab_id));
            }
            if remaining.is_empty() {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            let step = SHUTDOWN_POLL_INTERVAL.min(deadline - now);
            tokio::select! {
                () = tokio::time::sleep(step) => {}
                result = lifecycle.recv() => {
                    // A closed channel would otherwise spin this loop;
                    // it cannot happen while we hold the sender, but the
                    // wait must not depend on that.
                    if matches!(result, Err(broadcast::error::RecvError::Closed)) {
                        tokio::time::sleep(step).await;
                    }
                }
            }
        }
    }
}

/// Terminate a PTY child the way the Mac side does: SIGHUP first (via
/// portable-pty's killer, which sends SIGHUP on Unix), then a SIGKILL
/// fallback after a grace period if the child ignored the hangup.
///
/// Without the fallback a shell that traps/ignores SIGHUP outlives
/// `close()` indefinitely: portable-pty's *cloned* `ChildKiller` only
/// sends SIGHUP — the SIGKILL escalation that lives in
/// `std::process::Child::kill` is bypassed by the clone.
fn terminate_child(
    killer: &Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: Option<u32>,
    latch: Arc<ReapLatch>,
    tab_id: i64,
) {
    // The `killer` lock is taken inside the closure, so the order is
    // latch → killer, and only here. Poison-tolerant for the same reason
    // the latch is: a panic that poisoned this mutex would otherwise
    // turn every later hangup into a silent no-op, leaving the watchdog
    // to SIGKILL a child that never got the chance to run its traps.
    let mut failure = None;
    let signalled = latch.signal(|| {
        failure = killer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .kill()
            .err();
    });
    if let Some(err) = failure {
        // ESRCH (raw 3) / NotFound: child already gone — the wait task
        // has or will emit Exit. Anything else is a real failure worth
        // logging.
        let already_gone =
            err.kind() == std::io::ErrorKind::NotFound || err.raw_os_error() == Some(3);
        if !already_gone {
            warn!(tab_id, ?err, "pty SIGHUP failed");
        }
    }
    if !signalled {
        // Already reaped: the hangup was refused, and there is nothing
        // left for a watchdog to escalate against either.
        return;
    }
    let Some(pid) = pid else { return };
    // Detached watchdog: if the wait task hasn't reaped the child
    // within the grace window it ignored SIGHUP — force-kill. A plain
    // `std::thread` (not tokio) keeps `close()` callable from any
    // context regardless of runtime. SIGKILL against an
    // exited-but-unreaped zombie is harmless; the wait task reaps it.
    std::thread::spawn(move || {
        std::thread::sleep(KILL_GRACE);
        latch.signal(|| {
            // SAFETY: a pid we spawned, held unreaped by the latch for
            // the length of this closure.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        });
    });
}

/// Resolve the argv to exec. An empty argv (the plain "open a shell"
/// case) becomes the user's `$SHELL` (or `/bin/sh`), and we follow
/// Ghostty's platform split for whether it's a LOGIN shell:
///
///   * macOS → login shell (`-l`). GUI apps don't inherit the login
///     `PATH` (launchd doesn't source the profile), and the macOS dev
///     world keeps config in `.bash_profile` / `.zprofile` and expects
///     every terminal to be a login shell. So `-l` sources those and
///     puts login-only `PATH` entries (e.g. `claude`) in scope, and
///     silences the bash deprecation banner — matching Terminal.app.
///   * Linux (and other non-macOS) → non-login interactive shell. A
///     Linux login bash reads the profile chain and STOPS at the first
///     of `.bash_profile` / `.bash_login` / `.profile`, so a stray
///     `.bash_profile` (e.g. one a tool's installer drops in) shadows
///     `.profile` and the interactive `~/.bashrc` — where prompts,
///     aliases, and color usually live — never loads. Ghostty launches
///     a non-login shell everywhere but macOS for exactly this reason
///     ("No other platform behaves this way", `Exec.zig`); a Linux
///     desktop session already exports the login `PATH` before Roost
///     starts, so there's nothing to recover with `-l`. roost.bash's
///     non-login branch then sources `/etc/bash.bashrc` + `~/.bashrc`.
///
/// A non-empty argv (launcher commands) is passed through verbatim.
/// (`portable-pty` 0.8 couples program and argv[0], so we use the `-l`
/// flag rather than the `-bash` dash-prefix login convention.)
fn resolve_argv(argv: &[String], shell: &str) -> Vec<String> {
    if argv.is_empty() {
        if cfg!(target_os = "macos") {
            vec![shell.to_string(), "-l".to_string()]
        } else {
            vec![shell.to_string()]
        }
    } else {
        argv.to_vec()
    }
}

/// Whether to auto-bootstrap a modern bash: add `--posix` + point ENV at
/// roost.bash (see `bash_bootstrap_env` and roost.bash's inject header).
/// True iff argv[0] is a `bash`, it isn't Apple's SIP-locked `/bin/bash`
/// (3.2 — its ENV+POSIX path is patched out, so it keeps the documented
/// manual source), and the only extra args are plain login/interactive
/// flags (`-l`/`-i`). That admits the default-shell case (`[$SHELL, -l]`)
/// and an explicit `[bash, -l]`, but passes launcher commands (`-c`,
/// `--norc`, `--rcfile`, …) and an already-`--posix` argv through
/// untouched — forcing `--posix` onto those would change their semantics.
fn bash_autobootstrap(resolved: &[String], is_macos: bool) -> bool {
    let Some(arg0) = resolved.first() else {
        return false;
    };
    if std::path::Path::new(arg0)
        .file_name()
        .and_then(|n| n.to_str())
        != Some("bash")
    {
        return false;
    }
    if is_macos && arg0 == "/bin/bash" {
        return false;
    }
    resolved[1..].iter().all(|a| a == "-l" || a == "-i")
}

/// Insert `--posix` where bash needs it — right after argv[0], before the
/// short `-l`/`-i` flags. bash rejects a GNU long option that follows a
/// short one (`bash -l --posix` errors with `--: invalid option`), so the
/// long option goes first. Returns `resolved` unchanged when `apply` is
/// false.
fn with_bash_posix(mut resolved: Vec<String>, apply: bool) -> Vec<String> {
    if apply {
        resolved.insert(1, "--posix".to_string());
    }
    resolved
}

/// The env vars to overlay when auto-bootstrapping bash (see roost.bash's
/// inject header). `existing_env`/`existing_histfile` are the child's
/// inherited values. ENV points bash at roost.bash; ROOST_BASH_INJECT="1"
/// tells it to recreate startup (and distinguishes an auto-load from a
/// manual source). A prior ENV is preserved into ROOST_BASH_ENV so the
/// shim can restore it. HISTFILE is pinned to ~/.bash_history (POSIX mode
/// would otherwise default it to ~/.sh_history) only when fully unset, with
/// ROOST_BASH_UNEXPORT_HISTFILE telling the shim to un-export it afterward.
/// An *empty* HISTFILE is left alone — that's the idiom for disabling
/// history, so we must not re-enable it (matches Ghostty's null-only check).
fn bash_bootstrap_env(
    resources_dir: &std::path::Path,
    existing_env: Option<&str>,
    existing_histfile: Option<&str>,
    home: Option<&str>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(prev) = existing_env.filter(|v| !v.is_empty()) {
        out.push(("ROOST_BASH_ENV".into(), prev.to_string()));
    }
    let script = resources_dir.join("shell-integration").join("roost.bash");
    out.push(("ENV".into(), script.to_string_lossy().into_owned()));
    out.push(("ROOST_BASH_INJECT".into(), "1".into()));
    if existing_histfile.is_none() {
        if let Some(home) = home.filter(|h| !h.is_empty()) {
            out.push(("HISTFILE".into(), format!("{home}/.bash_history")));
            out.push(("ROOST_BASH_UNEXPORT_HISTFILE".into(), "1".into()));
        }
    }
    out
}

/// Current working directory of `pid`. Linux reads `/proc/<pid>/cwd`;
/// macOS asks libproc for `PROC_PIDVNODEPATHINFO`.
#[cfg(target_os = "linux")]
fn cwd_of_pid(pid: u32) -> Option<String> {
    proc_read_starts("cwd");
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
}

#[cfg(target_os = "macos")]
fn cwd_of_pid(pid: u32) -> Option<String> {
    use std::ffi::CStr;
    use std::mem::{size_of, MaybeUninit};

    proc_read_starts("cwd");
    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = i32::try_from(size_of::<libc::proc_vnodepathinfo>()).ok()?;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes for the
    // structure requested by PROC_PIDVNODEPATHINFO. We read it only when
    // libproc reports that it initialized the complete structure. The SDK
    // defines `vip_path` as a NUL-terminated MAXPATHLEN char array.
    let written = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the full structure was initialized above and `vip_path` is a
    // fixed C string buffer supplied by libproc.
    let path = unsafe {
        CStr::from_ptr(
            info.assume_init_ref()
                .pvi_cdir
                .vip_path
                .as_ptr()
                .cast::<libc::c_char>(),
        )
        .to_str()
        .ok()?
    };
    (!path.is_empty()).then(|| path.to_string())
}

#[cfg(test)]
type ProcReadHook = Box<dyn FnMut(&'static str)>;

#[cfg(test)]
thread_local! {
    /// Run at the start of every native read of a process on this
    /// thread, with what it reads, so a test can see which locks that
    /// read runs under.
    static PROC_READ_HOOK: std::cell::RefCell<Option<ProcReadHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Where every native read of a process begins. Does nothing outside
/// tests.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn proc_read_starts(what: &'static str) {
    #[cfg(test)]
    PROC_READ_HOOK.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(what);
        }
    });
    #[cfg(not(test))]
    let _ = what;
}

/// Who a process is, as far as the foreground-leader read needs it.
/// Linux's start time has no sub-second part, so it reads
/// `(starttime, 0)`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcStamp {
    pid: i64,
    pgrp: i64,
    session: i64,
    tty: i64,
    tpgid: i64,
    start: (u64, u64),
}

/// Whether the stamps taken before and after a leader's cwd read are one
/// process, leading its own group, in the child's session and on the
/// child's terminal. The start time is what tells a reused pid apart.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn same_leader(before: &ProcStamp, after: &ProcStamp, child_sid: i64, child_tty: i64) -> bool {
    [before, after].iter().all(|stamp| {
        stamp.pgrp == stamp.pid && stamp.session == child_sid && stamp.tty == child_tty
    }) && before.pid == after.pid
        && before.start == after.start
}

/// [`PtySupervisor::native_cwds`] of the child `child`, whose reap latch
/// is `latch`: the foreground leader's cwd, then the child's.
///
/// Nothing but the latch's liveness check runs under it. Every read —
/// the child's stat, the leader's two [`ProcStamp`]s, both cwds — runs
/// between two such checks, because the reaper takes the latch and any
/// of those reads can wait: on macOS a path read waits on the filesystem
/// holding the cwd (a hung NFS mount) and `proc_pidinfo` on a process
/// mid-exec, and on Linux a stat read takes the target's exec lock. Held
/// across one, the latch would strand every `close()`, SIGKILL
/// escalation, reap and [`PtySupervisor::shutdown_all`] behind it. The
/// child's reads are still its own: it was unreaped at both checks, and
/// reaping closes the latch before it releases the pid and never reopens
/// it, so the pid was not freed in between. A child reaped by the second
/// check answers nothing — its pid may be another process's by then.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn native_cwds_of(child: u32, latch: &ReapLatch) -> Vec<String> {
    if latch.read(|| ()).is_none() {
        return Vec::new();
    }
    let leader = ForegroundLeader::find(child);
    let leader_cwd = leader.as_ref().and_then(ForegroundLeader::cwd);
    let child_cwd = cwd_of_pid(child);
    let leader_unchanged = leader.as_ref().is_some_and(ForegroundLeader::unchanged);
    if latch.read(|| ()).is_none() {
        return Vec::new();
    }
    leader_cwd
        .filter(|_| leader_unchanged)
        .into_iter()
        .chain(child_cwd)
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn native_cwds_of(_child: u32, _latch: &ReapLatch) -> Vec<String> {
    Vec::new()
}

/// The leader of the terminal's foreground process group, when that is
/// not the child itself — a job the shell is running, or a nested shell —
/// with the [`ProcStamp`] taken when it was found.
///
/// The child's own `tpgid` names the group, with no master fd to keep:
/// portable-pty makes every child a session leader with the pty as its
/// controlling terminal. The leader is not our child, though, and
/// nothing holds its pid, so its cwd counts only when [`same_leader`]
/// matches the stamps taken before and after the read. Asking the
/// terminal for its foreground group again would not do: a dead job's
/// group stays the foreground one until the shell calls `tcsetpgrp`, and
/// its pid can be reused meanwhile. A leader this user can't read
/// (`sudo -s`) has no cwd, like any failed read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ForegroundLeader {
    process: LeaderProc,
    before: ProcStamp,
    child_sid: i64,
    child_tty: i64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ForegroundLeader {
    fn find(child: u32) -> Option<Self> {
        let own = proc_stamp(child)?;
        let pid = u32::try_from(own.tpgid)
            .ok()
            .filter(|&pid| pid != 0 && pid != child)?;
        let process = LeaderProc::open(pid)?;
        let before = process.stamp()?;
        Some(Self {
            process,
            before,
            child_sid: i64::from(child),
            child_tty: own.tty,
        })
    }

    fn cwd(&self) -> Option<String> {
        self.process.cwd()
    }

    /// Whether a second stamp still matches the first, by [`same_leader`].
    fn unchanged(&self) -> bool {
        self.process
            .stamp()
            .is_some_and(|after| same_leader(&self.before, &after, self.child_sid, self.child_tty))
    }
}

/// A `/proc/<pid>/stat` line's [`ProcStamp`]. The fields are counted
/// after the LAST `)`: the command name before it can hold spaces and
/// parens of its own.
#[cfg(target_os = "linux")]
fn parse_proc_stat(stat: &str) -> Option<ProcStamp> {
    let (head, tail) = stat.rsplit_once(')')?;
    let (pid, _) = head.split_once(" (")?;
    // `tail` starts at field 3 in proc(5)'s numbering.
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let field = |n: usize| fields.get(n - 3).copied();
    Some(ProcStamp {
        pid: pid.trim().parse().ok()?,
        pgrp: field(5)?.parse().ok()?,
        session: field(6)?.parse().ok()?,
        tty: field(7)?.parse().ok()?,
        tpgid: field(8)?.parse().ok()?,
        start: (field(22)?.parse().ok()?, 0),
    })
}

#[cfg(target_os = "linux")]
fn proc_stamp(pid: u32) -> Option<ProcStamp> {
    proc_read_starts("stat");
    parse_proc_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// A leader read through one `/proc/<pid>` directory fd — its stamps and
/// its cwd alike — which never reaches a later process given the same
/// pid.
#[cfg(target_os = "linux")]
struct LeaderProc(File);

#[cfg(target_os = "linux")]
impl LeaderProc {
    fn open(pid: u32) -> Option<Self> {
        proc_read_starts("open");
        File::open(format!("/proc/{pid}")).ok().map(Self)
    }

    fn stamp(&self) -> Option<ProcStamp> {
        use std::os::fd::FromRawFd;

        proc_read_starts("stat");
        // SAFETY: `self.0` is an open directory fd for the whole call,
        // and the name is a NUL-terminated literal.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c"stat".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        // SAFETY: `openat` just returned this fd, and nothing else owns it.
        let file = unsafe { File::from_raw_fd(fd) };
        parse_proc_stat(&std::io::read_to_string(file).ok()?)
    }

    fn cwd(&self) -> Option<String> {
        proc_read_starts("cwd");
        let mut buf = [0u8; libc::PATH_MAX as usize];
        // SAFETY: `buf` is writable for its full length, `self.0` is an
        // open directory fd, and the name is a NUL-terminated literal.
        let len = unsafe {
            libc::readlinkat(
                self.0.as_raw_fd(),
                c"cwd".as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        // A link that fills the buffer may have been cut short.
        let len = usize::try_from(len).ok().filter(|&len| len < buf.len())?;
        std::str::from_utf8(&buf[..len]).ok().map(str::to_owned)
    }
}

#[cfg(target_os = "macos")]
fn proc_stamp(pid: u32) -> Option<ProcStamp> {
    use std::mem::{size_of, MaybeUninit};

    proc_read_starts("stat");
    let raw = i32::try_from(pid).ok()?;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes for the
    // structure requested by PROC_PIDTBSDINFO, read only when libproc
    // reports that it initialized all of it.
    let written = unsafe {
        libc::proc_pidinfo(
            raw,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: libproc initialized the full structure above.
    let info = unsafe { info.assume_init() };
    // `proc_bsdinfo` carries no session id.
    // SAFETY: `getsid` reads nothing but the pid it is given.
    let session = unsafe { libc::getsid(raw) };
    (session >= 0).then(|| ProcStamp {
        pid: i64::from(info.pbi_pid),
        pgrp: i64::from(info.pbi_pgid),
        session: i64::from(session),
        tty: i64::from(info.e_tdev),
        tpgid: i64::from(info.e_tpgid),
        start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
    })
}

/// A leader read by pid. Nothing pins the pid; the stamps either side of
/// its cwd read are what tell a reused one apart.
#[cfg(target_os = "macos")]
struct LeaderProc(u32);

#[cfg(target_os = "macos")]
impl LeaderProc {
    fn open(pid: u32) -> Option<Self> {
        Some(Self(pid))
    }

    fn stamp(&self) -> Option<ProcStamp> {
        proc_stamp(self.0)
    }

    fn cwd(&self) -> Option<String> {
        cwd_of_pid(self.0)
    }
}

/// Shell-integration scripts, embedded at build time. The Mac copy under
/// mac/Sources/Roost/Resources/shell-integration/ is frozen for this
/// release and has diverged from these.
const ROOST_BASH: &str = include_str!("../resources/shell-integration/roost.bash");
const ROOST_ZSH: &str = include_str!("../resources/shell-integration/roost.zsh");
const ROOST_ZSH_ZDOTENV: &str = include_str!("../resources/shell-integration/zsh/.zshenv");

/// Write the embedded shell-integration scripts to a stable cache dir and
/// return that dir — the value of `ROOST_RESOURCES_DIR` (scripts live at
/// `<dir>/shell-integration/`). Written once per process; `None` if the
/// cache dir can't be resolved or written.
fn roost_resources_dir() -> Option<&'static std::path::Path> {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(std::path::PathBuf::from)
            // XDG: a relative cache path is invalid — ignore it and fall
            // back to $HOME/.cache rather than writing relative to cwd.
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache"))
            })?;
        let root = base.join("roost");
        let si = root.join("shell-integration");
        std::fs::create_dir_all(&si).ok()?;
        std::fs::write(si.join("roost.bash"), ROOST_BASH).ok()?;
        std::fs::write(si.join("roost.zsh"), ROOST_ZSH).ok()?;
        // zsh ZDOTDIR shim (auto-bootstrap): <si>/zsh/.zshenv
        let zsh_dir = si.join("zsh");
        std::fs::create_dir_all(&zsh_dir).ok()?;
        std::fs::write(zsh_dir.join(".zshenv"), ROOST_ZSH_ZDOTENV).ok()?;
        Some(root)
    })
    .as_deref()
}

fn build_command(
    cwd: &str,
    argv: &[String],
    tab_id: i64,
    socket_path: &std::path::Path,
) -> CommandBuilder {
    // Argv-first: never call a shell to parse a single command string.
    // An empty argv (plain "open a shell") resolves to the user's
    // `$SHELL` — a login shell (`-l`) on macOS, a non-login interactive
    // shell on Linux. See `resolve_argv`.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let resolved = resolve_argv(argv, &shell);
    // Modern bash: add `--posix` so it honors ENV (its only
    // per-interactive-shell hook), which we point at roost.bash below.
    // `--posix` and the ENV injection MUST be applied together — a `--posix`
    // shell with no ENV would be stuck in POSIX mode with no startup files
    // and no recreation — so gate both on the resources dir being writable
    // (if the cache write failed there's no roost.bash to source).
    let resources_dir = roost_resources_dir();
    let bash_boot =
        resources_dir.is_some() && bash_autobootstrap(&resolved, cfg!(target_os = "macos"));
    let resolved = with_bash_posix(resolved, bash_boot);
    let mut cmd = CommandBuilder::new(&resolved[0]);
    for a in &resolved[1..] {
        cmd.arg(a);
    }
    if !cwd.is_empty() {
        cmd.cwd(cwd);
    }
    // Advertise the terminal Roost provides — force TERM rather than
    // inheriting the launching terminal's (a child seeing an inherited
    // TERM=tmux-256color / xterm-kitty would emit unsupported sequences).
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // Forcing TERM makes an inherited TERMINFO wrong: it points at the
    // launching terminal's private DB (e.g. Ghostty's, which has no
    // xterm-256color entry), so strict $TERMINFO readers would find no
    // entry for the TERM Roost advertises.
    cmd.env_remove("TERMINFO");
    // Advertise OSC 8 hyperlink support. Roost renders + opens OSC 8
    // links (Ctrl-click), but the `supports-hyperlinks` library many CLIs
    // gate on — Claude Code, anything on chalk/terminal-link — only
    // allowlists known terminals by TERM_PROGRAM, and "Roost" isn't one.
    // Without this they emit plain text instead of a link (e.g. Claude
    // Code's footer "PR #N"). FORCE_HYPERLINK is that ecosystem's "my
    // terminal supports it" override; honest here because we genuinely do.
    cmd.env("FORCE_HYPERLINK", "1");
    // Roost contract (documented in docs/reference/paths.md and the
    // refactor plan's acceptance criteria): every shell Roost spawns
    // sees its tab id and the IPC socket path, so `roostctl` invoked
    // from inside the tab dials the correct UI and routes
    // notifications back to the originating tab without needing a
    // wider env discovery.
    cmd.env("ROOST_TAB_ID", tab_id.to_string());
    cmd.env("ROOST_SOCKET", socket_path.as_os_str());
    // The one hook entrypoint every installed agent config invokes
    // (plan 046 §3.2). Indirecting through the environment is what keeps
    // the written config identical on every machine and on every host —
    // it names no path of Roost's. Omitted rather than guessed when
    // nothing resolves: the installed command's fallback branch reads an
    // *unset* variable as "not inside Roost", and a path that does not
    // exist would instead exec-fail with no JSON on stdout.
    // An inherited value is dropped rather than passed through — it must
    // always describe *this* Roost, never an outer one this build
    // happens to be running inside.
    match crate::process::agent_hook_binary() {
        Some(hook) => cmd.env("ROOST_AGENT_HOOK", hook),
        None => cmd.env_remove("ROOST_AGENT_HOOK"),
    }
    // Roost shell-integration contract (parity with the Mac UI). TERM
    // stays xterm-256color (above). ROOST_SHELL_FEATURES is overridable.
    cmd.env("TERM_PROGRAM", "Roost");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env("ROOST_SHELL_INTEGRATION", "1");
    if std::env::var_os("ROOST_SHELL_FEATURES").is_none() {
        cmd.env("ROOST_SHELL_FEATURES", "cwd,title,marks,prompt,ssh-env");
    }
    if let Some(dir) = resources_dir {
        cmd.env("ROOST_RESOURCES_DIR", dir);
        // Auto-bootstrap the shipped integration with no rc edit (parity
        // with the Mac UI):
        //   * zsh: point ZDOTDIR at our shim — it restores the user's
        //     ZDOTDIR, runs their startup, then loads roost.zsh.
        //   * modern bash: set ENV + ROOST_BASH_INJECT so the `--posix`
        //     shell sources roost.bash, which recreates startup then loads
        //     the integration (see `bash_bootstrap_env`).
        let is_zsh = std::path::Path::new(&resolved[0])
            .file_name()
            .and_then(|n| n.to_str())
            == Some("zsh");
        if is_zsh {
            if let Some(z) = std::env::var_os("ZDOTDIR") {
                cmd.env("ROOST_ZSH_ZDOTDIR", z);
            }
            cmd.env("ZDOTDIR", dir.join("shell-integration").join("zsh"));
        } else if bash_boot {
            for (key, value) in bash_bootstrap_env(
                dir,
                std::env::var("ENV").ok().as_deref(),
                std::env::var("HISTFILE").ok().as_deref(),
                std::env::var("HOME").ok().as_deref(),
            ) {
                cmd.env(key, value);
            }
        }
    }
    cmd
}

/// Publish a tab's `Exit` event, at most once per spawn. Both the
/// reader task (the normal path) and the reap task's deadline backstop
/// call this; the compare-exchange decides which one gets to send, so
/// a consumer never sees two exits for one child. Returns whether this
/// call was the one that published.
fn publish_exit_once(output: &OutputPublisher, published: &AtomicBool, status: i32) -> bool {
    if published
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    output.send_exit(status);
    true
}

/// Blocking reads off the master fd until EOF or a hard error, handing
/// each chunk to `sink`. `sink` returns whether to keep reading, so a
/// consumer that has gone away stops the loop.
///
/// The two consumers are the default publisher and — under server-VT —
/// the tab task's bounded channel; sharing the loop keeps the read,
/// EOF and `Interrupted` handling identical for both.
fn pty_reader_loop(mut reader: File, tab_id: i64, mut sink: impl FnMut(Vec<u8>) -> bool) {
    let mut buf = vec![0u8; PTY_OUTPUT_CHUNK_SIZE];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                debug!(tab_id, "pty reached EOF");
                return;
            }
            Ok(n) => {
                // A partially filled chunk stays open for COALESCE_WINDOW
                // from its first byte before it is published. A streaming
                // child hands the line discipline one line at a time, and
                // a reader woken per line published ~40-byte events —
                // twice what the blocking reader did, measured — which is
                // what fills the 256-slot broadcast on a slow consumer.
                // The window is a deadline, not an idle gap: a trickle
                // cannot hold its first byte past it, and an interactive
                // echo pays it once.
                let mut filled = n;
                let deadline = Instant::now() + COALESCE_WINDOW;
                while filled < buf.len() {
                    match reader.read(&mut buf[filled..]) {
                        Ok(0) => break,
                        Ok(m) => filled += m,
                        Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                        Err(err) if err.kind() == ErrorKind::WouldBlock => {
                            let left = deadline.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                break;
                            }
                            // `poll` counts whole milliseconds; a
                            // sub-millisecond remainder still waits one.
                            let ms = i32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(1);
                            if !matches!(wait_readable_for(reader.as_raw_fd(), ms), Ok(true)) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                if !sink(buf[..filled].to_vec()) {
                    return;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            // The master is O_NONBLOCK for the writer's sake (#409); the
            // reader waits in `poll(2)` instead. Level-triggered, so
            // input landing between the read and the poll is not missed.
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                match wait_readable(reader.as_raw_fd()) {
                    Ok(true) => continue,
                    // A hang-up with nothing left to read. Neither kernel
                    // produces this today (Linux answers EIO, macOS 0),
                    // but it is the one shape that would spin.
                    Ok(false) => {
                        debug!(tab_id, "pty hung up");
                        return;
                    }
                    Err(err) => {
                        debug!(tab_id, ?err, "pty poll error, stopping reader");
                        return;
                    }
                }
            }
            // Linux's answer once the slave side is gone; portable-pty's
            // reader used to fold it into the EOF case.
            Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                debug!(tab_id, "pty reached EOF");
                return;
            }
            Err(err) => {
                debug!(tab_id, ?err, "pty read error, stopping reader");
                return;
            }
        }
    }
}

/// How long a partially filled chunk stays open for more output, counted
/// from its first byte. A streaming child fills the chunk well inside
/// this; an interactive echo pays it once.
const COALESCE_WINDOW: Duration = Duration::from_millis(1);

/// `true` once there is input to read; `false` on a hang-up or error
/// with no input pending.
fn wait_readable(fd: RawFd) -> std::io::Result<bool> {
    wait_readable_for(fd, -1)
}

/// `wait_readable` with a timeout in milliseconds (`-1` = none): `Ok(false)`
/// also when the timeout expires with nothing to read. The loop exists
/// only to retry `EINTR`.
fn wait_readable_for(fd: RawFd, timeout_ms: i32) -> std::io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one `pollfd`, passed with a count of one; the fd is
        // owned by the caller's `File` for the duration of the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(rc > 0 && pfd.revents & libc::POLLIN != 0);
    }
}

/// Write all of `data` to the non-blocking master, waiting on the
/// reactor between partial writes.
///
/// The hang-up check runs BEFORE the write on purpose: after the slave
/// hangs up, Linux answers a write with EAGAIN rather than an error, and
/// tokio keeps the closed readiness through `clear_ready`, so `writable`
/// returns at once every time — a loop that checked afterwards would
/// spin on EAGAIN for as long as the task lived (#409). macOS reaches
/// the same exit through EIO.
async fn write_all_nonblocking(fd: &AsyncFd<File>, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let mut guard = fd.writable().await?;
        if guard.ready().is_write_closed() {
            return Err(std::io::Error::new(
                ErrorKind::BrokenPipe,
                "pty slave hung up",
            ));
        }
        match guard.try_io(|inner| {
            let mut file: &File = inner.get_ref();
            file.write(data)
        }) {
            Ok(Ok(0)) => return Err(ErrorKind::WriteZero.into()),
            Ok(Ok(n)) => data = &data[n..],
            Ok(Err(err)) if err.kind() == ErrorKind::Interrupted => continue,
            Ok(Err(err)) => return Err(err),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: plain `fcntl` calls on a valid fd the caller owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A CLOEXEC dup of the master as a `File`. Taken before the child
/// exists, so no master fd ever reaches it.
fn dup_master(fd: RawFd) -> std::io::Result<File> {
    // SAFETY: `fd` is the master's, owned by `pair.master`, which
    // outlives this call.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    Ok(File::from(borrowed.try_clone_to_owned()?))
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("pty for tab {0} not found")]
    NotFound(i64),
    #[error("pty for tab {0} is closed")]
    Closed(i64),
    #[error("tab {0} already has a live pty session")]
    DuplicateTab(i64),
    /// The spawn lost its tab. Either a `close()` took the reservation
    /// back between the `pending` insert and the promotion, or the
    /// workspace row vanished *before* the reservation and the opener's
    /// re-check ([`crate::application::spawn_for_row`]) caught it after
    /// the fact. Both mean the same thing to a caller: the tab is gone.
    #[error("spawn for tab {0} cancelled by close()")]
    Cancelled(i64),
    #[error("supervisor is shutting down; refused to spawn tab {0}")]
    ShuttingDown(i64),
}

// A PATH lookup only — never executes `bin`, so an inherited BASH_ENV or
// a zsh startup file (`~/.zshenv` etc., which `zsh -f` still reads for a
// login/interactive shell) can't run as a side effect of merely checking
// presence.
#[cfg(test)]
fn shell_present(bin: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(bin);
        std::fs::metadata(&candidate)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// Spawn `bash --norc --noprofile -i` for `tab_id` in `shell_dir` and
/// have it run `(cd job_dir && printf 'JOB_%s\n' READY && exec sleep 30)`
/// as its foreground job, returning once the job has printed
/// `JOB_READY` and [`PtySupervisor::foreground_cwd`] reads `job_dir`.
/// The `%s` split is what keeps the terminal's echo of the command line
/// from matching. The tab hangs up when the answer drops; `None`, after
/// saying why, without bash on PATH.
#[cfg(test)]
pub(crate) async fn spawn_foreground_job(
    supervisor: &Arc<PtySupervisor>,
    tab_id: i64,
    shell_dir: &std::path::Path,
    job_dir: &std::path::Path,
) -> Option<crate::application::HangUp> {
    if !shell_present("bash") {
        eprintln!("skipping the foreground-job test: bash not found on PATH");
        return None;
    }
    let hang_up = crate::application::HangUp(supervisor.clone(), vec![tab_id]);
    let argv = ["bash", "--norc", "--noprofile", "-i"].map(String::from);
    let socket = std::path::Path::new("/tmp/roost-foreground-job-test.sock");
    let mut rx = supervisor
        .spawn(tab_id, &shell_dir.to_string_lossy(), &argv, 80, 24, socket)
        .expect("spawn bash");
    let mut seen = Vec::new();
    // The first prompt: the line editor is up, so the job isn't typed
    // into a shell still starting.
    wait_for_output(&mut rx, &mut seen, &[b"$ ", b"# "]).await;
    let command = format!(
        "(cd '{}' && printf 'JOB_%s\\n' READY && exec sleep 30)\n",
        job_dir.display()
    );
    supervisor
        .write(tab_id, command.into_bytes())
        .await
        .expect("type the job");
    wait_for_output(&mut rx, &mut seen, &[b"JOB_READY"]).await;

    let job = canonical(job_dir);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let read = supervisor.foreground_cwd(tab_id);
        if read.as_deref() == Some(job.as_str()) {
            return Some(hang_up);
        }
        assert!(
            Instant::now() < deadline,
            "the foreground cwd stayed {read:?}, never the job's {job}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
fn canonical(dir: &std::path::Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Read `rx` into `seen` until it holds one of `needles`; panics if the
/// tab exits or 20 s pass first.
#[cfg(test)]
async fn wait_for_output(
    rx: &mut broadcast::Receiver<PtyOutputEvent>,
    seen: &mut Vec<u8>,
    needles: &[&[u8]],
) {
    let holds = |seen: &[u8]| {
        needles
            .iter()
            .any(|needle| seen.windows(needle.len()).any(|window| window == *needle))
    };
    let found = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await {
                Ok(PtyOutputEvent::Bytes { data, .. }) => {
                    seen.extend_from_slice(&data);
                    if holds(seen) {
                        return true;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Ok(PtyOutputEvent::Exit { .. }) | Err(_) => return false,
            }
        }
    })
    .await;
    assert_eq!(
        found,
        Ok(true),
        "the tab never printed one of {:?}: {:?}",
        needles
            .iter()
            .map(|needle| String::from_utf8_lossy(needle))
            .collect::<Vec<_>>(),
        String::from_utf8_lossy(seen)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    // ---- the password poller (plan 074 §D2) -------------------------

    /// A PTY pair with nothing on either end: the master is what the
    /// poller samples, and the slave is where a test sets the line
    /// discipline a child at a prompt would.
    fn pty_pair() -> (OwnedFd, OwnedFd) {
        use std::os::fd::FromRawFd;

        let (mut master, mut slave) = (-1, -1);
        // SAFETY: `openpty` fills the two fds; nothing else is passed.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: both fds are fresh and owned by nothing else.
        unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
    }

    fn set_line_discipline(slave: &OwnedFd, canonical: bool, echo: bool) {
        // SAFETY: `tcgetattr`/`tcsetattr` read and write the `termios`
        // handed to them, on an fd this test owns.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut termios), 0);
            for (flag, on) in [(libc::ICANON, canonical), (libc::ECHO, echo)] {
                if on {
                    termios.c_lflag |= flag;
                } else {
                    termios.c_lflag &= !flag;
                }
            }
            assert_eq!(
                libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &termios),
                0,
                "tcsetattr: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    fn probe(tab_id: i64, incarnation: u64, fd: &Arc<OwnedFd>) -> PasswordProbe {
        PasswordProbe {
            tab_id,
            incarnation,
            fd: Arc::downgrade(fd),
        }
    }

    /// `(tab_id, incarnation, password)` per report, so a mismatch
    /// prints the sequence rather than a `Debug` of the enum.
    fn reports(events: Vec<SupervisorEvent>) -> Vec<(i64, u64, bool)> {
        events
            .into_iter()
            .map(|event| match event {
                SupervisorEvent::PasswordInput {
                    tab_id,
                    incarnation,
                    password,
                } => (tab_id, incarnation, password),
                other => panic!("the sampler reported a non-password event: {other:?}"),
            })
            .collect()
    }

    /// Ghostty's heuristic on a real line discipline, through all four
    /// `ICANON`×`ECHO` combinations: only line mode with echo off is a
    /// password prompt, and each move into or out of it is reported once.
    #[test]
    fn only_line_mode_with_echo_off_reads_as_a_password_prompt() {
        let (master, slave) = pty_pair();
        let master = Arc::new(master);
        let mut sampler = PasswordSampler::default();
        let mut seen = Vec::new();
        for (canonical, echo) in [
            (true, true),
            (true, false),
            (true, false),
            (false, false),
            (false, true),
            (true, false),
            (true, true),
            (true, true),
        ] {
            set_line_discipline(&slave, canonical, echo);
            assert_eq!(
                at_password_prompt(master.as_fd()).expect("tcgetattr on the master"),
                canonical && !echo,
                "ICANON={canonical} ECHO={echo}"
            );
            seen.extend(reports(sampler.tick(&[probe(3, 1, &master)])));
        }
        assert_eq!(
            seen,
            vec![
                (3, 1, false),
                (3, 1, true),
                (3, 1, false),
                (3, 1, true),
                (3, 1, false)
            ],
            "the spawn's first sample, then one report per move into or out \
             of a prompt — none for the other three combinations or a \
             repeated sample"
        );
    }

    /// A master `tcgetattr` fails on reads as no prompt, and says so in
    /// the log once for that spawn, not once per tick.
    #[test]
    fn a_failing_tcgetattr_reads_as_no_prompt_and_logs_once() {
        let (master, slave) = pty_pair();
        let master = Arc::new(master);
        let not_a_tty = Arc::new(OwnedFd::from(File::open("/dev/null").expect("/dev/null")));
        set_line_discipline(&slave, true, false);

        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || CapturedLog(Arc::clone(&writer)))
            .finish();
        let seen = tracing::subscriber::with_default(subscriber, || {
            let mut sampler = PasswordSampler::default();
            let mut seen = reports(sampler.tick(&[probe(3, 1, &master)]));
            for _ in 0..3 {
                seen.extend(reports(sampler.tick(&[probe(3, 1, &not_a_tty)])));
            }
            seen
        });

        assert_eq!(seen, [(3, 1, true), (3, 1, false)]);
        let log = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert_eq!(
            log.matches("tcgetattr on the pty master failed").count(),
            1,
            "{log}"
        );
    }

    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A tab id respawned under a raised flag: the replacement's first
    /// sample is reported under its own incarnation even when it agrees
    /// with the last thing reported for the tab — whether that was the
    /// predecessor's own clear or nothing at all, because a tick with
    /// neither spawn in the map came between them.
    #[test]
    fn a_respawns_first_sample_is_reported_whatever_came_before_it() {
        let (old_master, old_slave) = pty_pair();
        let (new_master, _new_slave) = pty_pair();
        let (old_master, new_master) = (Arc::new(old_master), Arc::new(new_master));
        let exited = Arc::new(OwnedFd::from(File::open("/dev/null").expect("/dev/null")));
        set_line_discipline(&old_slave, true, false);

        for gap in [vec![probe(3, 1, &exited)], Vec::new()] {
            let mut sampler = PasswordSampler::default();
            assert_eq!(
                reports(sampler.tick(&[probe(3, 1, &old_master)])),
                [(3, 1, true)]
            );
            sampler.tick(&gap);
            assert_eq!(
                reports(sampler.tick(&[probe(3, 2, &new_master)])),
                [(3, 2, false)],
                "a respawn after {} reported nothing",
                if gap.is_empty() {
                    "an empty tick"
                } else {
                    "its predecessor's clear"
                }
            );
            assert_eq!(reports(sampler.tick(&[probe(3, 2, &new_master)])), []);
        }
    }

    /// The whole sequence through the owner: A raises the flag and exits,
    /// and B takes the tab id before A's clear is delivered, so that
    /// clear is stale and A's exit is not B's. B's first sample is what
    /// leaves the row down — the plan's "a new incarnation starts false",
    /// end to end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_respawn_clears_the_prompt_its_predecessor_left() {
        use crate::application::{apply_password_report, spawn_in};

        let workspace = crate::Workspace::new();
        let project = workspace.create_project("p", "/tmp").unwrap().id;
        let tab = workspace.open_tab(project, "/tmp", "", true).unwrap().id;
        let supervisor = Arc::new(PtySupervisor::new());
        let apply = |events: Vec<SupervisorEvent>| {
            for event in &events {
                apply_password_report(&workspace, &supervisor, event);
            }
        };
        let (a_master, a_slave) = pty_pair();
        let (b_master, _b_slave) = pty_pair();
        let (a_master, b_master) = (Arc::new(a_master), Arc::new(b_master));
        let exited = Arc::new(OwnedFd::from(File::open("/dev/null").expect("/dev/null")));
        set_line_discipline(&a_slave, true, false);
        let mut sampler = PasswordSampler::default();

        let _a = spawn_in(&supervisor, tab, std::path::Path::new("/tmp"), COOPERATIVE);
        let a = supervisor.incarnation(tab).expect("A is live");
        apply(sampler.tick(&[probe(tab, a, &a_master)]));
        assert!(workspace.tab(tab).unwrap().password_input);

        let a_cleared = sampler.tick(&[probe(tab, a, &exited)]);
        supervisor.close(tab);
        let _b = spawn_in(&supervisor, tab, std::path::Path::new("/tmp"), COOPERATIVE);
        let b = supervisor.incarnation(tab).expect("B is live");
        apply(a_cleared);
        apply(vec![SupervisorEvent::TabExited {
            tab_id: tab,
            status: 0,
        }]);
        apply(sampler.tick(&[probe(tab, b, &b_master)]));
        assert!(
            !workspace.tab(tab).unwrap().password_input,
            "B inherited A's prompt"
        );
    }

    /// What a lagged owner's resync rides on: a snapshot reports every
    /// live tab, unchanged ones included, in one message — and becomes
    /// the record the next tick's changes are measured against.
    #[test]
    fn a_snapshot_reports_every_tab_in_one_message() {
        let (prompt_master, prompt_slave) = pty_pair();
        let (quiet_master, _quiet_slave) = pty_pair();
        let (prompt_master, quiet_master) = (Arc::new(prompt_master), Arc::new(quiet_master));
        set_line_discipline(&prompt_slave, true, false);
        let probes = [probe(3, 1, &prompt_master), probe(4, 2, &quiet_master)];
        let mut sampler = PasswordSampler::default();
        sampler.tick(&probes);
        assert_eq!(reports(sampler.tick(&probes)), []);

        match sampler.snapshot(&probes).as_slice() {
            [SupervisorEvent::PasswordSnapshot { entries }] => {
                assert_eq!(entries, &[(3, 1, true), (4, 2, false)]);
            }
            other => panic!("a snapshot is one PasswordSnapshot, got {other:?}"),
        }
        assert_eq!(reports(sampler.tick(&probes)), []);
    }

    /// The poller's dup of the master closes with the writer task's master
    /// handles, not with the session. A runtime torn down under a live
    /// child drops the writer task; the session stays in the map, held by
    /// a reap still waiting on that child. If the session held the dup, the
    /// master would stay open past every other handle, and a child that
    /// only ends on the hang-up the last master close sends would never
    /// end (#594's macOS hang).
    #[test]
    fn a_session_holds_no_master_open_once_its_writer_is_gone() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let supervisor = Arc::new(PtySupervisor::new());
        {
            let _inside = runtime.enter();
            let argv = ["/bin/sh", "-c", COOPERATIVE].map(String::from);
            let socket = std::path::Path::new("/tmp/roost-test.sock");
            supervisor
                .spawn(1, "/tmp", &argv, 80, 24, socket)
                .expect("spawn");
        }
        assert!(
            supervisor.password_watch.probes()[0].fd.upgrade().is_some(),
            "a live session's dup is reachable"
        );

        runtime.shutdown_background();
        let probes = supervisor.password_watch.probes();
        assert_eq!(probes.len(), 1, "the waiting reap keeps the session");
        let held_open = probes[0].fd.upgrade().is_some();
        drop(probes);
        supervisor.close(1);
        assert!(
            !held_open,
            "the session kept the master open past the writer's handles"
        );
    }

    /// A lag recovery converges whatever the tab count. More tabs than the
    /// lifecycle channel holds — a small one, so the PTYs stay few — are
    /// raised behind the poller's back. Each
    /// round, the republish the poller sends lands on the channel before
    /// the owner reads any of it — the worst case — and the owner applies
    /// what it can read and asks again whenever it lagged, as the applier
    /// does. Every row must end at its PTY's real state, out of a prompt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lag_recovery_converges_with_more_tabs_than_the_channel_holds() {
        use crate::application::{apply_password_report, spawn_in};
        use tokio::sync::broadcast::error::TryRecvError;

        let workspace = crate::Workspace::new();
        let project = workspace.create_project("p", "/tmp").unwrap().id;
        const CAPACITY: usize = 8;
        let supervisor = Arc::new(PtySupervisor::with_lifecycle_capacity(CAPACITY));
        let mut tabs = Vec::new();
        let mut ptys = Vec::new();
        for _ in 0..CAPACITY + 4 {
            let tab = workspace.open_tab(project, "/tmp", "", true).unwrap().id;
            ptys.push(spawn_in(
                &supervisor,
                tab,
                std::path::Path::new("/tmp"),
                COOPERATIVE,
            ));
            workspace.set_tab_password_input(tab, true);
            tabs.push(tab);
        }
        let mut sampler = PasswordSampler::default();
        let mut owner = supervisor.subscribe_lifecycle();

        for _ in 0..3 {
            for report in sampler.snapshot(&supervisor.password_watch.probes()) {
                let _ = supervisor.lifecycle.send(report);
            }
            let mut lagged = false;
            loop {
                match owner.try_recv() {
                    Ok(report) => apply_password_report(&workspace, &supervisor, &report),
                    Err(TryRecvError::Lagged(_)) => lagged = true,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Closed) => panic!("the lifecycle channel closed"),
                }
            }
            if !lagged {
                break;
            }
        }

        let raised: Vec<i64> = tabs
            .iter()
            .copied()
            .filter(|tab| workspace.tab(*tab).unwrap().password_input)
            .collect();
        assert!(
            raised.is_empty(),
            "a lag recovery left {} of {} rows raised: {raised:?}",
            raised.len(),
            tabs.len()
        );
    }

    /// `stty -echo` and a wait: canonical mode stays on in a
    /// non-interactive `sh` (there is no line editor to turn it off), so
    /// this is a password prompt until it reads a line.
    const PROMPT: &str = "stty -echo icanon; read line; stty echo; exec sleep 60";

    /// The incarnation of the next report that `tab_id` is or is not at a
    /// prompt.
    async fn reported(
        lifecycle: &mut broadcast::Receiver<SupervisorEvent>,
        tab_id: i64,
        password: bool,
    ) -> u64 {
        let wait = async {
            loop {
                match lifecycle.recv().await {
                    Ok(SupervisorEvent::PasswordInput {
                        tab_id: id,
                        incarnation,
                        password: reported,
                    }) if id == tab_id && reported == password => return incarnation,
                    Ok(_) => {}
                    Err(error) => panic!("lifecycle recv: {error:?}"),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .unwrap_or_else(|_| panic!("no password={password} report within 10 s"))
    }

    /// The poller is asleep with nothing to sample when the tab spawns —
    /// the test waits until it is — so the spawn has to wake it, and the
    /// report has to name the spawn it was sampled from.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_idle_poller_wakes_for_a_spawn_and_reports_its_prompt() {
        let supervisor = Arc::new(PtySupervisor::new());
        let mut lifecycle = supervisor.subscribe_lifecycle();
        let _poller = supervisor
            .start_password_poller()
            .expect("the first start starts it");
        assert!(
            supervisor.start_password_poller().is_none(),
            "a second start starts nothing"
        );
        let asleep = async {
            while supervisor.password_watch.idle_waits.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), asleep)
            .await
            .expect("the poller never went idle");
        let _pty =
            crate::application::spawn_in(&supervisor, 7, std::path::Path::new("/tmp"), PROMPT);
        let incarnation = supervisor.incarnation(7).expect("a live spawn");

        assert_eq!(reported(&mut lifecycle, 7, true).await, incarnation);
        supervisor
            .write(7, b"secret\n".to_vec())
            .await
            .expect("answer the prompt");
        assert_eq!(reported(&mut lifecycle, 7, false).await, incarnation);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_all_stops_the_poller() {
        let supervisor = Arc::new(PtySupervisor::new());
        let poller = supervisor.start_password_poller().expect("started");
        let _pty =
            crate::application::spawn_in(&supervisor, 7, std::path::Path::new("/tmp"), COOPERATIVE);
        supervisor.shutdown_all(Duration::from_secs(5)).await;
        tokio::time::timeout(Duration::from_secs(5), poller)
            .await
            .expect("the poller outlived shutdown_all")
            .expect("the poller panicked");
    }

    /// An idle poller holds its supervisor only weakly, and the drop is
    /// what wakes it to notice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_poller_ends_with_its_supervisor() {
        let supervisor = PtySupervisor::new();
        let poller = supervisor.start_password_poller().expect("started");
        drop(supervisor);
        tokio::time::timeout(Duration::from_secs(5), poller)
            .await
            .expect("the poller outlived its supervisor")
            .expect("the poller panicked");
    }

    /// The hang-up branch of `write_all_nonblocking` (#409), fenced
    /// without a child or a runtime shutdown to hide behind: a write
    /// waiting on a full slave input buffer ends when the slave hangs
    /// up. Remove the `is_write_closed()` check and this times out —
    /// after the hang-up Linux answers EAGAIN, and `try_io` then clears
    /// the readiness the one-time HUP edge came in on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_writer_waiting_on_a_full_buffer_ends_when_the_slave_hangs_up() {
        let (master, slave) = pty_pair();
        let master_fd = master.as_raw_fd();

        set_nonblocking(master_fd).expect("O_NONBLOCK");
        let writer =
            AsyncFd::with_interest(dup_master(master_fd).expect("dup"), Interest::WRITABLE)
                .expect("register");

        // Non-canonical, so the input buffer fills instead of the line
        // discipline discarding past one line.
        set_line_discipline(&slave, false, true);

        let payload = vec![b'x'; 1 << 20];
        let pending = tokio::spawn(async move { write_all_nonblocking(&writer, &payload).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !pending.is_finished(),
            "a mebibyte must not fit a slave input buffer nobody reads"
        );

        drop(slave);
        let result = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("the waiting write must end when the slave hangs up")
            .expect("writer task");
        assert!(
            result.is_err(),
            "a hung-up slave is an error, not a completed write: {result:?}"
        );
        drop(master);
    }

    #[test]
    fn concurrent_publishers_deliver_seqs_in_send_order() {
        // Hammers the assign+send critical section from many threads.
        // With the Mutex the receiver must see exactly 1..=N in order;
        // an implementation that reserved seqs with a bare fetch-add
        // and sent outside the lock could interleave reserve and send,
        // which this catches with high probability — and the correct
        // implementation can never fail it. Total sends stay within
        // the broadcast capacity so the undrained receiver cannot lag.
        let publisher = Arc::new(OutputPublisher::new());
        let mut rx = publisher.subscribe();
        let threads = 16;
        let sends_per_thread = PTY_OUTPUT_BROADCAST_CAPACITY / threads;
        let barrier = Arc::new(std::sync::Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let publisher = Arc::clone(&publisher);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..sends_per_thread {
                        publisher.send_bytes(vec![0]);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        for expected in 1..=(threads * sends_per_thread) as u64 {
            match rx.try_recv() {
                Ok(event) => assert_eq!(event.seq(), expected),
                Err(err) => panic!("receiver stopped at seq {expected}: {err:?}"),
            }
        }
    }

    #[test]
    fn empty_argv_becomes_default_shell() {
        // Default-shell case follows Ghostty's platform split: a login
        // shell (`-l`) on macOS so profile files load, a non-login
        // interactive shell on Linux so `~/.bashrc` loads (a stray
        // `.bash_profile` would otherwise shadow it). See `resolve_argv`.
        let empty: Vec<String> = Vec::new();
        let expected = if cfg!(target_os = "macos") {
            vec!["/bin/zsh".to_string(), "-l".to_string()]
        } else {
            vec!["/bin/zsh".to_string()]
        };
        assert_eq!(resolve_argv(&empty, "/bin/zsh"), expected);
    }

    #[test]
    fn explicit_argv_passes_through_unchanged() {
        // Launcher commands keep their argv — never force `-l`.
        let argv = vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            "echo hi".to_string(),
        ];
        assert_eq!(resolve_argv(&argv, "/bin/zsh"), argv);
    }

    fn sv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bash_autobootstrap_applies_to_simple_bash() {
        // Default-shell case (`[$SHELL, -l]`) and an explicit simple login
        // bash both auto-bootstrap.
        assert!(bash_autobootstrap(
            &sv(&["/opt/homebrew/bin/bash", "-l"]),
            true
        ));
        assert!(bash_autobootstrap(&sv(&["/usr/bin/bash", "-l"]), true));
        assert!(bash_autobootstrap(&sv(&["/usr/bin/bash"]), true));
        assert!(bash_autobootstrap(&sv(&["bash", "-i"]), true));
        assert!(bash_autobootstrap(&sv(&["bash", "-l", "-i"]), true));
    }

    #[test]
    fn bash_autobootstrap_skips_apple_32() {
        // /bin/bash on macOS is Apple's 3.2 (no ENV+POSIX) — skip it; on
        // Linux /bin/bash is modern, so it applies.
        assert!(!bash_autobootstrap(&sv(&["/bin/bash", "-l"]), true));
        assert!(bash_autobootstrap(&sv(&["/bin/bash", "-l"]), false));
    }

    #[test]
    fn bash_autobootstrap_skips_launcher_and_non_bash() {
        // Launcher / non-simple invocations pass through untouched.
        assert!(!bash_autobootstrap(
            &sv(&["/bin/bash", "-c", "echo hi"]),
            true
        ));
        assert!(!bash_autobootstrap(
            &sv(&["/usr/bin/bash", "--norc", "--noprofile"]),
            false
        ));
        assert!(!bash_autobootstrap(
            &sv(&["/usr/bin/bash", "--rcfile", "x"]),
            false
        ));
        assert!(!bash_autobootstrap(
            &sv(&["/usr/bin/bash", "--posix"]),
            false
        ));
        assert!(!bash_autobootstrap(&sv(&["/bin/zsh", "-l"]), true));
        assert!(!bash_autobootstrap(&[], true));
    }

    #[test]
    fn with_bash_posix_inserts_long_option_first() {
        // bash needs `--posix` before the short `-l` (a long option after a
        // short one errors), so it goes right after argv[0].
        assert_eq!(
            with_bash_posix(sv(&["/usr/bin/bash", "-l"]), true),
            sv(&["/usr/bin/bash", "--posix", "-l"])
        );
        assert_eq!(
            with_bash_posix(sv(&["/usr/bin/bash"]), true),
            sv(&["/usr/bin/bash", "--posix"])
        );
        // Not applied → untouched.
        assert_eq!(
            with_bash_posix(sv(&["/bin/bash", "-l"]), false),
            sv(&["/bin/bash", "-l"])
        );
    }

    #[test]
    fn bash_bootstrap_env_sets_env_and_inject() {
        let env = bash_bootstrap_env(std::path::Path::new("/res"), None, None, Some("/home/u"));
        assert!(env.contains(&(
            "ENV".to_string(),
            "/res/shell-integration/roost.bash".to_string()
        )));
        assert!(env.contains(&("ROOST_BASH_INJECT".to_string(), "1".to_string())));
        assert!(!env.iter().any(|(k, _)| k == "ROOST_BASH_ENV"));
    }

    #[test]
    fn bash_bootstrap_env_pins_histfile_when_unset() {
        let env = bash_bootstrap_env(std::path::Path::new("/res"), None, None, Some("/home/u"));
        assert!(env.contains(&("HISTFILE".to_string(), "/home/u/.bash_history".to_string())));
        assert!(env.contains(&("ROOST_BASH_UNEXPORT_HISTFILE".to_string(), "1".to_string())));
    }

    #[test]
    fn bash_bootstrap_env_keeps_existing_histfile_and_env() {
        // A user's HISTFILE wins (no pin, no un-export); a prior ENV is
        // preserved so the shim can restore it.
        let env = bash_bootstrap_env(
            std::path::Path::new("/res"),
            Some("/u/env.sh"),
            Some("/u/.myhist"),
            Some("/home/u"),
        );
        assert!(!env.iter().any(|(k, _)| k == "HISTFILE"));
        assert!(!env.iter().any(|(k, _)| k == "ROOST_BASH_UNEXPORT_HISTFILE"));
        assert!(env.contains(&("ROOST_BASH_ENV".to_string(), "/u/env.sh".to_string())));
    }

    #[test]
    fn bash_bootstrap_env_respects_empty_histfile() {
        // An empty HISTFILE disables history on purpose — don't re-enable
        // it by pinning ~/.bash_history (only a fully-unset HISTFILE pins).
        let env = bash_bootstrap_env(
            std::path::Path::new("/res"),
            None,
            Some(""),
            Some("/home/u"),
        );
        assert!(!env.iter().any(|(k, _)| k == "HISTFILE"));
        assert!(!env.iter().any(|(k, _)| k == "ROOST_BASH_UNEXPORT_HISTFILE"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cwd_of_pid_reads_current_process() {
        let got = cwd_of_pid(std::process::id()).expect("own cwd via platform process API");
        assert_eq!(
            std::path::Path::new(&got).canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_foreground_cwd_is_the_foreground_jobs_not_the_shells() {
        let (shell_dir, job_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let sup = Arc::new(PtySupervisor::new());
        let tab_id = 534;
        let Some(_hang_up) =
            spawn_foreground_job(&sup, tab_id, shell_dir.path(), job_dir.path()).await
        else {
            return;
        };

        assert_eq!(
            sup.native_cwds(tab_id),
            vec![canonical(job_dir.path()), canonical(shell_dir.path())],
            "the job's cwd, then the shell's, which never left its own"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_process_is_read_under_the_reap_latch() {
        let (shell_dir, job_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let sup = Arc::new(PtySupervisor::new());
        let tab_id = 535;
        let Some(_hang_up) =
            spawn_foreground_job(&sup, tab_id, shell_dir.path(), job_dir.path()).await
        else {
            return;
        };
        let latch = sup.sessions.lock().unwrap()[&tab_id].latch.clone();
        let reads = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        PROC_READ_HOOK.set(Some(Box::new({
            let reads = reads.clone();
            move |what| reads.borrow_mut().push((what, latch.0.try_lock().is_err()))
        })));

        let cwds = sup.native_cwds(tab_id);
        PROC_READ_HOOK.set(None);
        assert_eq!(
            cwds,
            vec![canonical(job_dir.path()), canonical(shell_dir.path())],
            "the precondition: both cwds were read"
        );
        let reads = reads.borrow();
        // Linux also opens the leader's `/proc` directory.
        let sequence: Vec<_> = reads
            .iter()
            .map(|&(what, _)| what)
            .filter(|&what| what != "open")
            .collect();
        assert_eq!(
            sequence,
            ["stat", "stat", "cwd", "cwd", "stat"],
            "the precondition: the child's stat, the leader's stamp, both cwds, the stamp again"
        );
        assert!(
            reads.iter().all(|&(_, latched)| !latched),
            "no read ran under the latch: {reads:?}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn same_leader_refuses_another_start_session_or_terminal() {
        let (child_sid, child_tty) = (500, 34816);
        let leader = ProcStamp {
            pid: 700,
            pgrp: 700,
            session: child_sid,
            tty: child_tty,
            tpgid: 700,
            start: (1_700_000_000, 250),
        };
        assert!(
            same_leader(&leader, &leader, child_sid, child_tty),
            "the precondition: a leader read twice unchanged is kept"
        );

        let restarted = ProcStamp {
            start: (1_700_000_000, 251),
            ..leader
        };
        assert!(
            !same_leader(&leader, &restarted, child_sid, child_tty),
            "a different start time is a reused pid"
        );
        for (what, stamp) in [
            (
                "a different session",
                ProcStamp {
                    session: child_sid + 1,
                    ..leader
                },
            ),
            (
                "a different terminal",
                ProcStamp {
                    tty: child_tty + 1,
                    ..leader
                },
            ),
            (
                "not its group's leader",
                ProcStamp {
                    pgrp: 701,
                    ..leader
                },
            ),
        ] {
            assert!(!same_leader(&stamp, &stamp, child_sid, child_tty), "{what}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_proc_stat_line_is_read_after_its_last_paren() {
        let stat = "4242 (a) b (c)) S 4000 4242 4000 34817 4242 4194560 1 2 3 4 5 6 7 8 20 0 1 0 \
                    987654 1000 200";
        assert_eq!(
            parse_proc_stat(stat),
            Some(ProcStamp {
                pid: 4242,
                pgrp: 4242,
                session: 4000,
                tty: 34817,
                tpgid: 4242,
                start: (987_654, 0),
            })
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_stamps_as_itself() {
        let stamp = proc_stamp(std::process::id()).expect("our own stamp");
        // SAFETY: plain queries of this process's own ids.
        let (pgrp, session) = unsafe { (libc::getpgrp(), libc::getsid(0)) };
        assert_eq!(
            (stamp.pid, stamp.pgrp, stamp.session),
            (
                i64::from(std::process::id()),
                i64::from(pgrp),
                i64::from(session)
            )
        );
    }

    /// Counts hangups instead of sending them. `clone_killer` hands out
    /// another handle onto the same counter, matching portable-pty's
    /// contract that a clone signals the same child.
    #[derive(Debug, Clone)]
    struct CountingKiller(Arc<AtomicUsize>);

    impl ChildKiller for CountingKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn a_close_after_the_reap_signals_nothing() {
        // `pid: None` on purpose: with a real pid the production
        // watchdog would SIGKILL a pid this test does not own once
        // `KILL_GRACE` elapsed.
        let kills = Arc::new(AtomicUsize::new(0));
        let killer = || {
            Mutex::new(Box::new(CountingKiller(kills.clone())) as Box<dyn ChildKiller + Send + Sync>)
        };

        let reaped = Arc::new(ReapLatch::new());
        reaped.reap(|| {});
        terminate_child(&killer(), None, reaped, 1);
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "a reaped child must not be signalled"
        );

        terminate_child(&killer(), None, Arc::new(ReapLatch::new()), 2);
        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "an unreaped child still gets its hangup"
        );
    }

    #[test]
    fn the_latch_refuses_to_signal_once_the_child_is_reaped() {
        let latch = ReapLatch::new();
        let mut before = false;
        assert!(latch.signal(|| before = true));
        assert!(before);

        latch.reap(|| {});
        let mut after = false;
        assert!(!latch.signal(|| after = true));
        assert!(!after, "a closed latch must not run its signal");

        // The fallback's poll closes signalling exactly when it reaps,
        // and not before.
        let polled = ReapLatch::new();
        assert!(matches!(polled.reap_if_exited(|| Ok(None::<()>)), Ok(None)));
        assert!(
            polled.signal(|| {}),
            "a poll that found the child alive leaves signalling open"
        );
        assert!(matches!(polled.reap_if_exited(|| Ok(Some(7))), Ok(Some(7))));
        assert!(
            !polled.signal(|| {}),
            "a poll that reaped the child closes signalling"
        );

        let lost = ReapLatch::new();
        let poll = || Err::<Option<()>, _>(std::io::Error::from_raw_os_error(libc::ECHILD));
        assert!(lost.reap_if_exited(poll).is_err());
        assert!(
            !lost.signal(|| {}),
            "a poll that lost the child closes signalling too"
        );
    }

    #[test]
    fn a_signal_in_flight_and_the_reap_serialise() {
        let latch = Arc::new(ReapLatch::new());
        // Flipped as the last act of the signal closure, still under the
        // latch: a reap that observes it false ran while the signal was
        // half-done.
        let finished = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(std::sync::Barrier::new(2));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let signaller = std::thread::spawn({
            let latch = latch.clone();
            let finished = finished.clone();
            let parked = parked.clone();
            move || {
                latch.signal(|| {
                    parked.wait();
                    release_rx.recv().expect("release");
                    finished.store(true, Ordering::SeqCst);
                });
            }
        });
        parked.wait();

        let (at_the_door_tx, at_the_door_rx) = std::sync::mpsc::channel::<()>();
        let (observed_tx, observed_rx) = std::sync::mpsc::channel::<bool>();
        let reaper = std::thread::spawn({
            let latch = latch.clone();
            let finished = finished.clone();
            move || {
                at_the_door_tx.send(()).expect("the test is listening");
                let _ = observed_tx.send(latch.reap(|| finished.load(Ordering::SeqCst)));
            }
        });
        at_the_door_rx.recv().expect("reaper thread");
        // Waiting, not synchronising: no channel can observe a thread
        // parked on a mutex, and from the send above the reaper is
        // microseconds from the latch. The wait only has to outlast
        // that for the reap to be contending when we release.
        std::thread::sleep(Duration::from_millis(50));

        release_tx.send(()).expect("release the parked signal");
        assert_eq!(
            observed_rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "a reap must not run while a signal is still in flight"
        );
        signaller.join().expect("signal thread");
        reaper.join().expect("reap thread");
    }

    #[test]
    fn waiting_without_reaping_leaves_the_status_for_wait() {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 7")
            .spawn()
            .expect("spawn /bin/sh");
        exited_without_reaping(child.id()).expect("waitid(WNOWAIT)");
        // The zombie is still ours: the pid was never released, so the
        // status is still there to collect.
        let status = child.wait().expect("wait after waitid(WNOWAIT)");
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn the_reap_fallback_never_blocks_a_signaller() {
        /// Kills whatever child is still in the cell — after a failed
        /// assertion, or a negative control that never reaps — so no
        /// `sleep` outlives the test. Ownership is the whole design:
        /// the poller takes the `Child` out of the cell the moment it
        /// reports an exit, so a cleanup here can only ever signal a pid
        /// nothing has reaped, with no disarm to get wrong. A `Mutex`
        /// around a `Child` is safe only because every use of it here is
        /// a `try_wait`; the production reaper waits, which is why
        /// [`ReapLatch`] keeps the child outside the lock.
        struct ChildGuard(Arc<Mutex<Option<std::process::Child>>>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let mut held = self.0.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(mut child) = held.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }

        let child = std::process::Command::new("/bin/sleep")
            .arg("100")
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = child.id();
        let child = Arc::new(Mutex::new(Some(child)));
        let _guard = ChildGuard(child.clone());

        let latch = Arc::new(ReapLatch::new());
        let (polling_tx, polling_rx) = std::sync::mpsc::channel::<()>();
        let (poll_tx, poll_rx) = std::sync::mpsc::channel();
        let poller = std::thread::spawn({
            let latch = latch.clone();
            let child = child.clone();
            move || {
                // Announced from inside the first poll, so the latch is
                // held when the test hears it: without that the signal
                // below could pass simply by arriving before the reaper.
                let mut announce = Some(polling_tx);
                let _ = poll_tx.send(reap_by_polling(&latch, || {
                    if let Some(tx) = announce.take() {
                        let _ = tx.send(());
                    }
                    let mut held = child.lock().unwrap_or_else(|p| p.into_inner());
                    let Some(alive) = held.as_mut() else {
                        return Ok(None);
                    };
                    let status = alive.try_wait()?;
                    if status.is_some() {
                        held.take();
                    }
                    Ok(status)
                }));
            }
        });
        polling_rx
            .recv()
            .expect("the poller reaches its first poll");

        // The SIGTERM is the only thing that will end this child, so a
        // fallback that held the latch across its wait would deadlock
        // the pair rather than merely delay the signal.
        let (signal_tx, signal_rx) = std::sync::mpsc::channel();
        let signaller = std::thread::spawn({
            let latch = latch.clone();
            move || {
                let _ = signal_tx.send(latch.signal(|| {
                    // SAFETY: a pid we spawned, held unreaped by the
                    // latch for the length of this closure.
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }));
            }
        });
        assert_eq!(
            signal_rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "a signaller must not wait on the reap fallback"
        );

        let status = poll_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the poller returns once the child exits")
            .expect("try_wait");
        assert!(
            !status.success(),
            "a SIGTERMed child is not a clean exit: {status:?}"
        );
        signaller.join().expect("signal thread");
        poller.join().expect("poll thread");
    }

    /// A shell that dies on SIGHUP as one process, leaving no
    /// descendant to hold the PTY open. The `COOPERATIVE` idiom of
    /// `tests/pty_shutdown_test.rs`, restated because that file is a
    /// separate crate target.
    const COOPERATIVE: &str = "exec sleep 100";

    /// Bounded wait for a tab's reap to reach the lifecycle channel.
    async fn exited(lifecycle: &mut broadcast::Receiver<SupervisorEvent>, tab_id: i64) -> bool {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match lifecycle.recv().await {
                    Ok(SupervisorEvent::TabExited { tab_id: id, .. }) if id == tab_id => return,
                    Ok(_) => {}
                    Err(err) => panic!("lifecycle recv: {err:?}"),
                }
            }
        })
        .await
        .is_ok()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_close_inside_the_promotion_window_cancels_the_spawn() {
        let sup = Arc::new(PtySupervisor::new());
        let mut lifecycle = sup.subscribe_lifecycle();
        let tab_id = 469;
        let socket = std::path::PathBuf::from("/tmp/roost-pty-promotion-window.sock");
        let argv: Vec<String> = vec!["/bin/sh".into(), "-c".into(), COOPERATIVE.into()];

        let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let spawning = tokio::task::spawn_blocking({
            let sup = sup.clone();
            let (socket, argv) = (socket.clone(), argv.clone());
            move || {
                sup.spawn_with(tab_id, "/tmp", &argv, 80, 24, &socket, || {
                    parked_tx.send(()).expect("the test is listening");
                    // Bounded, so a seam that ever moved inside the
                    // promotion locks fails this test on its named
                    // assertion instead of wedging the run: the park
                    // would release, the spawn would promote, and the
                    // `close()` it deadlocked would return.
                    release_rx
                        .recv_timeout(Duration::from_secs(10))
                        .expect("the test releases the park");
                })
            }
        });

        parked_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the spawn reaches the promotion window");
        sup.close(tab_id);
        release_tx.send(()).expect("the spawn is parked");
        let spawned = spawning.await.expect("the spawn task joins");

        // A promotion that installed the session anyway leaves a live
        // `sleep 100` behind, and tokio's shutdown waits out its reap
        // task — so tear it down here rather than stall the whole test
        // binary on the assertion below.
        if spawned.is_ok() {
            sup.close(tab_id);
            exited(&mut lifecycle, tab_id).await;
        }

        let Err(err) = spawned else {
            panic!("a close inside the promotion window cancels the spawn");
        };
        assert!(
            matches!(err.downcast_ref::<PtyError>(), Some(PtyError::Cancelled(id)) if *id == tab_id),
            "the cancellation names this tab: {err:?}"
        );
        assert!(!sup.has(tab_id), "the cancelled spawn installed no session");
        assert!(
            exited(&mut lifecycle, tab_id).await,
            "the unwanted child was terminated and reaped"
        );

        sup.spawn(tab_id, "/tmp", &argv, 80, 24, &socket)
            .expect("the freed slot leaked into neither pending nor sessions");
        sup.close(tab_id);
        assert!(
            exited(&mut lifecycle, tab_id).await,
            "the replacement child was reaped too"
        );
    }

    // #193: a user-defined `__roost_*` function must survive sourcing the
    // embedded shell-integration resource, and still be the one the hook
    // registration calls. These spawn real interactive bash/zsh so the
    // `case $- in *i*)` / `[[ -o interactive ]]` gates (and the PS0 install)
    // exercise the actual shipped bytes, not a stand-in.

    fn write_embedded(dir: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap_or_else(|e| {
            panic!("write embedded {name} for the shell-integration test: {e}")
        });
        path
    }

    struct ShellRun {
        stdout: String,
    }

    // Runs `body` as the `-c` script of an interactive, non-rc-loading
    // shell with `ROOST_TAB_ID` set and any env that would perturb the
    // sourcing path (the bash inject block, prior loaded-guards) cleared.
    // `None` means the binary isn't on PATH — the caller skips and says why.
    fn run_shell(
        bin: &str,
        interactive_flags: &[&str],
        script: &std::path::Path,
        body: &str,
    ) -> Option<ShellRun> {
        if !shell_present(bin) {
            eprintln!("skipping {bin} shell-integration test: {bin} not found on PATH");
            return None;
        }
        let mut cmd = std::process::Command::new(bin);
        cmd.args(interactive_flags).arg(body);
        cmd.env("ROOST_TAB_ID", "1");
        cmd.env("ROOST_SCRIPT", script);
        for var in [
            "ROOST_SHELL_FEATURES",
            "ROOST_BASH_INJECT",
            "ROOST_BASH_ENV",
            "_ROOST_BASH_LOADED",
            "_ROOST_ZSH_LOADED",
            "ROOST_ZSH_ZDOTDIR",
            // bash only honours BASH_ENV for non-interactive shells, but
            // strip both anyway: cheap, and keeps this hermetic against
            // whatever the ambient environment happens to set.
            "BASH_ENV",
            "ENV",
        ] {
            cmd.env_remove(var);
        }
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("spawn interactive {bin}: {e}"));
        // Interactive shells with no controlling tty print job-control
        // warnings on stderr; only stdout carries the assertions.
        Some(ShellRun {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }

    fn run_bash(script: &std::path::Path, body: &str) -> Option<ShellRun> {
        run_shell("bash", &["--norc", "--noprofile", "-i", "-c"], script, body)
    }

    fn run_zsh(script: &std::path::Path, body: &str) -> Option<ShellRun> {
        run_shell("zsh", &["-f", "-i", "-c"], script, body)
    }

    #[test]
    fn bash_user_function_survives_embedded_source() {
        let dir = tempfile::Builder::new()
            .prefix("roost-shell-it-")
            .tempdir_in("/tmp")
            .expect("tempdir under /tmp");
        let script = write_embedded(dir.path(), "roost.bash", ROOST_BASH);

        for name in ["__roost_osc7", "__roost_title", "__roost_marks"] {
            let body = format!(
                "{name}() {{ echo user-defined; }}\n\
                 before=$(declare -f {name})\n\
                 source \"$ROOST_SCRIPT\"\n\
                 after=$(declare -f {name})\n\
                 if [ \"$before\" = \"$after\" ]; then echo MATCH; else echo MISMATCH; fi\n\
                 echo \"HOOK=$PROMPT_COMMAND\"\n"
            );
            let Some(run) = run_bash(&script, &body) else {
                return;
            };
            assert!(
                run.stdout.lines().any(|l| l == "MATCH"),
                "{name}: the user's body did not survive sourcing roost.bash:\n{}",
                run.stdout
            );
            let hook = run
                .stdout
                .lines()
                .find(|l| l.starts_with("HOOK="))
                .unwrap_or_else(|| panic!("no HOOK= line for {name}:\n{}", run.stdout));
            assert!(
                hook.contains(name),
                "{name}: not named in PROMPT_COMMAND: {hook}"
            );
        }
    }

    #[test]
    fn bash_no_user_function_defines_roosts_own() {
        let dir = tempfile::Builder::new()
            .prefix("roost-shell-it-")
            .tempdir_in("/tmp")
            .expect("tempdir under /tmp");
        let script = write_embedded(dir.path(), "roost.bash", ROOST_BASH);
        let body = "source \"$ROOST_SCRIPT\"\n\
                    after=$(declare -f __roost_title)\n\
                    if [ -n \"$after\" ]; then echo DEFINED; else echo UNDEFINED; fi\n\
                    echo \"HOOK=$PROMPT_COMMAND\"\n";
        let Some(run) = run_bash(&script, body) else {
            return;
        };
        assert!(
            run.stdout.lines().any(|l| l == "DEFINED"),
            "Roost's own __roost_title was not defined:\n{}",
            run.stdout
        );
        let hook = run
            .stdout
            .lines()
            .find(|l| l.starts_with("HOOK="))
            .unwrap_or_else(|| panic!("no HOOK= line:\n{}", run.stdout));
        assert!(
            hook.contains("__roost_title"),
            "__roost_title not named in PROMPT_COMMAND: {hook}"
        );
    }

    #[test]
    fn bash_source_twice_changes_nothing() {
        let dir = tempfile::Builder::new()
            .prefix("roost-shell-it-")
            .tempdir_in("/tmp")
            .expect("tempdir under /tmp");
        let script = write_embedded(dir.path(), "roost.bash", ROOST_BASH);
        let body = "source \"$ROOST_SCRIPT\"\n\
                    after1=$(declare -f __roost_title)\n\
                    hook1=\"$PROMPT_COMMAND\"\n\
                    source \"$ROOST_SCRIPT\"\n\
                    after2=$(declare -f __roost_title)\n\
                    hook2=\"$PROMPT_COMMAND\"\n\
                    if [ \"$after1\" = \"$after2\" ] && [ \"$hook1\" = \"$hook2\" ]; then \
                      echo MATCH; else echo MISMATCH; fi\n";
        let Some(run) = run_bash(&script, body) else {
            return;
        };
        assert!(
            run.stdout.lines().any(|l| l == "MATCH"),
            "re-sourcing roost.bash changed the function or the hook:\n{}",
            run.stdout
        );
    }

    #[test]
    fn zsh_user_function_survives_embedded_source() {
        let dir = tempfile::Builder::new()
            .prefix("roost-shell-it-")
            .tempdir_in("/tmp")
            .expect("tempdir under /tmp");
        let script = write_embedded(dir.path(), "roost.zsh", ROOST_ZSH);

        for (name, hook_array) in [
            ("__roost_osc7", "precmd_functions"),
            ("__roost_title", "precmd_functions"),
            ("__roost_mark_c", "preexec_functions"),
            ("__roost_mark_d", "precmd_functions"),
        ] {
            let body = format!(
                "{name}() {{ echo user-defined }}\n\
                 before=$(functions {name})\n\
                 source \"$ROOST_SCRIPT\"\n\
                 after=$(functions {name})\n\
                 if [ \"$before\" = \"$after\" ]; then echo MATCH; else echo MISMATCH; fi\n\
                 echo \"HOOK=${{{hook_array}[*]}}\"\n"
            );
            let Some(run) = run_zsh(&script, &body) else {
                return;
            };
            assert!(
                run.stdout.lines().any(|l| l == "MATCH"),
                "{name}: the user's body did not survive sourcing roost.zsh:\n{}",
                run.stdout
            );
            let hook = run
                .stdout
                .lines()
                .find(|l| l.starts_with("HOOK="))
                .unwrap_or_else(|| panic!("no HOOK= line for {name}:\n{}", run.stdout));
            assert!(
                hook.contains(name),
                "{name}: not named in ${hook_array}: {hook}"
            );
        }
    }

    #[test]
    fn zsh_no_user_function_defines_roosts_own() {
        let dir = tempfile::Builder::new()
            .prefix("roost-shell-it-")
            .tempdir_in("/tmp")
            .expect("tempdir under /tmp");
        let script = write_embedded(dir.path(), "roost.zsh", ROOST_ZSH);
        // `add-zsh-hook` unconditionally autoload-marks the hook name it's
        // given (see `add-zsh-hook`'s trailing `autoload $autoopts -- $fn`),
        // so `functions __roost_mark_c` comes back non-empty — an autoload
        // placeholder body — even when roost.zsh never defined it. Invoking
        // the function and checking its actual output is what tells a real
        // definition apart from that placeholder (which errors instead).
        let body = "source \"$ROOST_SCRIPT\"\n\
                    output=$(__roost_mark_c 2>/dev/null)\n\
                    if [ -n \"$output\" ]; then echo DEFINED; else echo UNDEFINED; fi\n\
                    echo \"HOOK=${preexec_functions[*]}\"\n";
        let Some(run) = run_zsh(&script, body) else {
            return;
        };
        assert!(
            run.stdout.lines().any(|l| l == "DEFINED"),
            "Roost's own __roost_mark_c did not run:\n{}",
            run.stdout
        );
        let hook = run
            .stdout
            .lines()
            .find(|l| l.starts_with("HOOK="))
            .unwrap_or_else(|| panic!("no HOOK= line:\n{}", run.stdout));
        assert!(
            hook.contains("__roost_mark_c"),
            "__roost_mark_c not named in $preexec_functions: {hook}"
        );
    }

    #[test]
    fn zsh_source_twice_changes_nothing() {
        let dir = tempfile::Builder::new()
            .prefix("roost-shell-it-")
            .tempdir_in("/tmp")
            .expect("tempdir under /tmp");
        let script = write_embedded(dir.path(), "roost.zsh", ROOST_ZSH);
        let body = "source \"$ROOST_SCRIPT\"\n\
                    after1=$(functions __roost_mark_c)\n\
                    hook1=\"${preexec_functions[*]}\"\n\
                    source \"$ROOST_SCRIPT\"\n\
                    after2=$(functions __roost_mark_c)\n\
                    hook2=\"${preexec_functions[*]}\"\n\
                    if [ \"$after1\" = \"$after2\" ] && [ \"$hook1\" = \"$hook2\" ]; then \
                      echo MATCH; else echo MISMATCH; fi\n";
        let Some(run) = run_zsh(&script, body) else {
            return;
        };
        assert!(
            run.stdout.lines().any(|l| l == "MATCH"),
            "re-sourcing roost.zsh changed the function or the hook:\n{}",
            run.stdout
        );
    }
}
