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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;

use anyhow::Context;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, PtySize};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, warn};

const PTY_OUTPUT_BROADCAST_CAPACITY: usize = 256;
const PTY_INPUT_CHANNEL_CAPACITY: usize = 64;
const PTY_OUTPUT_CHUNK_SIZE: usize = 4096;

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
    sessions: Mutex<HashMap<i64, Session>>,
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
}

/// Dead-channel `Session` used to reserve a slot under the
/// `sessions` lock while the caller of `spawn()` builds the real
/// session. Any concurrent caller hitting the placeholder's
/// channels will see `Closed` / `Lagged` errors and surface a
/// `PtyError::Closed` to the workspace — that's the right
/// behavior: the slot is owned but not yet usable.
fn placeholder_session() -> Session {
    let (input_tx, _input_rx) = mpsc::channel::<Vec<u8>>(1);
    let (resize_tx, _resize_rx) = mpsc::channel::<PtySize>(1);
    let (output_tx, _drop_rx) = broadcast::channel::<PtyOutputEvent>(1);
    Session {
        input_tx,
        resize_tx,
        output_tx,
        killer: Mutex::new(Box::new(NoopKiller)),
    }
}

/// `ChildKiller` impl for the placeholder slot. `kill()` is a
/// no-op because there's no real child yet; if the placeholder
/// somehow gets `close()`d we just drop it.
#[derive(Debug)]
struct NoopKiller;
impl ChildKiller for NoopKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(NoopKiller)
    }
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
            sessions: Mutex::new(HashMap::new()),
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
        // Reserve the slot atomically under the lock so two
        // concurrent `spawn(tab_id, ...)` calls cannot both pass the
        // duplicate check, create two PTYs, and then have the second
        // insert orphan the first. CR-flagged race on PR #78.
        //
        // Strategy: insert a placeholder Session whose channels
        // immediately drop (so any racing `write`/`resize` sees
        // `Closed` and surfaces it to the caller) under the lock,
        // then build the real one outside the lock, and replace the
        // placeholder at the end. On spawn failure we explicitly
        // remove the placeholder so the slot doesn't leak.
        let placeholder_id = {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.contains_key(&tab_id) {
                return Err(PtyError::DuplicateTab(tab_id).into());
            }
            sessions.insert(tab_id, placeholder_session());
            tab_id
        };
        // Roll the placeholder back on early-return paths via this
        // guard. RAII gives us crash-safety without dotting `remove`
        // calls through every `?`.
        struct SlotGuard<'a> {
            sup: &'a PtySupervisor,
            tab_id: i64,
            armed: bool,
        }
        impl Drop for SlotGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    let _ = self.sup.sessions.lock().unwrap().remove(&self.tab_id);
                }
            }
        }
        let mut slot = SlotGuard {
            sup: self,
            tab_id: placeholder_id,
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
        tokio::task::spawn_blocking(move || match child.wait() {
            Ok(status) => {
                let code = status.exit_code() as i32;
                let _ = output_tx_exit.send(PtyOutputEvent::Exit(code));
                let _ = lifecycle_tx.send(SupervisorEvent::TabExited {
                    tab_id,
                    status: code,
                });
            }
            Err(err) => {
                error!(tab_id, ?err, "child.wait failed");
                let _ = output_tx_exit.send(PtyOutputEvent::Exit(-1));
                let _ = lifecycle_tx.send(SupervisorEvent::TabExited { tab_id, status: -1 });
            }
        });

        let session = Session {
            input_tx,
            resize_tx,
            output_tx,
            killer: Mutex::new(killer),
        };
        // Replace the placeholder atomically. From this point the
        // SlotGuard must NOT roll back — the real session owns the
        // slot now.
        self.sessions.lock().unwrap().insert(tab_id, session);
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
        let session = self.sessions.lock().unwrap().remove(&tab_id);
        if let Some(session) = session {
            if let Ok(mut killer) = session.killer.lock() {
                if let Err(err) = killer.kill() {
                    // `kill()` returns ESRCH when the child is
                    // already gone — treat as success, the wait
                    // task has already (or will) emit Exit.
                    let already_gone =
                        err.kind() == std::io::ErrorKind::NotFound || err.raw_os_error() == Some(3);
                    if !already_gone {
                        warn!(tab_id, ?err, "pty kill failed");
                    }
                }
            }
        }
    }

    pub fn has(&self, tab_id: i64) -> bool {
        self.sessions.lock().unwrap().contains_key(&tab_id)
    }
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
}
