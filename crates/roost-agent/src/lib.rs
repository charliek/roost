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
pub mod codex;
pub mod cursor;
pub mod grok;
pub mod opencode;

pub use claude::{canonical_hook_event, claude_event_to_reports, CLAUDE_HOOK_EVENTS};
pub use codex::{codex_event_to_reports, CODEX_HOOK_EVENTS};
pub use cursor::{cursor_event_to_reports, CURSOR_HOOK_EVENTS};
pub use grok::{grok_event_to_reports, GROK_HOOK_EVENTS};
pub use opencode::{opencode_event_to_reports, OPENCODE_HOOK_EVENTS};

use roost_ipc::agent::TabAgentReportParams;
use serde_json::Value;

/// The agents Roost has an adapter for.
///
/// A variant lands with its module, so this is a truthful inventory
/// rather than a roadmap: [`Agent::parse`] answers `None` for an agent
/// nothing here can map, and the caller takes its "no adapter" path —
/// drain stdin, answer `{}`, exit 0 — instead of hitting a silent no-op
/// arm that looks like a working install.
///
/// gx shares grok's `$GROK_HOME` and reports under the same `source`,
/// so it has no variant of its own — `Agent::Grok` covers both binaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Grok,
    Codex,
    Cursor,
    Opencode,
}

impl Agent {
    /// Resolve the name `roostctl agent-hook <agent>` was given. Case
    /// and separators are normalized away, matching how every adapter
    /// reads its own event names.
    pub fn parse(name: &str) -> Option<Agent> {
        common::parse_normalized(
            name,
            &[
                (claude::SOURCE, Agent::Claude),
                (grok::SOURCE, Agent::Grok),
                (codex::SOURCE, Agent::Codex),
                (cursor::SOURCE, Agent::Cursor),
                (opencode::SOURCE, Agent::Opencode),
            ],
        )
    }

    /// The ownership `source` every report from this agent carries. It
    /// is also the name [`Agent::parse`] accepts.
    pub fn source(self) -> &'static str {
        match self {
            Agent::Claude => claude::SOURCE,
            Agent::Grok => grok::SOURCE,
            Agent::Codex => codex::SOURCE,
            Agent::Cursor => cursor::SOURCE,
            Agent::Opencode => opencode::SOURCE,
        }
    }

    /// The canonical hook-event names to register for this agent.
    pub fn events(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &CLAUDE_HOOK_EVENTS,
            Agent::Grok => &GROK_HOOK_EVENTS,
            Agent::Codex => &CODEX_HOOK_EVENTS,
            Agent::Cursor => &CURSOR_HOOK_EVENTS,
            Agent::Opencode => &OPENCODE_HOOK_EVENTS,
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
            Agent::Grok => grok::grok_event_to_reports(event, payload, tab_id),
            Agent::Codex => codex::codex_event_to_reports(event, payload, tab_id),
            Agent::Cursor => cursor::cursor_event_to_reports(event, payload, tab_id),
            Agent::Opencode => opencode::opencode_event_to_reports(event, payload, tab_id),
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
        for spelling in ["grok", "Grok", "GROK"] {
            assert_eq!(Agent::parse(spelling), Some(Agent::Grok), "{spelling}");
        }
        for spelling in ["codex", "Codex", "CODEX"] {
            assert_eq!(Agent::parse(spelling), Some(Agent::Codex), "{spelling}");
        }
        for spelling in ["cursor", "Cursor", "CURSOR"] {
            assert_eq!(Agent::parse(spelling), Some(Agent::Cursor), "{spelling}");
        }
        for spelling in ["opencode", "OpenCode", "OPENCODE"] {
            assert_eq!(Agent::parse(spelling), Some(Agent::Opencode), "{spelling}");
        }
    }

    /// `gx` has no `Agent` variant of its own: it shares grok's config
    /// and reports as `grok`, so `roostctl agent-hook gx` is never a
    /// thing — only `agent-hook grok` is installed for either binary.
    #[test]
    fn gx_has_no_variant_of_its_own() {
        assert_eq!(Agent::parse("gx"), None);
    }

    /// An agent whose adapter has not been written must not resolve:
    /// `roostctl` has a defined "no adapter" path and it only runs when
    /// `parse` says so.
    #[test]
    fn parse_rejects_everything_without_a_module() {
        for name in ["", "gx", "amp", "gemini", "claude-code", "🙂"] {
            assert_eq!(Agent::parse(name), None, "{name}");
        }
    }

    /// One arm of the dispatch table, checked against the module it
    /// delegates to. `event` is that agent's own claiming event and
    /// `payload` whatever its gate requires of one — opencode's
    /// vocabulary is its own, not a variant of Claude's.
    fn dispatch_matches(
        source: &str,
        events: &[&str],
        adapter: fn(&str, &Value, i64) -> Vec<TabAgentReportParams>,
        event: &str,
        payload: &Value,
    ) {
        let agent = Agent::parse(source).unwrap();
        assert_eq!(agent.source(), source);
        assert_eq!(agent.events(), events);

        // Non-emptiness first: two empty vectors compare equal, so this
        // would pass if dispatch and the module regressed together.
        let via_dispatch = agent.event_to_reports(event, payload, 7);
        assert!(!via_dispatch.is_empty(), "{source}");
        assert_eq!(via_dispatch, adapter(event, payload, 7), "{source}");
    }

    /// Every arm of the table, so an agent that lands later joins by
    /// adding a line rather than by copying a test.
    #[test]
    fn dispatch_reaches_the_same_answer_as_each_module() {
        dispatch_matches(
            claude::SOURCE,
            &CLAUDE_HOOK_EVENTS,
            claude_event_to_reports,
            "SessionStart",
            &json!({ "session_id": "s-1", "source": "startup" }),
        );
        dispatch_matches(
            grok::SOURCE,
            &GROK_HOOK_EVENTS,
            grok_event_to_reports,
            "SessionStart",
            // grok's gate: the camelCase twin has to be present.
            &json!({ "session_id": "s-1", "source": "new", "hookEventName": "session_start" }),
        );
        dispatch_matches(
            codex::SOURCE,
            &CODEX_HOOK_EVENTS,
            codex_event_to_reports,
            "SessionStart",
            &json!({ "session_id": "s-1", "source": "startup" }),
        );
        dispatch_matches(
            cursor::SOURCE,
            &CURSOR_HOOK_EVENTS,
            cursor_event_to_reports,
            "sessionStart",
            // cursor's gate: its own version stamp has to be present.
            &json!({ "session_id": "s-1", "cursor_version": "2026.09.02-c22c1a3" }),
        );
        dispatch_matches(
            opencode::SOURCE,
            &OPENCODE_HOOK_EVENTS,
            opencode_event_to_reports,
            "session.created",
            &json!({ "sessionID": "ses_1" }),
        );
    }
}
