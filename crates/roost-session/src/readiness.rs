//! The channel a starting session says its one line on.
//!
//! The line's *format* — [`Verdict`] — belongs to
//! [`roost_ipc::session_launch`], because `roostctl` parses it and must
//! not depend on this crate to do so. What lives here is the sink: which
//! fd the verdict goes to, and the write-once-then-close discipline that
//! stops a reader from ever waiting on a second line.

use std::fs::File;
use std::io::Write;

pub use roost_ipc::session_launch::Verdict;

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
    fn reporting_twice_writes_once() {
        let mut readiness = Readiness::Discard;
        readiness.report(&Verdict::Ready(1));
        assert!(matches!(readiness, Readiness::Discard));
    }
}
