//! The one line a starting session says about itself, and the channel
//! it says it on.
//!
//! A `roostctl session start` has to learn three things and nothing
//! else: the session is up (and which pid it is), somebody else was
//! already up, or the start failed and why. That is one line of ASCII,
//! written once, on a channel that closes immediately afterwards so the
//! reader is never left guessing whether more is coming.
//!
//! Two transports, one format. Daemonized, the line goes down the
//! readiness pipe to the forking parent, which relays it and exits with
//! the matching status. Under `--foreground` it goes to **stdout**,
//! which carries this line and nothing else — the log lives in a file
//! and the console tee is on stderr, so a caller can read stdout
//! without a parser.

use std::fs::File;
use std::io::Write;

use crate::consts::MAX_VERDICT_BYTES;

/// What a start attempt turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Serving. The pid is this process's — the daemonized child's, not
    /// the parent's.
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
            // reader stops at the first.
            Self::Error(reason) => write!(f, "error: {}", one_line(reason)),
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

    /// Parse a verdict line back. The parent needs this to decide its
    /// own exit status; an unrecognized line is reported as an error
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

/// Collapse an error chain to one line: the frame format is
/// newline-delimited, so an embedded newline would truncate the verdict
/// at the reader.
fn one_line(reason: &str) -> String {
    let flattened = reason
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if flattened.len() > MAX_VERDICT_BYTES {
        // Truncate on a char boundary — `MAX_VERDICT_BYTES` is a byte
        // budget and the reason may be UTF-8.
        let mut cut = MAX_VERDICT_BYTES;
        while cut > 0 && !flattened.is_char_boundary(cut) {
            cut -= 1;
        }
        return flattened[..cut].to_string();
    }
    flattened
}

/// Where a starting session writes its verdict.
#[derive(Debug)]
pub enum Readiness {
    /// The write end of the fork's readiness pipe. Closed as soon as
    /// the verdict is written, so the parent sees EOF right behind it.
    Pipe(File),
    /// `--foreground`: stdout, which carries this line only.
    Stdout,
    /// In-process (tests): the verdict has no reader. Kept as a variant
    /// rather than an `Option<Readiness>` so every call site reports
    /// unconditionally and the "who is listening" question stays here.
    Discard,
}

impl Readiness {
    /// Announce the outcome. At most one verdict per start: a later
    /// call is dropped, because the reader has already acted on the
    /// first and a second line would be read as another session's.
    pub fn report(&mut self, verdict: &Verdict) {
        let line = format!("{verdict}\n");
        match self {
            Self::Pipe(pipe) => {
                // Best-effort by construction: the parent may have died
                // (EPIPE) or timed out, and neither is a reason for a
                // serving session to stop serving.
                let _ = pipe.write_all(line.as_bytes());
                let _ = pipe.flush();
            }
            Self::Stdout => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(line.as_bytes());
                let _ = out.flush();
            }
            Self::Discard => {}
        }
        // Closing is the other half of the signal: the parent's read
        // returns EOF immediately after the line instead of waiting out
        // its whole timeout on a pipe nobody will write to again.
        *self = Self::Discard;
    }
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

    #[test]
    fn an_unrecognized_line_parses_as_an_error() {
        assert!(matches!(Verdict::parse("who knows"), Verdict::Error(_)));
        // A malformed pid is not a `ready` — the caller must not learn a
        // pid of 0 for a live session.
        assert!(matches!(Verdict::parse("ready pid=abc"), Verdict::Error(_)));
    }

    #[test]
    fn reporting_twice_writes_once() {
        let mut readiness = Readiness::Discard;
        readiness.report(&Verdict::Ready(1));
        assert!(matches!(readiness, Readiness::Discard));
    }
}
