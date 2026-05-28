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

/// A command on a tab's single writer channel. Input and resize share
/// one FIFO so they reach the PTY in submission order end-to-end —
/// the writer loop applies them in the exact order they were sent (#80).
enum WriterCmd {
    Input(Vec<u8>),
    Resize(PtySize),
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
    /// Unified input+resize command channel — one FIFO, so commands
    /// reach the PTY in submission order through the writer loop.
    cmd_tx: mpsc::Sender<WriterCmd>,
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

    /// Best-effort native read of the tab's shell cwd — the new-tab
    /// fallback for shells that don't emit OSC 7. Reads the direct
    /// child (the shell) process's current directory; a new tab spawns
    /// a LOCAL shell, so the local path is what it should inherit.
    /// `None` if the tab has no live PTY or the read fails.
    pub fn foreground_cwd(&self, tab_id: i64) -> Option<String> {
        let pid = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(&tab_id).and_then(|s| s.pid)?
        };
        cwd_of_pid(pid)
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

        // Acquire the master reader + writer BEFORE spawning the
        // child, so `spawn_command` becomes the last fallible step.
        // If these fail, the PTY tears down with no child to orphan.
        // Doing them *after* the spawn (as before) could return an
        // error while a live shell had no wait task installed — that
        // PTY would escape supervisor control entirely (#80).
        let reader_handle = pair
            .master
            .try_clone_reader()
            .context("master.try_clone_reader")?;
        let writer = pair.master.take_writer().context("master.take_writer")?;

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
        // One command channel for input + resize so they apply to the
        // PTY in submission order (#80).
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WriterCmd>(PTY_INPUT_CHANNEL_CAPACITY);

        let master = pair.master;

        // Reader: blocking read off the master fd, push to broadcast.
        tokio::task::spawn_blocking({
            let output_tx = output_tx.clone();
            move || pty_reader_loop(reader_handle, &output_tx, tab_id)
        });

        // Writer + resizer: a single ordered loop over the unified
        // command stream, so a resize never reorders relative to the
        // input bytes submitted around it (and keystrokes never
        // reorder relative to each other).
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    WriterCmd::Input(data) => {
                        if let Err(err) = tokio::task::block_in_place(|| writer.write_all(&data)) {
                            warn!(tab_id, ?err, "pty write failed");
                            break;
                        }
                    }
                    WriterCmd::Resize(size) => {
                        if let Err(err) = tokio::task::block_in_place(|| master.resize(size)) {
                            warn!(tab_id, ?err, "pty resize failed");
                        }
                    }
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
            // Only remove the session if THIS waiter still owns it.
            // `close()` frees the slot synchronously, so the same
            // tab_id can be re-spawned before a stale waiter fires;
            // matching the per-spawn `reaped` identity prevents
            // evicting a newer live session (#80).
            let mut sessions = sessions_for_reap.lock().unwrap();
            let owns = sessions
                .get(&tab_id)
                .map(|s| Arc::ptr_eq(&s.reaped, &reaped_for_wait))
                .unwrap_or(false);
            if owns {
                sessions.remove(&tab_id);
            }
        });

        let session = Session {
            cmd_tx,
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
                .map(|s| s.cmd_tx.clone())
                .ok_or(PtyError::NotFound(tab_id))?
        };
        tx.send(WriterCmd::Input(data))
            .await
            .map_err(|_| PtyError::Closed(tab_id))?;
        Ok(())
    }

    pub async fn resize(&self, tab_id: i64, cols: u16, rows: u16) -> Result<(), PtyError> {
        let tx = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(&tab_id)
                .map(|s| s.cmd_tx.clone())
                .ok_or(PtyError::NotFound(tab_id))?
        };
        tx.send(WriterCmd::Resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }))
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

/// Resolve the argv to exec. An empty argv (the plain "open a shell"
/// case) becomes the user's `$SHELL` (or `/bin/sh`) launched as a
/// LOGIN shell via `-l`, so it sources profile files (`.bash_profile`
/// / `.zprofile`): that silences macOS's bash deprecation banner and
/// puts login-only PATH entries (e.g. `claude`) in scope, matching
/// Terminal.app / Ghostty. A non-empty argv (launcher commands) is
/// passed through verbatim. (`portable-pty` 0.8 couples program and
/// argv[0], so we use the `-l` flag rather than the `-bash`
/// dash-prefix login convention.)
fn resolve_argv(argv: &[String], shell: &str) -> Vec<String> {
    if argv.is_empty() {
        vec![shell.to_string(), "-l".to_string()]
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
/// would otherwise default it to ~/.sh_history) only when unset, with
/// ROOST_BASH_UNEXPORT_HISTFILE telling the shim to un-export it afterward.
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
    if existing_histfile.unwrap_or("").is_empty() {
        if let Some(home) = home.filter(|h| !h.is_empty()) {
            out.push(("HISTFILE".into(), format!("{home}/.bash_history")));
            out.push(("ROOST_BASH_UNEXPORT_HISTFILE".into(), "1".into()));
        }
    }
    out
}

/// Current working directory of `pid`. Linux reads `/proc/<pid>/cwd`;
/// other platforms (the macOS GTK dev build) have no `/proc` and
/// return `None` — the shipping GTK target is Linux. Backs the new-tab
/// cwd fallback when no OSC 7 cwd is tracked.
#[cfg(target_os = "linux")]
fn cwd_of_pid(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
}

#[cfg(not(target_os = "linux"))]
fn cwd_of_pid(_pid: u32) -> Option<String> {
    None
}

/// Shell-integration scripts, embedded at build time. Kept byte-identical
/// to the Mac copy under mac/Sources/Roost/Resources/shell-integration/.
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
    // An empty argv (plain "open a shell") resolves to `$SHELL -l` — a
    // login shell, see `resolve_argv`.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let resolved = resolve_argv(argv, &shell);
    // Modern bash: add `--posix` so it honors ENV (its only
    // per-interactive-shell hook), which we point at roost.bash below. The
    // argv and env decisions must agree, so both gate on the same helper.
    let bash_boot = bash_autobootstrap(&resolved, cfg!(target_os = "macos"));
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
    // Roost contract (documented in docs/reference/paths.md and the
    // refactor plan's acceptance criteria): every shell Roost spawns
    // sees its tab id and the IPC socket path, so `roostctl` invoked
    // from inside the tab dials the correct UI and routes
    // notifications back to the originating tab without needing a
    // wider env discovery.
    cmd.env("ROOST_TAB_ID", tab_id.to_string());
    cmd.env("ROOST_SOCKET", socket_path.as_os_str());
    // Roost shell-integration contract (parity with the Mac UI). TERM
    // stays xterm-256color (above). ROOST_SHELL_FEATURES is overridable.
    cmd.env("TERM_PROGRAM", "Roost");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env("ROOST_SHELL_INTEGRATION", "1");
    if std::env::var_os("ROOST_SHELL_FEATURES").is_none() {
        cmd.env("ROOST_SHELL_FEATURES", "cwd,title,marks,prompt");
    }
    if let Some(dir) = roost_resources_dir() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_argv_becomes_login_shell() {
        // Default-shell case: `$SHELL -l` so profile files load.
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            resolve_argv(&empty, "/bin/zsh"),
            vec!["/bin/zsh".to_string(), "-l".to_string()]
        );
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

    #[cfg(target_os = "linux")]
    #[test]
    fn cwd_of_pid_reads_proc_self() {
        let got = cwd_of_pid(std::process::id()).expect("own cwd via /proc");
        assert_eq!(
            std::path::Path::new(&got).canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }
}
