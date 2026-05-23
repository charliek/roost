//! PTY supervision: spawn a shell, surface the master fd as async
//! streams of bytes, bridge writes/resizes back.
//!
//! Copied + adapted from `crates/roost-core/src/pty.rs` at M3 of
//! the daemon-removal refactor. Adaptations vs the daemon original:
//!
//! * Tab id type stays `i64` (matches the roost-ipc wire id range).
//! * `ROOST_TAB_ID` + `ROOST_SOCKET` env vars are now injected into
//!   the child process — the daemon never did this in the Rust path
//!   (the Go path did via `cmd/roost/spawn.go`). The acceptance
//!   criterion in the plan explicitly calls these out.
//! * Output goes to a per-tab broadcast channel rather than a
//!   single-consumer mpsc, so the UI's renderer and any future
//!   in-process subscriber can fan out. The legacy daemon's
//!   single-stream consumer is the `tokio::sync::broadcast`'s only
//!   subscriber for now, but the design pre-bakes the multi-sub
//!   path that M3+ doesn't need yet.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, PtySize};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, warn};

const PTY_OUTPUT_BROADCAST_CAPACITY: usize = 256;
const PTY_INPUT_CHANNEL_CAPACITY: usize = 64;
const PTY_OUTPUT_CHUNK_SIZE: usize = 4096;
/// Grace period after SIGHUP before `close()` escalates to SIGKILL.
/// Matches the Mac side's 20×10ms teardown window in
/// `PtySupervisor.swift`.
const KILL_GRACE: Duration = Duration::from_millis(200);

/// What a subscriber gets back from `PtySupervisor::subscribe`.
#[derive(Debug, Clone)]
pub enum PtyOutputEvent {
    /// PTY emitted `bytes`. Bytes are owned to make `broadcast`
    /// cheap (each subscriber Clones the `Arc<Vec<u8>>`-equivalent
    /// internal repr; here we use plain `Vec<u8>` since
    /// per-frame chunks are small and the broadcast clone is cheap
    /// enough at the workloads roost runs).
    Bytes(Vec<u8>),
    /// PTY child exited with this status.
    Exit(i32),
}

/// Supervisor-level lifecycle events, fan-out to higher-level
/// state (e.g. `Workspace` listens for `Exit` and closes the tab).
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    TabExited { tab_id: i64, status: i32 },
}

pub struct PtySupervisor {
    sessions: Arc<Mutex<HashMap<i64, Session>>>,
    /// Tab ids whose `spawn()` is in flight — the PTY has not yet
    /// been created but the slot is reserved so a concurrent
    /// `spawn(tab_id, ...)` rejects with `DuplicateTab` instead of
    /// racing the first one. Cleaned up on every `spawn()` exit
    /// path via `SlotGuard`.
    pending: Mutex<HashSet<i64>>,
    /// One broadcast channel for supervisor-level events. The
    /// `Workspace` subscribes once at startup.
    lifecycle: broadcast::Sender<SupervisorEvent>,
}

struct Session {
    input_tx: mpsc::Sender<Vec<u8>>,
    resize_tx: mpsc::Sender<PtySize>,
    output_tx: broadcast::Sender<PtyOutputEvent>,
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
    /// Set true by the wait task once `child.wait()` returns (the
    /// child is reaped). `close()`'s SIGKILL watchdog reads this to
    /// skip force-killing an already-dead child.
    reaped: Arc<AtomicBool>,
}

impl Default for PtySupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl PtySupervisor {
    pub fn new() -> Self {
        let (lifecycle, _rx) = broadcast::channel(64);
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            pending: Mutex::new(HashSet::new()),
            lifecycle,
        }
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
            .map(|s| s.output_tx.subscribe())
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
        // Shared with the wait task; flipped true once the child is
        // reaped so the SIGKILL watchdog can stand down.
        let reaped = Arc::new(AtomicBool::new(false));

        // Drop the slave end now that the shell has it.
        drop(pair.slave);

        let (output_tx, _drop_rx) =
            broadcast::channel::<PtyOutputEvent>(PTY_OUTPUT_BROADCAST_CAPACITY);
        // Subscribe BEFORE we spawn the reader task. Returning this
        // to the caller guarantees no Bytes/Exit event between
        // spawn and caller-subscribe can be lost.
        let early_rx = output_tx.subscribe();
        let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(PTY_INPUT_CHANNEL_CAPACITY);
        let (resize_tx, mut resize_rx) = mpsc::channel::<PtySize>(8);

        let master = pair.master;

        // Reader: blocking read off the master fd, push to broadcast.
        let reader_handle = master
            .try_clone_reader()
            .context("master.try_clone_reader")?;
        tokio::task::spawn_blocking({
            let output_tx = output_tx.clone();
            move || pty_reader_loop(reader_handle, &output_tx, tab_id)
        });

        // Writer + resizer.
        let writer = master.take_writer().context("master.take_writer")?;
        tokio::spawn(async move {
            let mut writer = writer;
            loop {
                tokio::select! {
                    Some(data) = input_rx.recv() => {
                        if let Err(err) = tokio::task::block_in_place(|| writer.write_all(&data)) {
                            warn!(tab_id, ?err, "pty write failed");
                            break;
                        }
                    }
                    Some(size) = resize_rx.recv() => {
                        if let Err(err) = tokio::task::block_in_place(|| master.resize(size)) {
                            warn!(tab_id, ?err, "pty resize failed");
                        }
                    }
                    else => break,
                }
            }
            debug!(tab_id, "pty input loop ended");
        });

        // Wait for the child to exit; forward the exit status onto
        // the output channel AND the lifecycle channel so both
        // per-tab consumers and the workspace converge.
        let output_tx_exit = output_tx.clone();
        let lifecycle_tx = self.lifecycle.clone();
        let sessions_for_reap = self.sessions.clone();
        let reaped_for_wait = reaped.clone();
        tokio::task::spawn_blocking(move || {
            let status = match child.wait() {
                Ok(status) => status.exit_code() as i32,
                Err(err) => {
                    error!(tab_id, ?err, "child.wait failed");
                    -1
                }
            };
            // Mark reaped first so a concurrent `close()` SIGKILL
            // watchdog stands down, then publish exit and drop the
            // dead session so later writes get `NotFound` instead of
            // silently succeeding against a closed PTY.
            reaped_for_wait.store(true, Ordering::SeqCst);
            let _ = output_tx_exit.send(PtyOutputEvent::Exit(status));
            let _ = lifecycle_tx.send(SupervisorEvent::TabExited { tab_id, status });
            // Tab ids are allocated monotonically (`Workspace::alloc_id`),
            // so this can never evict a newer same-id session.
            sessions_for_reap.lock().unwrap().remove(&tab_id);
        });

        let session = Session {
            input_tx,
            resize_tx,
            output_tx,
            killer: Mutex::new(killer),
            pid,
            reaped,
        };
        // Promote the slot from pending → sessions atomically.
        // If `close(tab_id)` ran while we were building the PTY it
        // removed our entry from `pending` as a cancellation
        // signal. Detect that here, kill the freshly-spawned
        // child, and don't insert into `sessions`. The killer was
        // moved into the wait task already, so we reach for the
        // copy we stashed in `session` below — actually the
        // session struct already holds the killer, so we tear it
        // back down via `terminate_child` (SIGHUP→SIGKILL) and drop
        // `session` (which drops the input/resize channels, the
        // writer task exits, and the wait task reaps once the
        // signal lands).
        {
            let mut sessions = self.sessions.lock().unwrap();
            let mut pending = self.pending.lock().unwrap();
            if !pending.remove(&tab_id) {
                // Cancelled by close(). Kill the child rather than
                // returning a usable receiver.
                drop(pending);
                drop(sessions);
                terminate_child(&session.killer, session.pid, session.reaped.clone(), tab_id);
                drop(session);
                // SlotGuard is no longer needed — pending was
                // already cleared by close(); we already cleaned
                // up the child.
                slot.armed = false;
                return Err(PtyError::Cancelled(tab_id).into());
            }
            sessions.insert(tab_id, session);
        }
        slot.armed = false;

        Ok(early_rx)
    }

    pub async fn write(&self, tab_id: i64, data: Vec<u8>) -> Result<(), PtyError> {
        let tx = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(&tab_id)
                .map(|s| s.input_tx.clone())
                .ok_or(PtyError::NotFound(tab_id))?
        };
        tx.send(data).await.map_err(|_| PtyError::Closed(tab_id))?;
        Ok(())
    }

    pub async fn resize(&self, tab_id: i64, cols: u16, rows: u16) -> Result<(), PtyError> {
        let tx = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(&tab_id)
                .map(|s| s.resize_tx.clone())
                .ok_or(PtyError::NotFound(tab_id))?
        };
        tx.send(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
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
        let (session, was_pending) = {
            let mut sessions = self.sessions.lock().unwrap();
            let mut pending = self.pending.lock().unwrap();
            (sessions.remove(&tab_id), pending.remove(&tab_id))
        };
        if let Some(session) = session {
            terminate_child(&session.killer, session.pid, session.reaped.clone(), tab_id);
        } else if was_pending {
            debug!(tab_id, "close() cancelled in-flight spawn");
        }
    }

    pub fn has(&self, tab_id: i64) -> bool {
        self.sessions.lock().unwrap().contains_key(&tab_id)
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
    reaped: Arc<AtomicBool>,
    tab_id: i64,
) {
    if let Ok(mut killer) = killer.lock() {
        if let Err(err) = killer.kill() {
            // ESRCH (raw 3) / NotFound: child already gone — the wait
            // task has or will emit Exit. Anything else is a real
            // failure worth logging.
            let already_gone =
                err.kind() == std::io::ErrorKind::NotFound || err.raw_os_error() == Some(3);
            if !already_gone {
                warn!(tab_id, ?err, "pty SIGHUP failed");
            }
        }
    }
    let Some(pid) = pid else { return };
    // Detached watchdog: if the wait task hasn't reaped the child
    // within the grace window it ignored SIGHUP — force-kill. A plain
    // `std::thread` (not tokio) keeps `close()` callable from any
    // context regardless of runtime. SIGKILL against an
    // exited-but-unreaped zombie is harmless; the wait task reaps it.
    // PID reuse inside the short window is negligible and gated by
    // `reaped`.
    std::thread::spawn(move || {
        std::thread::sleep(KILL_GRACE);
        if !reaped.load(Ordering::SeqCst) {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    });
}

fn build_command(
    cwd: &str,
    argv: &[String],
    tab_id: i64,
    socket_path: &std::path::Path,
) -> CommandBuilder {
    // Argv-first: never call a shell to parse a single command string. If
    // the caller sent an empty argv, fall back to the user's $SHELL (or
    // /bin/sh) with no arguments.
    let mut cmd = if let Some((program, args)) = argv.split_first() {
        let mut c = CommandBuilder::new(program);
        for a in args {
            c.arg(a);
        }
        c
    } else {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        CommandBuilder::new(shell)
    };
    if !cwd.is_empty() {
        cmd.cwd(cwd);
    }
    if let Some(term) = std::env::var_os("TERM") {
        cmd.env("TERM", term);
    } else {
        cmd.env("TERM", "xterm-256color");
    }
    cmd.env("COLORTERM", "truecolor");
    // Roost contract (documented in docs/reference/paths.md and the
    // refactor plan's acceptance criteria): every shell Roost spawns
    // sees its tab id and the IPC socket path, so `roostctl` invoked
    // from inside the tab dials the correct UI and routes
    // notifications back to the originating tab without needing a
    // wider env discovery.
    cmd.env("ROOST_TAB_ID", tab_id.to_string());
    cmd.env("ROOST_SOCKET", socket_path.as_os_str());
    cmd
}

fn pty_reader_loop(
    mut reader: Box<dyn Read + Send>,
    output_tx: &broadcast::Sender<PtyOutputEvent>,
    tab_id: i64,
) {
    let mut buf = vec![0u8; PTY_OUTPUT_CHUNK_SIZE];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                debug!(tab_id, "pty reached EOF");
                return;
            }
            Ok(n) => {
                let _ = output_tx.send(PtyOutputEvent::Bytes(buf[..n].to_vec()));
            }
            Err(err) => {
                if matches!(err.kind(), std::io::ErrorKind::Interrupted) {
                    continue;
                }
                debug!(tab_id, ?err, "pty read error, stopping reader");
                return;
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("pty for tab {0} not found")]
    NotFound(i64),
    #[error("pty for tab {0} is closed")]
    Closed(i64),
    #[error("tab {0} already has a live pty session")]
    DuplicateTab(i64),
    #[error("spawn for tab {0} cancelled by close()")]
    Cancelled(i64),
}
