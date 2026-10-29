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
//! 4. **Vouch for the socket dir before the flock.** Both
//!    `single_instance::acquire` and `IpcServer::bind` `create_dir_all`
//!    at the default mode, and the validators reject rather than repair
//!    — so a leaf either of them materializes first is refused from then
//!    on, with no recovery short of an `rmdir`.
//! 5. **Logger and panic hook before the locks**, so a refusal is
//!    recorded rather than merely returned.
//! 6. **A session on an old path before the locks.** The locks only
//!    contend with a session on *this* socket and state dir; a pre-M2
//!    daemon on its old socket, with a state dir of its own, would never
//!    meet them and would end up running beside this one.
//! 7. **Locks before the runtime**, so a losing racer costs one process
//!    start and nothing else.
//! 8. **The agent-hook path before the fork**, because it is derived
//!    from `current_exe()` and the fork `chdir`s away from the directory
//!    a relative one would need — see [`crate::agent_hook`] for the
//!    other half of the reason.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use roost_engine::single_instance::{self, LocksError};
use roost_ipc::old_session;
use roost_ipc::paths::BundleProfile;
use tracing::info;

use crate::consts::{
    FirstProject, LAUNCH_CWD_ENV, NO_SEED_ENV, OWNER_ONLY_DIR_MODE, PROCESS_UMASK,
};
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
/// invocation with no `roostctl` in front of it. The result is used only
/// for the startup log line below — a first-ever session seeds its
/// project at `$HOME` like every UI (plan 063 §D4), not at this
/// directory.
pub fn capture_launch_cwd() -> PathBuf {
    let hint = std::env::var_os(LAUNCH_CWD_ENV);
    std::env::remove_var(LAUNCH_CWD_ENV);
    hint.map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Read and consume the no-seed hint (plan 063 §D8 phase 1).
///
/// [`capture_launch_cwd`]'s sibling, in every respect that matters:
/// captured here, at the one point in the process's life that is before
/// the fork and before any thread, and **erased** so no shell this
/// session spawns can inherit it. A daemon started by any other route
/// finds nothing and seeds normally, which is what makes this a hint on
/// one spawn rather than a change to §D4's rule.
pub fn capture_first_project() -> FirstProject {
    let hint = std::env::var(NO_SEED_ENV).ok();
    std::env::remove_var(NO_SEED_ENV);
    FirstProject::from_env_value(hint.as_deref())
}

/// Install the file-creation posture for everything this process makes.
pub fn set_process_umask() {
    // SAFETY: `umask` cannot fail and returns the previous mask, which
    // this process has no use for. Called before any thread exists, so
    // the process-global write races nothing.
    unsafe { libc::umask(PROCESS_UMASK) };
}

/// Steps 3 through 8. Returns only in the process that will actually
/// serve — the forking parent exits inside [`daemonize::daemonize`].
///
/// The profile is resolved by the caller, before the fork, so a
/// resolution failure reaches the user on their own terminal rather than
/// down a pipe from a child that never got far enough to log.
pub fn start(
    profile: &BundleProfile,
    foreground: bool,
    launch_cwd: &Path,
    first_project: FirstProject,
    readiness: &mut Readiness,
) -> Result<Outcome> {
    // Step 0, and it has to be step 0: before the fork (which `chdir`s
    // to `/`, so a relative `current_exe` would stop resolving) and
    // long before any tab is spawned. Bootstrap replaces this binary by
    // an atomic rename, and a still-running old session's
    // `/proc/self/exe` then reads `… (deleted)` — resolving lazily at
    // the first spawn would hand tabs a path that no longer exists.
    roost_engine::process::set_agent_hook_binary(crate::agent_hook::hook_binary());
    // The same window, the same reason: `session.identify.exe_path`
    // must name the binary this daemon started from.
    let exe_path = crate::identity::canonical_exe();

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
        ?first_project,
        "roost-session starting"
    );

    refuse_beside_an_old_path_session(profile)?;

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
            // socket: the holder is on another socket, and none this
            // build could find. Two processes writing one `state.json`
            // is the thing the lock exists to stop, so refuse.
            Err(LocksError::StateHeld { pid, path }) => {
                anyhow::bail!("{}", state_held(&path, pid));
            }
            Err(error) => return Err(anyhow::anyhow!("single-instance lock failed: {error}")),
        };

    let config = SessionConfig::from_profile(profile, first_project, exe_path);
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
/// that one goes through its profile's validation
/// ([`BundleProfile::prepare_socket_dir`]: owner-only, ours, never
/// through a symlink) and is rejected rather than repaired. State and
/// log dirs are ours to create at `0700`. The socket dir comes first:
/// on Linux its `<ns>` is also the log dir, and a `create_dir_all` here
/// first would skip the checks that dir gets.
fn prepare_directories(profile: &BundleProfile) -> Result<()> {
    profile.prepare_socket_dir()?;
    create_owner_only_dir(&profile.state_dir)?;
    create_owner_only_dir(&profile.log_dir)?;
    Ok(())
}

/// [`old_session::check_start`], on a runtime of its own that is gone
/// before this returns: `start` is past its fork here, and the serving
/// runtime is built after the locks.
fn refuse_beside_an_old_path_session(profile: &BundleProfile) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build the old-session probe runtime")?
        .block_on(old_session::check_start(&profile.socket_path))
        .map_err(anyhow::Error::msg)
}

/// The state lock is held, and no socket this build can find belongs to
/// its holder. The holder's pid is the kernel's word where there is one
/// (`lock_holder`), never just the pid written in the file — that one
/// outlives a crashed holder.
fn state_held(lock: &Path, pid_in_file: i32) -> String {
    #[cfg(target_os = "linux")]
    let holder = {
        let _ = pid_in_file;
        single_instance::lock_holder(lock)
    };
    #[cfg(not(target_os = "linux"))]
    let holder = u32::try_from(pid_in_file).ok().filter(|pid| *pid > 0);
    let pid = holder.map_or_else(|| "pid unknown".to_string(), |pid| format!("pid {pid}"));
    format!(
        "a session holds the state lock; its socket wasn't found ({pid}, lock: {}): another \
         session is using this state directory, and this one refuses to write state.json \
         beside it",
        lock.display()
    )
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
