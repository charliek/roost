//! Agent adapters — the seam between a coding agent's own event
//! vocabulary and Roost's one op set.
//!
//! An adapter is a **pure function**: hook event JSON in, a list of
//! [`roost_ipc::agent::TabAgentReportParams`] out. No I/O, no socket, no
//! clap, no tokio. `roostctl` owns dialing the UI; this crate owns the
//! policy, so the mapping is unit-testable without a running Roost and a
//! second agent costs an afternoon (plan 002 §3.10).
//!
//! Adapters deliberately do **not** know which session currently owns a
//! tab. Session scoping is enforced downstream by `Workspace` via
//! [`roost_ipc::agent::apply_report`], which matches the report's
//! `(source, session_id)` pair against the live owner. A pure adapter
//! cannot know the current owner, and pretending otherwise is what
//! forced the explicit patch semantics on the op (plan §3.3, §3.6).
//!
//! One module per agent, one shape: `SOURCE`, the event list, and an
//! `<agent>_event_to_reports` function. [`Agent`] is the dispatch table
//! `roostctl agent-hook <agent>` reaches them through (plan 046 §3.1).

#![deny(unsafe_op_in_unsafe_fn)]

mod common;

pub mod claude;

pub use claude::{canonical_hook_event, claude_event_to_reports, CLAUDE_HOOK_EVENTS};

use roost_ipc::agent::TabAgentReportParams;
use serde_json::Value;

/// The agents Roost has an adapter for.
///
/// A variant lands with its module, so this is a truthful inventory
/// rather than a roadmap: [`Agent::parse`] answers `None` for an agent
/// nothing here can map, and the caller takes its "no adapter" path —
/// drain stdin, answer `{}`, exit 0 — instead of hitting a silent no-op
/// arm that looks like a working install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
}

impl Agent {
    /// Resolve the name `roostctl agent-hook <agent>` was given. Case
    /// and separators are normalized away, matching how every adapter
    /// reads its own event names.
    pub fn parse(name: &str) -> Option<Agent> {
        common::parse_normalized(name, &[(claude::SOURCE, Agent::Claude)])
    }

    /// The ownership `source` every report from this agent carries. It
    /// is also the name [`Agent::parse`] accepts.
    pub fn source(self) -> &'static str {
        match self {
            Agent::Claude => claude::SOURCE,
        }
    }

    /// The canonical hook-event names to register for this agent.
    pub fn events(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &CLAUDE_HOOK_EVENTS,
        }
    }

    /// Map one hook event to the reports it implies. Empty for an event
    /// this adapter does not map, including a malformed payload —
    /// nothing here fails, because a hook that cannot be interpreted
    /// must not break the turn it fired from.
    pub fn event_to_reports(
        self,
        event: &str,
        payload: &Value,
        tab_id: i64,
    ) -> Vec<TabAgentReportParams> {
        match self {
            Agent::Claude => claude::claude_event_to_reports(event, payload, tab_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_accepts_the_spellings_a_cli_actually_receives() {
        for spelling in ["claude", "Claude", "CLAUDE"] {
            assert_eq!(Agent::parse(spelling), Some(Agent::Claude), "{spelling}");
        }
    }

    /// An agent whose adapter has not been written must not resolve:
    /// `roostctl` has a defined "no adapter" path and it only runs when
    /// `parse` says so.
    #[test]
    fn parse_rejects_everything_without_a_module() {
        for name in [
            "",
            "codex",
            "grok",
            "cursor",
            "opencode",
            "claude-code",
            "🙂",
        ] {
            assert_eq!(Agent::parse(name), None, "{name}");
        }
    }

    #[test]
    fn dispatch_reaches_the_same_answer_as_the_module() {
        let payload = json!({ "session_id": "s-1", "source": "startup" });
        let agent = Agent::parse("claude").unwrap();
        assert_eq!(agent.source(), claude::SOURCE);
        assert_eq!(agent.events(), &CLAUDE_HOOK_EVENTS);
        assert_eq!(
            agent.event_to_reports("SessionStart", &payload, 7),
            claude_event_to_reports("SessionStart", &payload, 7),
        );
        assert!(!agent
            .event_to_reports("SessionStart", &payload, 7)
            .is_empty());
    }
}
