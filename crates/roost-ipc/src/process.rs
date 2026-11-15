//! The process layer: which process a tab's agent state belongs to, and
//! what that process's fate does to the state (#621, #519).
//!
//! Pure, like [`crate::agent`]. `roost-engine` reads the process table and
//! the terminal's foreground group and hands the evidence in; these
//! functions decide what it does, and `tests/agent-state-fixtures/process.json`
//! pins them. Nothing here reads a clock, and nothing ends on time alone:
//! a stamp ends only on evidence that its process is gone.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::{self, AgentLifecycle, AgentTabState};

/// A process a layer's state belongs to: the leader of the group that was
/// in front when the layer became live. The start time is what tells a
/// reused pid apart. Linux's has no sub-second part, so it reads
/// `(ticks since boot, 0)`; macOS's is `(seconds, microseconds)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stamp {
    pub pid: i64,
    pub start: (u64, u64),
    pub pgid: i64,
    pub sid: i64,
}

/// What a read of a stamped process found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Running,
    /// Stopped by job control (`Ctrl-Z`). A traced process is running.
    Stopped,
    /// A zombie, no process at that pid, another process at it (start
    /// time), or the process left the tab's session.
    Gone,
    /// The read could not say: another user's process hidden from us, a
    /// failed read, a line that would not parse.
    Unknown,
}

/// Who has the tab's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Foreground {
    /// The tab's child is a shell, and its own group is in front: it has
    /// taken the terminal back from whatever it was running.
    Shell,
    /// This process group is in front. A tab whose child is not a shell
    /// (a command tab) reads its child's own group as this.
    Group(i64),
    /// Not known: no PTY, or the read failed.
    Unknown,
}

/// The state a stamp covers. The owner is the hook owner; the records
/// are the tab's live OSC 7501 records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Owner,
    Records,
}

/// What an `A`, `B` or `D` mark does to a layer, beyond the mark's own
/// effect ([`agent::apply_shell_mark`], and the records' prompt rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkVerdict {
    /// The mark's own effect, and nothing else.
    Today,
    /// The mark's own effect, then the owner is released: the shell took
    /// the terminal back, and no stamp says the owner lives elsewhere.
    Release,
    /// What the mark drops is kept aside under the stamp, and a sample
    /// decides whether it comes back.
    Park,
}

/// What a sample of a stamp's process does to its layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleVerdict {
    Keep,
    /// The process runs: what a mark parked comes back, and a suspended
    /// layer is no longer suspended.
    Restore,
    /// The process is stopped: the layer shows suspended, and live state
    /// not yet parked is parked.
    Suspend,
    /// The process is gone and its group is not in front: the layer ends.
    End,
}

/// [`MarkVerdict`] for `layer`, stamped with `stamp` or not, at a mark
/// read with `fg` in front.
///
/// A mark from the group the stamp belongs to is that program's own (DL-29
/// rule c), so it does what it always did. A mark with another group in
/// front parks rather than drops: a stopped or backgrounded agent is still
/// alive. Only an owner with no stamp is released, and only when the shell
/// itself is in front; with anything else in front, or nothing known, a
/// mark does what it always did.
pub fn at_mark(layer: Layer, stamp: Option<&Stamp>, fg: Foreground) -> MarkVerdict {
    match (stamp, fg) {
        (None, Foreground::Shell) if layer == Layer::Owner => MarkVerdict::Release,
        (None, _) | (Some(_), Foreground::Unknown) => MarkVerdict::Today,
        (Some(stamp), Foreground::Group(pgid)) if pgid == stamp.pgid => MarkVerdict::Today,
        (Some(_), _) => MarkVerdict::Park,
    }
}

/// [`SampleVerdict`] for a layer stamped with `stamp`, whose process read
/// as `liveness` with `fg` in front.
///
/// Only a gone process whose group is no longer in front ends anything:
/// a pipeline's other members, or stragglers in the group, keep it while
/// the group has the terminal, and the shell takes the terminal back when
/// its own children exit, whoever is left. Every uncertain reading keeps.
/// An unstamped layer is never ended by a sample.
pub fn on_sample(stamp: Option<&Stamp>, liveness: Liveness, fg: Foreground) -> SampleVerdict {
    let Some(stamp) = stamp else {
        return SampleVerdict::Keep;
    };
    let in_front = match fg {
        Foreground::Group(pgid) => Some(pgid == stamp.pgid),
        Foreground::Shell => Some(false),
        Foreground::Unknown => None,
    };
    match (liveness, in_front) {
        (Liveness::Stopped, _) => SampleVerdict::Suspend,
        (Liveness::Running, Some(_)) => SampleVerdict::Restore,
        (Liveness::Gone, Some(false)) => SampleVerdict::End,
        _ => SampleVerdict::Keep,
    }
}

/// Whether the hook owner has a process for the layer to follow: an
/// agent's claim. `manual` and `legacy` are someone taking the wheel, and
/// keep what a mark always did to them.
pub fn owner_follows_a_process(state: &AgentTabState) -> bool {
    state
        .ownership
        .as_ref()
        .is_some_and(|owner| agent::is_agent_source(&owner.source))
}

/// `state` with its owner released, as a matching `release` report leaves
/// it: no owner, lifecycle `inactive`, the shell axis untouched.
pub fn released(state: &AgentTabState) -> AgentTabState {
    AgentTabState {
        shell: state.shell,
        lifecycle: AgentLifecycle::Inactive,
        ownership: None,
    }
}

/// Whether a group led by a process named `comm` is never stamped: a
/// multiplexer or a remote session. The agent inside it is not the
/// process in front, and the process in front outlives it.
///
/// The name's first word counts: on Linux tmux renames its client
/// `tmux: client`.
pub fn never_stamped(comm: &str) -> bool {
    let name = comm
        .split(|c: char| c == ':' || c.is_whitespace())
        .next()
        .unwrap_or_default();
    matches!(
        name,
        "tmux" | "screen" | "zellij" | "ssh" | "mosh" | "mosh-client"
    )
}

/// `Tab.process`: the stamped process behind a tab's agent state.
///
/// Present while a layer holds a stamp. `state` is an open value: a client
/// that cannot read it reads the whole object as absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabProcess {
    pub state: ProcessState,
    /// The agent the stamped layer belongs to: the hook owner's `source`.
    /// Empty when no name is known.
    #[serde(default)]
    pub agent: String,
    #[serde(with = "crate::messages::string_int64")]
    #[schemars(with = "String")]
    pub pid: i64,
    /// Unix seconds: when the process was stamped, or last changed
    /// between running and suspended.
    #[serde(default)]
    pub since: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    /// Stopped by job control. The tab shows what a prompt mark left;
    /// what the agent was doing comes back when it runs again.
    Suspended,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Ownership, ShellState};

    fn owned_by(source: &str) -> AgentTabState {
        AgentTabState {
            shell: ShellState::ForegroundProcess,
            lifecycle: AgentLifecycle::Working,
            ownership: Some(Ownership {
                source: source.into(),
                ..Ownership::default()
            }),
        }
    }

    #[test]
    fn only_an_agents_claim_follows_a_process() {
        assert!(owner_follows_a_process(&owned_by("claude")));
        assert!(owner_follows_a_process(&owned_by("aider")));
        for source in ["manual", "legacy"] {
            assert!(!owner_follows_a_process(&owned_by(source)), "{source}");
        }
        assert!(!owner_follows_a_process(&AgentTabState::default()));
    }

    #[test]
    fn released_drops_the_owner_and_the_lifecycle_and_keeps_the_shell() {
        let released = released(&owned_by("claude"));
        assert_eq!(released.ownership, None);
        assert_eq!(released.lifecycle, AgentLifecycle::Inactive);
        assert_eq!(released.shell, ShellState::ForegroundProcess);
    }

    #[test]
    fn multiplexers_and_remote_sessions_are_never_stamped() {
        for comm in [
            "tmux",
            "tmux: client",
            "tmux: server",
            "screen",
            "zellij",
            "ssh",
            "mosh",
            "mosh-client",
        ] {
            assert!(never_stamped(comm), "{comm}");
        }
        for comm in [
            "claude", "codex", "node", "sleep", "bash", "sshd", "tmux2", "",
        ] {
            assert!(!never_stamped(comm), "{comm}");
        }
    }

    #[test]
    fn an_unknown_process_state_reads_as_an_error() {
        let value = serde_json::json!({
            "state": "paused", "agent": "claude", "pid": "7", "since": 1
        });
        assert!(serde_json::from_value::<TabProcess>(value).is_err());
    }
}
