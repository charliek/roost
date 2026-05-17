//! PTY supervision: spawn a shell, surface the master fd as async streams
//! of bytes that the gRPC `StreamPty` handler can plug into.
//!
//! `portable-pty` is the workhorse. Its blocking reader/writer pair are
//! moved onto dedicated `spawn_blocking` workers and bridged to async via
//! `mpsc` channels. PTY ownership belongs to the supervisor, not the gRPC
//! stream — a UI reattaching after a disconnect should still find its tab
//! alive (that wiring is Phase 5; today the supervisor and the stream are
//! one-to-one).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;

use anyhow::Context;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

const PTY_OUTPUT_CHANNEL_CAPACITY: usize = 64;
const PTY_INPUT_CHANNEL_CAPACITY: usize = 64;
const PTY_OUTPUT_CHUNK_SIZE: usize = 4096;

pub struct PtySupervisor {
    sessions: Mutex<HashMap<i64, Session>>,
}

struct Session {
    input_tx: mpsc::Sender<Vec<u8>>,
    resize_tx: mpsc::Sender<PtySize>,
}

/// What the supervisor returns to the gRPC StreamPty handler when a tab
/// is attached: a stream of output bytes plus channels for input/resize.
pub struct AttachHandle {
    pub output_rx: mpsc::Receiver<Vec<u8>>,
    pub exit_rx: mpsc::Receiver<i32>,
}

impl Default for PtySupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl PtySupervisor {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn a shell for `tab_id` rooted at `cwd`. `argv` is the argument
    /// vector — empty means "use the user's `$SHELL`" (or `/bin/sh` as a
    /// last resort). The supervisor never invokes a shell to parse a
    /// composite command string; clients that want shell-style word
    /// splitting must send `["sh", "-c", "..."]` themselves.
    pub fn spawn(
        &self,
        tab_id: i64,
        cwd: &str,
        argv: &[String],
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<AttachHandle> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let cmd = build_command(cwd, argv);
        let mut child = pair.slave.spawn_command(cmd).context("spawn shell")?;

        // Drop the slave end now that the shell has it.
        drop(pair.slave);

        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(PTY_OUTPUT_CHANNEL_CAPACITY);
        let (exit_tx, exit_rx) = mpsc::channel::<i32>(1);
        let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(PTY_INPUT_CHANNEL_CAPACITY);
        let (resize_tx, mut resize_rx) = mpsc::channel::<PtySize>(8);

        let master = pair.master;

        // Reader: blocking read off the master fd, push to output channel.
        let reader_handle = master
            .try_clone_reader()
            .context("master.try_clone_reader")?;
        tokio::task::spawn_blocking({
            let output_tx = output_tx.clone();
            move || pty_reader_loop(reader_handle, &output_tx, tab_id)
        });

        // Writer + resizer: async loop on input/resize, blocking I/O underneath.
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
                        // `resize` ultimately calls `ioctl(TIOCSWINSZ)`,
                        // which is fast but technically blocking. Stay
                        // consistent with the `write_all` branch above and
                        // mark it as a blocking section.
                        if let Err(err) = tokio::task::block_in_place(|| master.resize(size)) {
                            warn!(tab_id, ?err, "pty resize failed");
                        }
                    }
                    else => break,
                }
            }
            debug!(tab_id, "pty input loop ended");
        });

        // Wait for the child to exit and report the status.
        tokio::task::spawn_blocking(move || match child.wait() {
            Ok(status) => {
                let code = status.exit_code() as i32;
                let _ = exit_tx.blocking_send(code);
            }
            Err(err) => {
                error!(tab_id, ?err, "child.wait failed");
                let _ = exit_tx.blocking_send(-1);
            }
        });

        let session = Session {
            input_tx,
            resize_tx,
        };
        self.sessions.lock().unwrap().insert(tab_id, session);

        Ok(AttachHandle { output_rx, exit_rx })
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
        // Dropping the senders signals the shell to exit on the next read/write.
        // The blocking workers spawned at `spawn()` time clean themselves up.
        let _ = self.sessions.lock().unwrap().remove(&tab_id);
    }
}

fn build_command(cwd: &str, argv: &[String]) -> CommandBuilder {
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
    cmd
}

fn pty_reader_loop(
    mut reader: Box<dyn Read + Send>,
    output_tx: &mpsc::Sender<Vec<u8>>,
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
                if output_tx.blocking_send(buf[..n].to_vec()).is_err() {
                    debug!(tab_id, "output consumer dropped, stopping reader");
                    return;
                }
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

// `master.resize(size)` is `&self` in portable-pty. The supervisor calls it
// from the same task that owns `master` via the resize channel, so no
// cross-thread synchronisation is needed.
