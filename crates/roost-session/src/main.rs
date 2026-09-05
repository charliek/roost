//! `roost-session` — the headless host-session daemon.
//!
//! Thin by design: parse, capture the launch directory, set the file
//! posture, and hand off to the library. Everything with a constraint
//! behind it lives in `start.rs`, which documents why each step is where
//! it is.

use anyhow::Context;
use clap::{Parser, Subcommand};
use roost_ipc::paths::BundleProfile;
use roost_session::{Outcome, Readiness, Verdict};

#[derive(Debug, Parser)]
#[command(
    name = "roost-session",
    about = "Headless Roost host session: a workspace and its shells, with no UI",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the session for this machine's session profile.
    ///
    /// Prints exactly one line on stdout — `ready pid=N`,
    /// `already-running[ pid=N]`, or `error: <reason>` — and exits 0 for
    /// the first two.
    Start {
        /// Stay in the foreground instead of forking into the
        /// background. The readiness line still goes to stdout; the log
        /// still goes to the log file, with a copy on stderr.
        #[arg(long)]
        foreground: bool,
    },
    /// Bridge stdin/stdout to this machine's session socket.
    ///
    /// The far side of the SSH transport, one process per accepted
    /// client connection. stdout carries wire bytes and nothing else.
    ClientBridge,
    /// Print this build's offline identity and exit.
    ///
    /// Prints exactly one `SessionBinaryIdentity` JSON line on stdout
    /// and exits 0. No socket, no profile, no side effects — answerable
    /// by a binary that has never run; the bootstrap probe (plan 039
    /// §3.1) execs this to compare a remote candidate's build against
    /// the client's before installing or starting anything.
    Identify,
    /// The one agent hook entrypoint, on the host side (plan 046 §3.2).
    ///
    /// Reads the agent's JSON payload from stdin, takes the event name
    /// from the payload, reports it to the session named by
    /// `ROOST_SOCKET`, and always prints `{}` and exits 0. The session
    /// binary carries this itself so a host needs no second file and the
    /// adapter cannot drift from the daemon it reports to.
    AgentHook {
        /// `claude`, `grok`, `codex`, `cursor`, or `opencode`.
        agent: String,
    },
}

fn main() -> ! {
    let cli = Cli::parse();
    let foreground = match cli.command {
        // Exits before `Readiness::Stdout` is installed below, because
        // for the bridge stdout *is* the wire: one stray line on it
        // corrupts the client's stream. `identify` gets the same
        // early-exit treatment for the same reason — its stdout is a
        // machine-read contract too. So does `agent-hook`, on a third
        // reason: it fires inside a *running* session's tabs, where a
        // readiness line would be read as a start verdict and the log
        // file already has the live daemon's appender on it.
        Command::ClientBridge => std::process::exit(roost_session::bridge::run()),
        Command::Identify => std::process::exit(roost_session::identity::run()),
        Command::AgentHook { agent } => std::process::exit(roost_session::agent_hook::run(&agent)),
        Command::Start { foreground } => foreground,
    };

    // Stdout in *both* modes, and set before the first fallible step.
    //
    // Until the fork happens, this process is the one the caller is
    // reading, so every failure on the way there — resolving the
    // profile, creating the pipe, the `fork` itself — still lands as one
    // machine-readable line where the caller is looking. `daemonize`
    // swaps this for the pipe end in the child, at which point the
    // parent becomes the one printing and the contract is unchanged:
    // exactly one line on stdout, whatever happens.
    let mut readiness = Readiness::Stdout;

    let code = match run(foreground, &mut readiness) {
        Ok(outcome) => roost_session::report(&outcome, &mut readiness),
        Err(error) => {
            let verdict = Verdict::Error(format!("{error:#}"));
            // stderr as well, because either can be the only one that
            // lands: a failure before `logging::init` has no subscriber,
            // and a failure after the fork has `/dev/null` for stderr.
            tracing::error!(%error, "session start failed");
            eprintln!("roost-session: {error:#}");
            readiness.report(&verdict);
            verdict.exit_code()
        }
    };

    // Never a plain `return`: the PTY supervisor's readers are blocking
    // tasks that need not have finished, and dropping the runtime would
    // wait for them. The session has already flushed its state, reaped
    // its children, unlinked its socket and released its locks — there
    // is nothing left worth waiting on.
    std::process::exit(code);
}

/// Everything that can fail, in one place, so a single error arm owns
/// the verdict.
fn run(foreground: bool, readiness: &mut Readiness) -> anyhow::Result<Outcome> {
    // Before the fork and before any thread: the profile so a bad
    // environment fails on the caller's terminal, the launch directory
    // so it is captured (and erased) while it still exists.
    let profile = BundleProfile::session().context("resolve the session bundle profile")?;
    let launch_cwd = roost_session::capture_launch_cwd();
    roost_session::set_process_umask();
    roost_session::start(&profile, foreground, launch_cwd, readiness)
}
