//! Every named constant this crate owns, in one module.
//!
//! Collected rather than scattered because most of them are timing
//! policy — a reader deciding whether a start is "slow" or "wedged"
//! should be able to see all the budgets at once, and a CI runner that
//! needs one widened should not have to hunt.

use std::time::Duration;

/// Consumed-once hint naming the directory the user ran `roostctl
/// session start` from.
///
/// The daemon `chdir("/")`s before it does anything else, so the launch
/// cwd cannot be recovered later — and it is what seeds the first
/// project on a fresh state file. It travels as an env var because the
/// fork happens before any IPC exists, and it is removed from the
/// environment the instant it is read so no PTY can inherit it.
pub const LAUNCH_CWD_ENV: &str = "ROOST_SESSION_LAUNCH_CWD";

/// `umask` the daemon installs before it creates anything. Everything
/// downstream — state dir, log dir, `state.json`, crash reports, the
/// socket — inherits this posture rather than restating it.
pub const PROCESS_UMASK: libc::mode_t = 0o077;

/// Mode for the directories the daemon creates itself (state, log).
/// The socket dir is `validate_runtime_dir`'s business and it enforces
/// the same value.
pub const OWNER_ONLY_DIR_MODE: u32 = 0o700;

/// Multiplier applied to the budgets that wait on somebody else's work.
///
/// The same `ROOST_TEST_TIMEOUT_SCALE` the Python harness reads, honoured
/// here so a loaded CI runner widens the daemon's own waits rather than
/// only the driver's. Nonsense values (unparseable, zero, negative) fall
/// back to 1.0: a bad env var must not be able to disarm a timeout.
pub fn timeout_scale() -> f64 {
    std::env::var("ROOST_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|factor| factor.is_finite() && *factor > 0.0)
        .unwrap_or(1.0)
}

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

/// Cap on the readiness frame the parent will buffer. A verdict line is
/// tens of bytes; anything past this is a child writing garbage down the
/// pipe, and the parent stops rather than growing without bound.
pub const MAX_VERDICT_BYTES: usize = 8 * 1024;

/// `(cols, rows)` every tab this session opens defaults to.
///
/// A headless session has no window to measure, so it states a size
/// instead of inheriting a UI's 80x24. Restored and IPC-opened tabs both
/// land here when the caller does not ask for a size.
pub const DEFAULT_TAB_COLS: u16 = 120;
pub const DEFAULT_TAB_ROWS: u16 = 40;

/// The workspace announces `TabOpened` before the supervisor has
/// spawned the PTY (the dispatcher opens the row first so a failed spawn
/// can roll it back), so the drain attacher polls for the live PTY
/// rather than assuming one.
pub const ATTACH_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long that poll runs before the attacher stops waiting for a PTY.
///
/// This is the backstop, not the mechanism: a PTY that came and went
/// before the drain looked is reported by the supervisor's `TabExited`,
/// which the attacher subscribes to before any tab exists. The deadline
/// only covers the case where that event was lost (a lagged lifecycle
/// broadcast), so it is long — nothing should ever reach it — and
/// reaching it still closes the row rather than leaving a phantom.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

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
