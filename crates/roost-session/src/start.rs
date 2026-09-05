//! Process startup, in the order the steps have to happen.
//!
//! Almost every constraint here is an ordering constraint, and each one
//! is load-bearing:
//!
//! 1. **Capture the launch cwd, then erase the hint.** It arrives in the
//!    environment because the fork happens long before any IPC exists.
//!    Removing it immediately is what keeps it out of every shell the
//!    session later spawns.
//! 2. **`umask` before anything is created.** The state dir, the log,
//!    `state.json`, crash reports and the socket all inherit this
//!    posture instead of each restating it.
//! 3. **Fork before any thread exists.** `fork(2)` copies only the
//!    calling thread; a runtime, a logger with a writer thread, or a
//!    signal driver started first would leave the child holding locks
//!    nobody will ever release.
//! 4. **Validate the runtime dir before the flock.** Both
//!    `single_instance::acquire` and `IpcServer::bind` `create_dir_all`
//!    at the default mode, and `validate_runtime_dir` rejects rather
//!    than repairs — so a leaf either of them materializes first is
//!    refused from then on, with no recovery short of an `rmdir`.
//! 5. **Logger and panic hook before the locks**, so a refusal is
//!    recorded rather than merely returned.
//! 6. **Locks before the runtime**, so a losing racer costs one process
//!    start and nothing else.
//! 7. **The agent-hook path before the fork**, because it is derived
//!    from `current_exe()` and the fork `chdir`s away from the directory
//!    a relative one would need — see [`crate::agent_hook`] for the
//!    other half of the reason.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use roost_engine::single_instance::{self, LocksError};
use roost_ipc::paths::BundleProfile;
use roost_ipc::validate_runtime_dir;
use tracing::info;

use crate::consts::{LAUNCH_CWD_ENV, OWNER_ONLY_DIR_MODE, PROCESS_UMASK};
use crate::readiness::{Readiness, Verdict};
use crate::serve::{serve, SessionConfig};
use crate::{daemonize, logging};

/// What a start attempt settled on, once it is past the point where a
/// failure would be an error.
pub enum Outcome {
    /// The session ran and stopped cleanly.
    Served,
    /// Another session already owns this profile's socket. `pid` is the
    /// winner's, as recorded in the lock file it holds — `None` when
    /// that could not be read.
    AlreadyRunning(Option<i32>),
}

/// Read and consume the launch-directory hint.
///
/// Consumed rather than merely read: a session spawns shells, and an
/// inherited `ROOST_SESSION_LAUNCH_CWD` would be a stale directory
/// pointer in the environment of every one of them. Removed before the
/// fork, so no code path can ever race a PTY spawn against it.
///
/// The fallback is the process cwd, which is right for a direct
/// invocation with no `roostctl` in front of it.
pub fn capture_launch_cwd() -> PathBuf {
    let hint = std::env::var_os(LAUNCH_CWD_ENV);
    std::env::remove_var(LAUNCH_CWD_ENV);
    hint.map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Install the file-creation posture for everything this process makes.
pub fn set_process_umask() {
    // SAFETY: `umask` cannot fail and returns the previous mask, which
    // this process has no use for. Called before any thread exists, so
    // the process-global write races nothing.
    unsafe { libc::umask(PROCESS_UMASK) };
}

/// Steps 3 through 9. Returns only in the process that will actually
/// serve — the forking parent exits inside [`daemonize::daemonize`].
///
/// The profile is resolved by the caller, before the fork, so a
/// resolution failure reaches the user on their own terminal rather than
/// down a pipe from a child that never got far enough to log.
pub fn start(
    profile: &BundleProfile,
    foreground: bool,
    launch_cwd: PathBuf,
    readiness: &mut Readiness,
) -> Result<Outcome> {
    // Step 0, and it has to be step 0: before the fork (which `chdir`s
    // to `/`, so a relative `current_exe` would stop resolving) and
    // long before any tab is spawned. Bootstrap replaces this binary by
    // an atomic rename, and a still-running old session's
    // `/proc/self/exe` then reads `… (deleted)` — resolving lazily at
    // the first spawn would hand tabs a path that no longer exists.
    roost_engine::process::set_agent_hook_binary(crate::agent_hook::hook_binary());

    if !foreground {
        *readiness = daemonize::daemonize()?;
    }

    prepare_directories(profile)?;

    logging::init(profile)?;
    roost_engine::crash::install_panic_hook(
        profile.log_dir.clone(),
        profile.app_label,
        env!("CARGO_PKG_VERSION"),
    );
    info!(
        profile = profile.kind.as_str(),
        foreground,
        launch_cwd = %launch_cwd.display(),
        "roost-session starting"
    );

    let locks =
        match single_instance::acquire_locks(profile.socket_lock_path(), profile.state_lock_path())
        {
            Ok(locks) => locks,
            // A session is not a window: there is nothing to raise and
            // no second instance to become. Report who won and leave.
            Err(LocksError::SocketHeld { pid, path }) => {
                info!(pid, lock = %path.display(), "another session owns this socket");
                // `acquire` records 0 when the holder's pid could not be
                // read back, which is a lost diagnostic rather than a
                // different outcome — so the verdict drops the suffix
                // instead of naming a pid that means "unknown".
                return Ok(Outcome::AlreadyRunning((pid > 0).then_some(pid)));
            }
            // We hold the socket lock, so nothing is listening on our
            // socket: the holder is on a different runtime dir, and
            // there is no socket to reach it through. Two processes
            // writing one `state.json` is the thing the lock exists to
            // stop, so refuse.
            Err(LocksError::StateHeld { pid, path }) => {
                anyhow::bail!(
                    "another session (pid {pid}) is using this state directory; \
                     refusing to write state.json from two processes (lock: {})",
                    path.display()
                );
            }
            Err(error) => return Err(anyhow::anyhow!("single-instance lock failed: {error}")),
        };

    let config = SessionConfig::from_profile(profile, launch_cwd);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("roost-session")
        .build()
        .context("build the session tokio runtime")?;
    let served = runtime.block_on(serve(config, locks, readiness));
    // Not a plain drop: the PTY readers are `spawn_blocking` loops that
    // a dropped runtime waits out, and one held open by a descendant
    // that outlived its shell would hang the exit. The session has
    // already flushed, reaped, unlinked and unlocked by this point.
    runtime.shutdown_background();
    served?;
    Ok(Outcome::Served)
}

/// Create (or vouch for) the three directories a session writes into.
///
/// The socket's own `0600` is only worth what its directory is worth, so
/// the runtime dir goes through the full validation — owner-only, ours,
/// no symlinked component, no writable ancestor without the sticky bit —
/// and rejects rather than repairs. State and log dirs are ours to
/// create at `0700`.
fn prepare_directories(profile: &BundleProfile) -> Result<()> {
    let socket_dir = profile.socket_path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "session socket path {} has no parent directory",
            profile.socket_path.display()
        )
    })?;
    validate_runtime_dir(socket_dir)
        .with_context(|| format!("validate runtime dir {}", socket_dir.display()))?;
    create_owner_only_dir(&profile.state_dir)?;
    create_owner_only_dir(&profile.log_dir)?;
    Ok(())
}

fn create_owner_only_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(OWNER_ONLY_DIR_MODE);
    builder
        .create(path)
        .with_context(|| format!("create {}", path.display()))
}

/// Relay `outcome` on the readiness channel and translate it into a
/// process exit status. Losing a start race is a successful no-op.
pub fn report(outcome: &Outcome, readiness: &mut Readiness) -> i32 {
    let verdict = match outcome {
        // `serve` already reported `ready` at the moment it began
        // answering; by the time it returns the session is over, and a
        // second verdict would be read as another session's.
        Outcome::Served => return 0,
        Outcome::AlreadyRunning(pid) => Verdict::AlreadyRunning(*pid),
    };
    readiness.report(&verdict);
    verdict.exit_code()
}
