//! The contract between whoever *launches* a host session and the
//! session itself: one env hint on the way in, one verdict line on the
//! way out.
//!
//! It lives here rather than in `roost-session` because both ends need
//! it and only one end can afford the other's dependencies. `roostctl`
//! spawns `roost-session` and parses what it says; depending on the
//! session crate to learn the format would drag the whole engine —
//! workspace, PTY supervisor, `portable-pty` — into a shell-integration
//! binary. `roost-ipc` is the crate both already depend on, and the
//! verdict line is wire format like everything else in here.
//!
//! # The verdict
//!
//! A start attempt has three outcomes and a caller needs no more than
//! that: the session is up (and which pid it is), somebody else was
//! already up, or the start failed and why. That is one line of ASCII,
//! written once, on a channel that closes immediately afterwards so the
//! reader is never left guessing whether more is coming.
//!
//! Two transports, one format. Daemonized, the line goes down the
//! readiness pipe to the forking parent, which relays it to **stdout**
//! and exits with the matching status. Under `--foreground` the session
//! writes it to stdout itself. Either way stdout carries this line and
//! nothing else — the log lives in a file and the console tee is on
//! stderr — so a caller can read stdout without a parser.

/// Consumed-once hint naming the directory the user ran `roostctl
/// session start` from.
///
/// The daemon `chdir("/")`s before it does anything else, so the launch
/// cwd cannot be recovered later — and it is what seeds the first
/// project on a fresh state file. It travels as an env var because the
/// fork happens before any IPC exists, and it is removed from the
/// environment the instant it is read so no PTY can inherit it.
pub const LAUNCH_CWD_ENV: &str = "ROOST_SESSION_LAUNCH_CWD";

/// Cap on a verdict line, newline excluded. A verdict is tens of bytes;
/// anything past this is a session writing garbage down the pipe.
///
/// Both ends are held to it: a reader stops here rather than buffering
/// without bound, and [`Verdict`]'s `Display` truncates so the **whole**
/// line — prefix included — fits. If only the reason were capped, the
/// formatter could emit a line the reader is required to reject.
pub const MAX_VERDICT_BYTES: usize = 8 * 1024;

/// Narrowest and widest [`timeout_scale`] accepted.
///
/// Both ends exist to keep a budget a budget. Above the ceiling
/// `Duration::mul_f64` overflows and **panics** (`1e300` on a 30s budget
/// is not a representable `Duration`), so an env var could crash the
/// process it was meant to slow down. Below the floor a scaled budget
/// rounds toward zero and every timeout fires instantly, which disarms
/// them just as thoroughly as a zero would. The range still spans
/// 100x faster to 1000x slower than shipped — far past any real runner.
const MIN_TIMEOUT_SCALE: f64 = 0.01;
const MAX_TIMEOUT_SCALE: f64 = 1000.0;

/// Multiplier for every budget that waits on the other end of this
/// contract — the daemon's own waits and `roostctl session`'s waits on
/// it alike. The same `ROOST_TEST_TIMEOUT_SCALE` the Python harness
/// reads, so a loaded CI runner widens every side together.
pub fn timeout_scale() -> f64 {
    parse_timeout_scale(std::env::var("ROOST_TEST_TIMEOUT_SCALE").ok().as_deref())
}

/// [`timeout_scale`]'s rule, over an already-read value.
///
/// Anything unparseable, non-finite, or outside
/// `MIN_TIMEOUT_SCALE..=MAX_TIMEOUT_SCALE` falls back to 1.0 rather than
/// being clamped: a value that far out is a typo or an attack, and the
/// shipped budget is the safe reading of both. Pure, so both crates can
/// pin the policy without mutating process-global env.
pub fn parse_timeout_scale(raw: Option<&str>) -> f64 {
    raw.and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|factor| {
            factor.is_finite() && (MIN_TIMEOUT_SCALE..=MAX_TIMEOUT_SCALE).contains(factor)
        })
        .unwrap_or(1.0)
}

/// What a start attempt turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Serving. The pid is the session process's — the daemonized
    /// child's, not the forking parent's.
    Ready(u32),
    /// Another session already owns this profile's socket. The pid is
    /// the winner's, read from the lock file it holds; `None` when that
    /// file could not be read, which is a diagnostic loss and not a
    /// different outcome — so the word stays and only the suffix goes.
    AlreadyRunning(Option<i32>),
    /// The start failed. The reason is a one-line rendering of the
    /// error chain.
    Error(String),
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(pid) => write!(f, "ready pid={pid}"),
            Self::AlreadyRunning(Some(pid)) => write!(f, "already-running pid={pid}"),
            Self::AlreadyRunning(None) => f.write_str("already-running"),
            // Newlines would turn one verdict into two frames, and the
            // reader stops at the first. The reason's budget is what is
            // left of the line's after the prefix, so the emitted line
            // is one the reader will accept.
            Self::Error(reason) => write!(
                f,
                "{ERROR_PREFIX}{}",
                one_line(reason, MAX_VERDICT_BYTES - ERROR_PREFIX.len())
            ),
        }
    }
}

impl Verdict {
    /// The exit status a *parent* relaying this verdict should take.
    /// Losing a race is a successful no-op, not a failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Ready(_) | Self::AlreadyRunning(_) => 0,
            Self::Error(_) => 1,
        }
    }

    /// Parse a verdict line back. The forking parent needs this to
    /// decide its own exit status and `roostctl` needs it to decide
    /// whether to confirm; an unrecognized line is reported as an error
    /// rather than guessed at.
    pub fn parse(line: &str) -> Self {
        let line = line.trim();
        if let Some(pid) = line.strip_prefix("ready pid=") {
            if let Ok(pid) = pid.trim().parse() {
                return Self::Ready(pid);
            }
        }
        if line == "already-running" {
            return Self::AlreadyRunning(None);
        }
        if let Some(pid) = line.strip_prefix("already-running pid=") {
            return Self::AlreadyRunning(pid.trim().parse().ok());
        }
        if let Some(reason) = line.strip_prefix("error: ") {
            return Self::Error(reason.to_string());
        }
        Self::Error(format!("unrecognized readiness verdict: {line:?}"))
    }
}

/// The one prefix that costs a verdict line part of its byte budget.
const ERROR_PREFIX: &str = "error: ";

/// Collapse an error chain to one line of at most `budget` bytes: the
/// frame format is newline-delimited, so an embedded newline would
/// truncate the verdict at the reader.
fn one_line(reason: &str, budget: usize) -> String {
    let flattened = reason
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if flattened.len() > budget {
        // Truncate on a char boundary — the budget is in bytes and the
        // reason may be UTF-8.
        let mut cut = budget;
        while cut > 0 && !flattened.is_char_boundary(cut) {
            cut -= 1;
        }
        return flattened[..cut].to_string();
    }
    flattened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_round_trip_through_their_wire_line() {
        for verdict in [
            Verdict::Ready(4321),
            Verdict::AlreadyRunning(Some(99)),
            Verdict::AlreadyRunning(None),
            Verdict::Error("state dir is busy".into()),
        ] {
            assert_eq!(Verdict::parse(&verdict.to_string()), verdict);
        }
    }

    #[test]
    fn exit_codes_treat_a_lost_race_as_success() {
        assert_eq!(Verdict::Ready(1).exit_code(), 0);
        assert_eq!(Verdict::AlreadyRunning(Some(1)).exit_code(), 0);
        assert_eq!(Verdict::AlreadyRunning(None).exit_code(), 0);
        assert_eq!(Verdict::Error("x".into()).exit_code(), 1);
    }

    /// An anyhow chain rendered with `{:#}` is single-line already, but
    /// a `Display` impl somewhere in it need not be — and a newline
    /// would truncate the verdict at the reader.
    #[test]
    fn an_error_verdict_is_always_one_line() {
        let verdict = Verdict::Error("bind failed\ncaused by: EADDRINUSE\n".into());
        let line = verdict.to_string();
        assert!(!line.contains('\n'), "{line}");
        assert_eq!(line, "error: bind failed; caused by: EADDRINUSE");
    }

    /// The prefix comes out of the same budget as the reason: a reader
    /// that rejects anything over `MAX_VERDICT_BYTES` must never be
    /// handed a line this formatter produced.
    #[test]
    fn a_whole_error_line_fits_the_budget_the_reader_enforces() {
        for reason in ["é".repeat(MAX_VERDICT_BYTES), "x".repeat(MAX_VERDICT_BYTES)] {
            let line = Verdict::Error(reason).to_string();
            assert!(
                line.len() <= MAX_VERDICT_BYTES,
                "{} bytes exceeds the cap",
                line.len()
            );
            assert!(line.starts_with(ERROR_PREFIX));
        }
        // Cut on a char boundary, so the truncated reason is still the
        // multi-byte text it started as.
        assert!(Verdict::Error("é".repeat(MAX_VERDICT_BYTES))
            .to_string()
            .starts_with("error: éé"));
    }

    #[test]
    fn a_reason_that_fits_is_not_touched() {
        assert_eq!(
            Verdict::Error("state dir is busy".into()).to_string(),
            "error: state dir is busy"
        );
    }

    #[test]
    fn an_unrecognized_line_parses_as_an_error() {
        assert!(matches!(Verdict::parse("who knows"), Verdict::Error(_)));
        // A malformed pid is not a `ready` — the caller must not learn a
        // pid of 0 for a live session.
        assert!(matches!(Verdict::parse("ready pid=abc"), Verdict::Error(_)));
        // Nor is an empty line, which is what a reader gets from a
        // channel that closed without a verdict.
        assert!(matches!(Verdict::parse(""), Verdict::Error(_)));
    }

    #[test]
    fn a_verdict_line_survives_the_trailing_newline_it_travels_with() {
        assert_eq!(Verdict::parse("ready pid=7\n"), Verdict::Ready(7));
        assert_eq!(
            Verdict::parse("already-running pid=7\r\n"),
            Verdict::AlreadyRunning(Some(7))
        );
    }

    /// The launch hint's name is a cross-process contract: `roostctl`
    /// sets it, `roost-session` consumes it, and nothing renames it
    /// without both ends moving.
    #[test]
    fn the_launch_cwd_env_name_is_frozen() {
        assert_eq!(LAUNCH_CWD_ENV, "ROOST_SESSION_LAUNCH_CWD");
    }

    #[test]
    fn a_plausible_scale_is_taken_at_face_value() {
        assert_eq!(parse_timeout_scale(Some("2.5")), 2.5);
        assert_eq!(parse_timeout_scale(Some(" 3 ")), 3.0);
        // The endpoints are inclusive.
        assert_eq!(parse_timeout_scale(Some("0.01")), MIN_TIMEOUT_SCALE);
        assert_eq!(parse_timeout_scale(Some("1000")), MAX_TIMEOUT_SCALE);
    }

    #[test]
    fn an_unusable_scale_falls_back_to_the_shipped_budget() {
        for raw in [
            None,
            Some(""),
            Some("nope"),
            Some("0"),
            Some("-2"),
            Some("NaN"),
            Some("inf"),
            // Over the ceiling: `Duration::mul_f64` panics on these.
            Some("1e300"),
            Some("1001"),
            // Under the floor: these round every budget to zero.
            Some("5e-324"),
            Some("0.0001"),
        ] {
            assert_eq!(parse_timeout_scale(raw), 1.0, "{raw:?}");
        }
    }

    /// The reason the range exists: every accepted scale must survive
    /// the multiplication it is read for, and leave a budget that is
    /// still a budget.
    #[test]
    fn every_accepted_scale_scales_a_budget_without_panicking() {
        let budget = std::time::Duration::from_secs(60);
        for raw in ["1e300", "5e-324", "0.01", "1000", "nope", "2.5", "1"] {
            let scaled = budget.mul_f64(parse_timeout_scale(Some(raw)));
            assert!(!scaled.is_zero(), "{raw} rounded the budget to zero");
        }
    }
}
