//! Every named constant this crate owns, in one module.
//!
//! Collected rather than scattered because most of them are timing
//! policy — a reader deciding whether a start is "slow" or "wedged"
//! should be able to see all the budgets at once, and a CI runner that
//! needs one widened should not have to hunt.

use std::time::Duration;

/// What this crate shares with whoever launches it: two constants and
/// the timeout-scale reader.
///
/// They are defined in `roost-ipc` because `roostctl` sets/reads them
/// too, and a shell-integration binary must not depend on the engine to
/// learn them. Re-exported here so this module stays the one place to
/// look for a named constant.
pub use roost_ipc::session_launch::{timeout_scale, LAUNCH_CWD_ENV, MAX_VERDICT_BYTES};

/// Test-mode override for the `libghostty_build` string this session
/// reports and negotiates against (plan 037 §3.7).
///
/// Read **only** when `ROOST_TEST_MODE=1`. The build string is the
/// attach negotiation's identity check: a client whose libghostty pin
/// differs cannot decode this session's snapshots, and `tab.attach`
/// refuses it with `build-mismatch`. That refusal drives a whole
/// user-facing flow (the upgrade/restart dialog), and reproducing it
/// otherwise takes a second binary built against a second Ghostty pin —
/// which no CI lane can produce. Setting this makes the mismatch a
/// one-line fixture instead.
pub const FAKE_BUILD_ENV: &str = "ROOST_SESSION_FAKE_BUILD";

/// `umask` the daemon installs before it creates anything. Everything
/// downstream — state dir, log dir, `state.json`, crash reports, the
/// socket — inherits this posture rather than restating it.
pub const PROCESS_UMASK: libc::mode_t = 0o077;

/// Mode for the directories the daemon creates itself (state, log).
/// The socket dir is `validate_runtime_dir`'s business and it enforces
/// the same value.
pub const OWNER_ONLY_DIR_MODE: u32 = 0o700;

/// Base budget for [`parent_ready_timeout`].
const PARENT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the forking parent waits for the child's readiness verdict
/// before giving up and reporting a failed start.
///
/// Generous even unscaled: the child hydrates the whole saved layout —
/// one `forkpty` and shell exec per restored tab — before it says
/// `ready`, and a cold CI runner restoring a dozen tabs is slow without
/// being wedged. Scaled on top of that, because "slow" is exactly what
/// `ROOST_TEST_TIMEOUT_SCALE` exists to say.
///
/// Expiring does not kill the child: the parent only stops waiting. A
/// session that reports `ready` a second later is still a live session,
/// which a caller finds by dialling the socket.
pub fn parent_ready_timeout() -> Duration {
    PARENT_READY_TIMEOUT.mul_f64(timeout_scale())
}

/// `(cols, rows)` every tab this session opens defaults to.
///
/// A headless session has no window to measure, so it states a size
/// instead of inheriting a UI's 80x24. Restored and IPC-opened tabs both
/// land here when the caller does not ask for a size.
pub const DEFAULT_TAB_COLS: u16 = 120;
pub const DEFAULT_TAB_ROWS: u16 = 40;

/// How long `serve` waits, after the accept loop unwinds, for the
/// detached stop finalizer to finish unlinking the socket and releasing
/// the locks.
///
/// The finalizer fires the shutdown token *first*, so `run_until`
/// returns while the rest of the tail is still in flight; without this
/// join the process could exit with its socket still on disk.
pub const FINALIZE_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for the signal path's self-dialed `session.stop`, from
/// connect to reply.
///
/// Must comfortably exceed `roost_engine::ipc::SESSION_STOP_SOFT_DEADLINE`
/// (5s) plus the post-SIGKILL tail — the reply does not come back until
/// every child has been accounted for. On expiry the signal path stops
/// waiting for the wire and finalizes directly, so a broken socket can
/// never leave a SIGTERM unanswered.
pub const SIGNAL_STOP_TIMEOUT: Duration = Duration::from_secs(30);
