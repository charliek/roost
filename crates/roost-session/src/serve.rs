//! The serving core: bind, hydrate, answer, stop.
//!
//! Split out from `start` on purpose. Everything here runs in-process
//! against paths the caller chose and locks the caller already holds, so
//! a test can drive a whole session — bind, ops, `session.stop`, socket
//! unlink, state flush — on a tempdir without forking, redirecting
//! stdio, or installing a process-global subscriber. `start` is the thin
//! shell that supplies the real profile's paths and the real process
//! posture.
//!
//! # How a session stops
//!
//! One path, three entrances. `session.stop` over the wire is the
//! definition: latch, drain in-flight mutations, flush `state.json`,
//! reap every child, reply with the report — all of it inside
//! `roost-engine`'s dispatcher — and only then hand back the
//! [`StopHandle`] installed here to run the process-level tail.
//!
//! SIGTERM and SIGINT do not reimplement any of that. They **dial this
//! session's own socket and send `session.stop`**, so the signal path
//! and the wire path are the same code down to the last line, and a
//! signal racing a client's stop simply loses the latch and is told
//! `shutting-down`. The direct finalize is reserved for the one case the
//! wire cannot cover: the socket itself is unreachable, and a SIGTERM
//! must still bring the process down.
//!
//! The tail — fire the accept loop's cancel token, flush the workspace,
//! unlink the socket if it is still ours, release the instance locks —
//! runs at most once, however many entrances raced.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use roost_engine::ipc::{IpcHandler, SessionInfo, StopHandle};
use roost_engine::single_instance::InstanceLocks;
use roost_engine::workspace::{REPLAY_BUDGET_BYTES, REPLAY_WINDOW};
use roost_engine::{
    LocalClient, PtySupervisor, ReplayBounds, ServerVtConfig, ServerVtWorkspace, Workspace,
};
use roost_ipc::messages::{ops, AttachPayloadKind, SessionStopParams};
use roost_ipc::paths::BundleProfile;
use roost_ipc::{IpcClient, IpcServer};
use tokio::sync::{oneshot, watch};
use tracing::{debug, error, info, warn};

use crate::consts::{
    DEFAULT_TAB_COLS, DEFAULT_TAB_ROWS, FINALIZE_JOIN_TIMEOUT, SIGNAL_STOP_TIMEOUT,
};
use crate::readiness::{Readiness, Verdict};
use crate::socket_guard::{unlink_if_ours, SocketIdentity, Unlinked};
use crate::{hydrate, identity};

/// Everything a session needs that is not a lock and not a log.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub socket_path: PathBuf,
    /// Absolute path to this session's `state.json`.
    pub state_path: PathBuf,
    pub app_label: String,
    pub app_id: String,
    /// Directory the user launched from, captured before the daemon
    /// `chdir`'d to `/`. Seeds the first project on an empty state file.
    pub launch_cwd: PathBuf,
    /// Where `session.put_file` lands what a client uploads, swept at
    /// every start and every clean stop.
    ///
    /// `None` when no cache path could be resolved at all — the session
    /// still serves, and the op answers `not-supported`. A field rather
    /// than a profile lookup inside `serve` so the integration tests can
    /// point a session at a tempdir instead of the developer's cache.
    pub files_dir: Option<PathBuf>,
    /// Where the store goes instead when `files_dir` cannot be pasted
    /// bare — [`BundleProfile::files_dir_fallback`] in production, a
    /// tempdir in tests.
    pub files_fallback: PathBuf,
    /// Whether this session serves the test-mode op set
    /// (`tab.feed_pty_bytes`, `tab.capture_pty_input`) and keeps the
    /// input-capture buffer.
    ///
    /// **This is the in-process test-harness knob, and nothing else.**
    /// The shipped binary never sets it directly: `start` builds its
    /// config through [`SessionConfig::from_profile`], which derives
    /// this from `ROOST_TEST_MODE` via `identity::test_mode_env`, so a
    /// daemon's behaviour is still decided by that one environment
    /// variable — and by the same reader `identify` uses.
    ///
    /// A field rather than a read inside `serve` because the in-process
    /// integration tests run several sessions in one process, and a
    /// process-global env var is neither settable safely from a
    /// `#[tokio::test]` nor scopable to one of them.
    pub test_mode: bool,
    /// What this session reports as its `libghostty_build`, when a test
    /// wants it to be something other than the truth.
    ///
    /// `None` in every shipped run: [`SessionConfig::from_profile`]
    /// fills it via [`identity::test_mode_env`], which reads
    /// [`crate::consts::FAKE_BUILD_ENV`] only while `test_mode` is on,
    /// so a production daemon cannot be talked into lying about the pin
    /// it can actually decode. Carried as a field
    /// rather than read where it is used, so the environment is
    /// consulted once, at the edge, and everything downstream reads the
    /// config — the session e2e lane drives it by spawning a daemon
    /// with the env var set.
    pub fake_libghostty_build: Option<String>,
    /// How many committed batches this session retains for
    /// `events.subscribe {from_revision}`, when a test wants a window it
    /// can drive past.
    ///
    /// `None` in every shipped run — [`SessionConfig::from_profile`]
    /// fills it from [`crate::consts::REPLAY_WINDOW_ENV`] only while
    /// `test_mode` is on, the same way `fake_libghostty_build` works, so
    /// the environment is read once at the edge and a production daemon
    /// keeps `REPLAY_WINDOW`.
    pub replay_window: Option<usize>,
}

impl SessionConfig {
    /// The shipped configuration for a profile.
    pub fn from_profile(profile: &BundleProfile, launch_cwd: PathBuf) -> Self {
        let (test_mode, fake_libghostty_build) = identity::test_mode_env();
        let replay_window = parse_replay_window(
            test_mode,
            std::env::var(crate::consts::REPLAY_WINDOW_ENV)
                .ok()
                .as_deref(),
        );
        let files_dir = match profile.files_dir() {
            Ok(dir) => Some(dir),
            // Handled, not propagated: the only way this fails is a
            // process with no home to hang a cache off, and that is a
            // session without `session.put_file`, not a session that
            // refuses to start.
            Err(error) => {
                warn!(%error, "no file-store path resolved; session.put_file will answer not-supported");
                None
            }
        };
        Self {
            socket_path: profile.socket_path.clone(),
            state_path: profile.state_json_path(),
            app_label: profile.app_label.to_string(),
            app_id: profile.app_id.to_string(),
            launch_cwd,
            files_dir,
            files_fallback: profile.files_dir_fallback(),
            test_mode,
            fake_libghostty_build,
            replay_window,
        }
    }
}

/// The replay-window override, or `None` for the shipped default.
///
/// Pure so the gate and the fallbacks are testable without a
/// process-global environment: production ignores the variable outright,
/// and in test mode a value that is not a positive count is a mistake
/// worth naming rather than a reason to serve a zero-length ring.
fn parse_replay_window(test_mode: bool, raw: Option<&str>) -> Option<usize> {
    let raw = raw.filter(|_| test_mode)?;
    match raw.parse::<usize>() {
        Ok(window) if window > 0 => Some(window),
        _ => {
            warn!(
                value = %raw,
                "ignoring an unusable {}; the default replay window applies",
                crate::consts::REPLAY_WINDOW_ENV
            );
            None
        }
    }
}

/// Run one session to completion.
///
/// Returns when the accept loop has unwound and the stop tail has
/// finished, so a caller may exit immediately afterwards without racing
/// the socket unlink. `locks` are held for the whole run and released by
/// that tail.
pub async fn serve(
    config: SessionConfig,
    locks: InstanceLocks,
    readiness: &mut Readiness,
) -> Result<()> {
    // The one workspace that serves `events.subscribe`, so the one that
    // pays for a replay ring.
    let workspace = Arc::new(Workspace::open(config.state_path.clone()).with_replay(
        ReplayBounds {
            window: config.replay_window.unwrap_or(REPLAY_WINDOW),
            budget_bytes: REPLAY_BUDGET_BYTES,
        },
    ));
    let supervisor = Arc::new(PtySupervisor::new());

    // Before the first spawn, which is what makes it total: every tab
    // this session ever opens — hydrated or asked for over the wire —
    // gets a server terminal, so there is no second class of tab that
    // cannot be attached, dumped, or answer a device query. The
    // workspace is handed in as the seam the tab tasks apply OSC
    // transitions and row-closes through (the supervisor itself stays
    // workspace-agnostic).
    let test_mode = config.test_mode;
    supervisor
        .enable_server_vt(
            ServerVtConfig::new(Arc::clone(&workspace) as Arc<dyn ServerVtWorkspace>)
                // The capture buffer only grows, so it is test-mode
                // only — the same gate `tab.capture_pty_input` itself
                // is behind.
                .with_input_capture(test_mode),
        )
        .context("enable the server-VT pipeline")?;

    let client = LocalClient::new(
        Arc::clone(&workspace),
        Arc::clone(&supervisor),
        config.socket_path.clone(),
    );

    hydrate::hydrate(&client, &config.launch_cwd)
        .await
        .context("hydrate the saved layout")?;

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (done_tx, done_rx) = oneshot::channel();
    let stop = Arc::new(StopState {
        finalized: AtomicBool::new(false),
        cancel: cancel_tx,
        workspace: Arc::clone(&workspace),
        socket_path: config.socket_path.clone(),
        socket_identity: OnceLock::new(),
        locks: Mutex::new(Some(locks)),
        done: Mutex::new(Some(done_tx)),
    });

    let session = SessionInfo {
        session_id: identity::session_id(),
        started_at: identity::rfc3339_utc(std::time::SystemTime::now()),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        // Both answered for real now that every tab has a server
        // terminal behind it. The build string is the negotiation: a
        // client whose libghostty pin differs cannot decode this
        // session's snapshots, and `tab.attach` refuses it by name
        // rather than letting the mismatch surface as a corrupt screen.
        payload_kinds: vec![AttachPayloadKind::GHOSTTY_SNAPSHOT.into()],
        // One string, both uses: what `session.identify` reports and
        // what `tab.attach` compares against. A test-mode override
        // therefore makes a build mismatch reproducible end to end
        // without a second binary (plan 037 §3.7) — and cannot make the
        // two disagree, which would be a failure mode no client could
        // make sense of.
        //
        // `identity::build_identity` is the one place this resolution
        // happens — `roost-session identify` (plan 039 §3.1) answers the
        // same question for a binary that has never run, and a second
        // copy of this match here is exactly how the two would drift.
        libghostty_build: identity::build_identity(
            config.fake_libghostty_build.as_deref(),
            config.test_mode,
        )
        .libghostty_build,
        default_tab_size: (DEFAULT_TAB_COLS, DEFAULT_TAB_ROWS),
        test_mode,
    };
    info!(
        session_id = %session.session_id,
        started_at = %session.started_at,
        socket = %config.socket_path.display(),
        "host session starting"
    );

    // Opened before the handler is built, because the root it settled
    // on — which is not always the one that was configured — is also
    // what the clean stop sweeps.
    //
    // On the blocking pool like the writes and the stop sweep: the start
    // sweep walks and removes whatever a crash left, and `$HOME` can be
    // a network mount.
    let file_store = match config.files_dir.clone() {
        Some(dir) => {
            let fallback = config.files_fallback.clone();
            let opened =
                tokio::task::spawn_blocking(move || crate::files::open_store(&dir, &fallback))
                    .await
                    .unwrap_or_else(|join| {
                        Err(anyhow::anyhow!("the file-store open task failed: {join}"))
                    });
            match opened {
                Ok(store) => Some(store),
                // A session that cannot land files still serves
                // everything else; the op answers `not-supported`
                // (plan 047 §3.1).
                Err(error) => {
                    error!(%error, "no file store; session.put_file will answer not-supported");
                    None
                }
            }
        }
        None => None,
    };
    let files_root = file_store.as_ref().map(|store| store.root().to_path_buf());

    // Never `.with_ui`: there is no main thread to hop to, and the
    // UI-only ops answer `internal: no UI attached` for exactly that
    // reason.
    let mut handler = IpcHandler::new(
        Arc::clone(&workspace),
        supervisor,
        config.socket_path.clone(),
        config.app_label.clone(),
        config.app_id.clone(),
    )
    .with_session(session, stop_handle(&stop, files_root))
    // The install engine lives on this side of the seam and only this
    // side: `roost-engine` decodes and lease-gates the op, the daemon
    // owns the `$HOME` it writes (plan 046 §3.4).
    .with_agent_hooks(crate::agent_hooks::handle());
    if let Some(store) = file_store {
        handler = handler.with_file_store(store);
    }

    let server = IpcServer::bind(&config.socket_path, handler)
        .await
        .with_context(|| format!("bind {}", config.socket_path.display()))?
        // A forwarded socket is opened as the forwarding user, so the
        // filesystem mode alone stops nobody once this session is
        // reachable over SSH. The kernel's answer is the one that counts.
        .require_same_uid();

    // Recorded from the bound socket, not from the path we asked for:
    // the tail unlinks by identity so a successor that rebinds the same
    // name survives this session's shutdown.
    let bound = SocketIdentity::of(&config.socket_path).context("stat the bound session socket")?;
    let _ = stop.socket_identity.set(bound);

    spawn_signal_stops(&stop);

    readiness.report(&Verdict::Ready(std::process::id()));

    let served = server
        .run_until(async move {
            let mut cancel = cancel_rx;
            // An error means the sender is gone, which can only happen
            // once the whole session is finished — treat it as shutdown.
            let _ = cancel.wait_for(|cancelled| *cancelled).await;
        })
        .await;

    if stop.finalized.load(Ordering::Acquire) {
        // The tail runs on a detached task (that is what lets it survive
        // the connection cancellation it causes), so the accept loop can
        // unwind while the socket is still on disk. Wait for it.
        if tokio::time::timeout(FINALIZE_JOIN_TIMEOUT, done_rx)
            .await
            .is_err()
        {
            warn!("the session stop tail did not finish within its budget");
        }
    }
    served.context("serve the session socket")?;
    info!("host session stopped");
    Ok(())
}

/// The process-level tail of a stop: everything that happens after the
/// engine has flushed state, reaped every child, and replied.
struct StopState {
    /// Latched by whichever entrance gets here first. The engine has its
    /// own latch for the *session*; this one guards the process tail,
    /// which the direct-finalize fallback can reach without going
    /// through the engine at all.
    finalized: AtomicBool,
    cancel: watch::Sender<bool>,
    /// Flushed by the tail. On the wire path the engine has already
    /// flushed and frozen this, so the call is a no-op; on the
    /// direct-finalize fallback it is the only flush there will be.
    workspace: Arc<Workspace>,
    socket_path: PathBuf,
    /// Set once, immediately after `bind`. Absent only if bind never
    /// completed, in which case there is no socket of ours to remove.
    socket_identity: OnceLock<SocketIdentity>,
    /// Taken and dropped by the tail. Releasing the flocks is what lets
    /// a replacement session start the moment this one is done.
    locks: Mutex<Option<InstanceLocks>>,
    done: Mutex<Option<oneshot::Sender<()>>>,
}

impl StopState {
    /// Fire the tail. Idempotent: later callers observe the latch and
    /// return, so a signal racing a wire stop costs nothing.
    fn finalize(&self) {
        if self.finalized.swap(true, Ordering::AcqRel) {
            return;
        }
        // First, so nothing new is accepted and no live connection is
        // left parked waiting on a session that has already reaped its
        // children. The reply this tail was handed back by is already on
        // the wire — that ordering is `ConnAction::FinalizeStop`'s
        // contract.
        let _ = self.cancel.send(true);

        // Idempotent and freezing, so on the wire path — where the
        // engine flushed before it replied — this changes nothing. It is
        // here for the other entrance: a SIGTERM whose self-dial could
        // not reach the socket has run none of the engine's stop, and
        // without this the session's layout would be whatever the last
        // write-through happened to leave.
        //
        // Deliberately no `shutdown_all` alongside it. That path is only
        // reached after the signal's own 30s budget has already expired,
        // so there is no time left to spend hanging children up politely;
        // they die with the process, which is the same posture a crash
        // leaves behind.
        self.workspace.flush();

        match self.socket_identity.get() {
            Some(identity) => match unlink_if_ours(&self.socket_path, *identity) {
                Ok(Unlinked::Removed) => {
                    debug!(socket = %self.socket_path.display(), "unlinked the session socket");
                }
                Ok(Unlinked::Absent) => {
                    debug!(socket = %self.socket_path.display(), "session socket was already gone");
                }
                Ok(Unlinked::Foreign) => {
                    warn!(
                        socket = %self.socket_path.display(),
                        "another socket now holds this path; leaving it alone"
                    );
                }
                Err(error) => {
                    error!(socket = %self.socket_path.display(), %error, "failed to unlink the session socket");
                }
            },
            None => debug!("no socket was bound; nothing to unlink"),
        }

        // After the unlink: the lock is what serializes the whole
        // probe→unlink→bind sequence, so a successor must not be able to
        // take it until this session's socket is off disk.
        drop(take(&self.locks));

        if let Some(done) = take(&self.done) {
            let _ = done.send(());
        }
    }
}

/// Take a `Mutex<Option<T>>`'s contents, recovering from poisoning: a
/// poisoned lock means a panic elsewhere in the process, and refusing to
/// release the instance locks over it would strand every future start.
fn take<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

fn stop_handle(stop: &Arc<StopState>, files_root: Option<PathBuf>) -> StopHandle {
    let stop = Arc::clone(stop);
    StopHandle::new(move || {
        let stop = Arc::clone(&stop);
        let files_root = files_root.clone();
        async move { run_stop(&stop, files_root).await }
    })
}

/// The clean stop, in order: sweep the file store, then run the process
/// tail.
///
/// The sweep is here and **not** in [`StopState::finalize`] on purpose.
/// This runs only on the wire path, after the engine has drained the
/// mutation barrier and reaped every child, so nothing can be mid-upload
/// and no shell is left holding a path that is about to vanish. The
/// SIGTERM direct-finalize fallback reaches `finalize` without coming
/// through here, and must not sweep: its children may still be alive,
/// and the next start sweeps anyway.
///
/// On the blocking pool and awaited, for the reason the writes are:
/// `$HOME` can be a network mount.
async fn run_stop(stop: &StopState, files_root: Option<PathBuf>) {
    if let Some(root) = files_root {
        let files = root.display().to_string();
        match tokio::task::spawn_blocking(move || crate::files::sweep(&root)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => error!(%files, %error, "could not sweep the file store"),
            Err(error) => error!(%files, %error, "the file-store sweep task failed"),
        }
    }
    stop.finalize();
}

/// Route SIGTERM and SIGINT into the wire's own stop.
fn spawn_signal_stops(stop: &Arc<StopState>) {
    use tokio::signal::unix::{signal, SignalKind};

    for (name, kind) in [
        ("SIGTERM", SignalKind::terminate()),
        ("SIGINT", SignalKind::interrupt()),
    ] {
        let stop = Arc::clone(stop);
        let mut stream = match signal(kind) {
            Ok(stream) => stream,
            // Not fatal: a session that cannot listen for SIGTERM still
            // serves, and `session.stop` still stops it cleanly.
            Err(error) => {
                warn!(signal = name, %error, "could not install a signal handler");
                continue;
            }
        };
        tokio::spawn(async move {
            if stream.recv().await.is_none() {
                return;
            }
            info!(signal = name, "signal received; requesting a session stop");
            request_stop_over_the_wire(&stop, name).await;
        });
    }
}

/// Dial our own socket and send `session.stop`, so a signal takes
/// exactly the path a client's stop takes.
async fn request_stop_over_the_wire(stop: &StopState, signal_name: &str) {
    let dial = async {
        let mut client = IpcClient::connect(&stop.socket_path).await?;
        client
            .call_raw(ops::SESSION_STOP, SessionStopParams {})
            .await
    };
    match tokio::time::timeout(SIGNAL_STOP_TIMEOUT, dial).await {
        Ok(Ok(_)) => {}
        // The dispatcher answered, so the session is alive and owns its
        // own shutdown — most often `shutting-down`, meaning a client's
        // stop got there first. Finalizing behind it would cancel the
        // connection its reply is still travelling on.
        Ok(Err(roost_ipc::ClientError::Server { code, message })) => {
            info!(
                signal = signal_name,
                code, message, "the session is already stopping; leaving it to finish"
            );
        }
        // The socket is unreachable or the stop outran its budget.
        // Either way a SIGTERM must still bring the process down, so
        // take the tail directly.
        Ok(Err(error)) => {
            warn!(signal = signal_name, %error, "self-dialed session.stop failed; finalizing directly");
            stop.finalize();
        }
        Err(_) => {
            warn!(
                signal = signal_name,
                "self-dialed session.stop timed out; finalizing directly"
            );
            stop.finalize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `StopState` with no locks, no socket and no completion signal —
    /// everything the tail touches is optional, which is what lets the
    /// two stop entrances be compared without standing a session up.
    fn stop_state(root: &std::path::Path) -> Arc<StopState> {
        let (cancel, _rx) = watch::channel(false);
        Arc::new(StopState {
            finalized: AtomicBool::new(false),
            cancel,
            workspace: Arc::new(Workspace::open(root.join("state.json"))),
            socket_path: root.join("roost.sock"),
            socket_identity: OnceLock::new(),
            locks: Mutex::new(None),
            done: Mutex::new(None),
        })
    }

    fn seeded_store(root: &std::path::Path) -> PathBuf {
        let files = root.join("files");
        std::fs::create_dir_all(files.join("4b9d1e7f0a3c5e21")).expect("seed the store");
        std::fs::write(files.join("4b9d1e7f0a3c5e21/shot.png"), b"png").expect("seed a file");
        files
    }

    #[tokio::test]
    async fn the_clean_stop_sweeps_the_file_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = seeded_store(dir.path());
        let stop = stop_state(dir.path());

        run_stop(&stop, Some(files.clone())).await;

        assert!(!files.exists(), "a clean stop must leave no store behind");
        assert!(stop.finalized.load(Ordering::Acquire), "and still finalize");
    }

    /// The other entrance, and the reason the sweep does not live in
    /// `finalize`: a SIGTERM whose self-dial could not reach the socket
    /// has run none of the engine's stop, so its children may still be
    /// alive and holding paths in the store. The next start sweeps.
    #[test]
    fn a_direct_finalize_leaves_the_file_store_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = seeded_store(dir.path());
        let stop = stop_state(dir.path());

        stop.finalize();

        assert!(
            files.join("4b9d1e7f0a3c5e21/shot.png").exists(),
            "the direct finalize must not sweep"
        );
        assert!(stop.finalized.load(Ordering::Acquire));
    }

    /// A session with no store still stops.
    #[tokio::test]
    async fn a_stop_without_a_store_still_finalizes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stop = stop_state(dir.path());

        run_stop(&stop, None).await;

        assert!(stop.finalized.load(Ordering::Acquire));
    }

    /// The knob is a test-harness knob and nothing else: a production
    /// daemon keeps the shipped window whatever the environment says,
    /// and a value that is not a positive count falls back to it rather
    /// than serving a ring nothing can resume from.
    #[test]
    fn the_replay_window_override_is_test_mode_only_and_falls_back() {
        assert_eq!(parse_replay_window(true, Some("4")), Some(4));

        assert_eq!(
            parse_replay_window(false, Some("4")),
            None,
            "a production daemon never reads it"
        );
        for unusable in ["0", "-1", "many", "4.5", ""] {
            assert_eq!(
                parse_replay_window(true, Some(unusable)),
                None,
                "{unusable:?} must fall back to the default window"
            );
        }
        assert_eq!(parse_replay_window(true, None), None);
    }
}
