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
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
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

    /// Spawn a shell for `tab_id`. See `crates/roost-core/src/pty.rs`
    /// for the legacy daemon docstring; semantics here are unchanged
    /// except for the env injection and the broadcast output channel.
    ///
    /// `socket_path` is the absolute path to the IPC socket, injected
    /// into the child as `ROOST_SOCKET` so `roostctl` invoked from
    /// inside the tab dials the right UI.
    pub fn spawn(
        &self,
        tab_id: i64,
        cwd: &str,
        argv: &[String],
        cols: u16,
        rows: u16,
        socket_path: &std::path::Path,
    ) -> anyhow::Result<()> {
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

        // Drop the slave end now that the shell has it.
        drop(pair.slave);

        let (output_tx, _output_rx) =
            broadcast::channel::<PtyOutputEvent>(PTY_OUTPUT_BROADCAST_CAPACITY);
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
        };
        self.sessions.lock().unwrap().insert(tab_id, session);

        Ok(())
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
        // Dropping the senders signals the shell to exit on the next
        // read/write. The blocking workers spawned at `spawn()` time
        // clean themselves up.
        let _ = self.sessions.lock().unwrap().remove(&tab_id);
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
}
