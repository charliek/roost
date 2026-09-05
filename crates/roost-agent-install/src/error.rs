//! What the install path can refuse to do, and why.
//!
//! Nothing here is logged and swallowed. Every writer returns, `ensure`
//! collects into its `errors` / `skipped` lists, and doctor renders
//! them — this crate is the reason the project's "errors are returned"
//! rule exists.
//!
//! The split matters: an [`InstallError`] is a *failure* (the machine
//! would not let us do a correct thing), while a [`SkipReason`] is a
//! deliberate refusal to touch a file we do not understand. Neither is
//! silent, but only the first is a problem to fix.

use std::path::PathBuf;

use roost_agent::Agent;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The harness jail, belt and braces. `ROOST_TEST_MODE=1` means a
    /// test is driving this machine, and a test that writes into real
    /// dotfiles is a bug with consequences no assertion can undo.
    #[error(
        "refusing to write agent hooks under ROOST_TEST_MODE=1 \
         (set ROOST_AGENT_HOOKS_FORCE=1 to override)"
    )]
    TestModeRefused,

    /// The file changed between [`crate::plan`] reading it and
    /// [`crate::apply`] writing it. Claude rewrites `settings.json` on
    /// its own schedule, so this is a real race, not a theoretical one —
    /// and the only safe answer is to do nothing and say so.
    #[error("{}: changed underneath us since it was read", .path.display())]
    ChangedUnderneath { path: PathBuf },

    /// A write failed, and putting the earlier files of the same plan
    /// back did not fully succeed. Both halves are named because the
    /// disk is now in a state neither Roost nor the user chose — codex's
    /// `hooks.json` without its trust hashes is a review dialog on every
    /// launch, and silence about it is how that becomes a mystery.
    #[error(
        "{source}; and could not be put back: {}",
        .paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    )]
    RollbackFailed {
        #[source]
        source: Box<InstallError>,
        paths: Vec<PathBuf>,
    },

    #[error("{}: not writable", .path.display())]
    ReadOnly { path: PathBuf },

    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("no home directory: neither $HOME nor a passed-in root")]
    NoHome,
}

impl InstallError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> InstallError {
        let path = path.into();
        if source.kind() == std::io::ErrorKind::PermissionDenied {
            return InstallError::ReadOnly { path };
        }
        InstallError::Io { path, source }
    }
}

/// Why an agent was left alone. Every variant is rendered by
/// `roostctl agent status` and by doctor, so "nothing happened" is
/// never a silence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The agent's config directory does not exist.
    NotPresent,
    /// The caller's skip list named it.
    SkipList,
    /// `agent-hooks = off`.
    ModeOff,
    /// The file is not JSON/TOML we can parse. Never coerced, never
    /// rewritten — a file we cannot read is a file we cannot safely put
    /// back.
    Unparseable { path: PathBuf, detail: String },
    /// The file parsed but is not the shape the agent documents (say,
    /// `hooks` present as an array). Same treatment, different cause.
    UnexpectedShape { path: PathBuf, detail: String },
    /// A file Roost owns by name exists but was not written by Roost.
    ForeignFile { path: PathBuf },
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SkipReason::NotPresent => f.write_str("not installed"),
            SkipReason::SkipList => f.write_str("skip-list"),
            SkipReason::ModeOff => f.write_str("agent-hooks = off"),
            SkipReason::Unparseable { path, detail } => {
                write!(f, "{}: unreadable ({detail})", path.display())
            }
            SkipReason::UnexpectedShape { path, detail } => {
                write!(f, "{}: unexpected shape ({detail})", path.display())
            }
            SkipReason::ForeignFile { path } => {
                write!(f, "{}: not written by Roost", path.display())
            }
        }
    }
}

/// Something true about the user's files that is worth saying out loud
/// but is not a reason to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// A hook that looks like Roost's but is not byte-equal to any
    /// version Roost wrote. Left exactly where it is — ownership is
    /// exact match — and named here so doctor can explain why an
    /// apparently-wired agent is also getting a fresh entry.
    ModifiedRoostEntry { path: PathBuf, event: String },
    /// The state record could not be read. The agents are rescanned
    /// anyway, so this costs the "which files did we touch on a machine
    /// we no longer have" list, not correctness.
    UnreadableRecord { path: PathBuf, detail: String },
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Warning::ModifiedRoostEntry { path, event } => write!(
                f,
                "{}: {event} carries a modified Roost entry; left in place",
                path.display()
            ),
            Warning::UnreadableRecord { path, detail } => {
                write!(f, "{}: state record unreadable ({detail})", path.display())
            }
        }
    }
}

/// An [`InstallError`] with the agent it happened to.
#[derive(Debug)]
pub struct AgentError {
    pub agent: Agent,
    pub error: InstallError,
}

/// A [`SkipReason`] with the agent it applies to.
#[derive(Debug, Clone)]
pub struct AgentSkip {
    pub agent: Agent,
    pub reason: SkipReason,
}

/// A [`Warning`] with the agent it came from.
#[derive(Debug, Clone)]
pub struct AgentWarning {
    pub agent: Agent,
    pub warning: Warning,
}
