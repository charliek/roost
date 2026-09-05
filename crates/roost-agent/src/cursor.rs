//! Cursor hook adapter — plan 046 §3.1.
//!
//! # Verified hook contract
//!
//! Captured live against Cursor `2026.09.02-c22c1a3` on 2026-09-04
//! (plan 046's probe; one session, 16 usable records — one record was
//! lost to a torn write in the probe's own logger, not to anything
//! cursor did).
//!
//! Event names are cursor's own lowerCamelCase; every *field* name is
//! `snake_case`, with no camelCase alias anywhere in the payload.
//!
//! ```text
//! sessionStart          model, cursor_version, conversation_id, workspace_roots
//! beforeSubmitPrompt    prompt, attachments
//! preToolUse            tool_name, tool_input, tool_use_id, cwd
//! postToolUse           + tool_output, duration
//! postToolUseFailure    tool_name                              <- not observed live
//! afterAgentResponse    text, input_tokens/output_tokens/…
//! stop                  status: "completed" | "aborted" | "error", loop_count
//! sessionEnd            reason, final_status, duration_ms
//! ```
//!
//! The probe also recorded `afterAgentThought`,
//! `beforeShellExecution` and `afterShellExecution`. Roost registers
//! none of the three and this adapter maps none of them.
//!
//! # Three accepted gaps (plan §4), none of them fixable from here
//!
//! **`stop` repeats.** The probe's second turn fired `stop` twice in a
//! row (`aborted` then `error`); the plan records up to three per turn.
//! An unguarded report would banner each one, so `stop` carries
//! `lifecycle_if: [working, waiting]` — the first one ends the turn, the
//! repeats land vetoed and stay silent.
//!
//! **`status: error` is not a failure signal.** Pressing Esc produces
//! `stop{status: aborted}` immediately followed by `stop{status:
//! error}` (probe lines 14-15), so reading `error` as a failed turn
//! would paint every interrupt red. Every status therefore maps to
//! `finished`, with the raw status carried in `detail`. A genuine cursor
//! error consequently reads as "finished"; that is the accepted cost of
//! not misreporting interrupts, and there is no heuristic here that
//! could separate the two.
//!
//! **No blocked state.** Cursor has no permission hook. Its nearest
//! candidate, `beforeShellExecution`, fired ~0.1 s before
//! `afterShellExecution` in the probe because the command was
//! auto-approved — so it cannot distinguish "waiting for approval" from
//! "running", and mapping it to `waiting` would show a long command as
//! blocked. Cursor tabs simply never reach `waiting` from this adapter.
//!
//! # Foreign payloads
//!
//! The event vocabulary alone does **not** discriminate: cursor's
//! `sessionStart`, `preToolUse`, `postToolUse`, `stop` and `sessionEnd`
//! normalize onto exactly the names Claude, codex and grok use, so a
//! Claude `SessionStart` reaching this function would claim the tab.
//! Nor can the reject-list the Claude and codex adapters use be copied
//! here — the two keys on it, `conversation_id` and `cursor_version`,
//! are *cursor's own*.
//!
//! So the gate runs the same direction grok's does: `cursor_version` is
//! **required**, and required as a non-empty *string*. Every one of the
//! 16 probe records carries one (`"2026.09.02-c22c1a3"`), and no other
//! probed agent emits the key at all. Merely present-and-non-null is too
//! weak a test for a gate that stands between a foreign payload and an
//! unconditional `sessionStart` claim: `{"cursor_version": false}` is
//! not a cursor payload.

use roost_ipc::agent::{
    AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::Value;

use crate::common::{field, non_empty, parse_normalized};

pub const SOURCE: &str = "cursor";

/// Listed in the order a turn fires them, which is also the order the
/// probe recorded. Cursor's own spelling — these strings are what
/// `agent install cursor` writes into its hooks file.
pub const CURSOR_HOOK_EVENTS: [&str; 8] = [
    EventKind::SessionStart.canonical(),
    EventKind::BeforeSubmitPrompt.canonical(),
    EventKind::PreToolUse.canonical(),
    EventKind::PostToolUse.canonical(),
    EventKind::PostToolUseFailure.canonical(),
    EventKind::AfterAgentResponse.canonical(),
    EventKind::Stop.canonical(),
    EventKind::SessionEnd.canonical(),
];

const TITLE: &str = "Cursor";

/// Map one cursor hook event to the reports it implies. Same discipline
/// as every other adapter: pure, total, and a payload it cannot
/// interpret costs nothing.
pub fn cursor_event_to_reports(
    event: &str,
    payload: &Value,
    tab_id: i64,
) -> Vec<TabAgentReportParams> {
    // The positive gate the module doc explains. `field` answers `""`
    // for a missing key, a null, and anything that is not a string, and
    // `Value::get` answers `None` for a payload that is not an object,
    // so this one check covers all four without a second.
    if non_empty(field(payload, "cursor_version")).is_none() {
        return Vec::new();
    }

    let Some(kind) = EventKind::parse(event) else {
        return Vec::new();
    };

    let session_id = field(payload, "session_id");
    if matches!(kind, EventKind::SessionStart) && session_id.is_empty() {
        // Same reasoning as every other adapter: a claim supersedes any
        // live owner unconditionally, so a sessionStart that lost its id
        // would install an owner nothing can ever release.
        return Vec::new();
    }

    let base = TabAgentReportParams {
        session_id: session_id.to_string(),
        ..TabAgentReportParams::sessionless(tab_id, SOURCE, OwnershipAction::Preserve, None)
    };

    let report = match kind {
        EventKind::SessionStart => session_start(base, payload),
        EventKind::BeforeSubmitPrompt => before_submit_prompt(base),
        EventKind::PreToolUse => turn_progress(base, "pre_tool_use"),
        EventKind::PostToolUse => turn_progress(base, "post_tool_use"),
        EventKind::PostToolUseFailure => turn_progress(base, "post_tool_use_failure"),
        EventKind::AfterAgentResponse => turn_progress(base, "after_agent_response"),
        EventKind::Stop => stop(base, payload),
        EventKind::SessionEnd => session_end(base),
    };

    vec![report]
}

fn session_start(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    report.ownership_action = OwnershipAction::Claim;
    report.lifecycle = Some(AgentLifecycle::Inactive);
    report.detail = "session_start".to_string();
    for key in ["model", "cursor_version"] {
        if let Some(value) = non_empty(field(payload, key)) {
            report.metadata.insert(key.to_string(), value.to_string());
        }
    }
    report
}

fn before_submit_prompt(mut report: TabAgentReportParams) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Working);
    report.attention = AttentionOp::Clear;
    report.detail = "before_submit_prompt".to_string();
    report
}

/// The four mid-turn events. `afterAgentResponse` joins the tool events
/// here because it precedes `stop` rather than ending the turn: the
/// model has answered, but cursor may still loop.
fn turn_progress(mut report: TabAgentReportParams, detail: &str) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Working);
    report.detail = detail.to_string();
    report
}

/// Guarded, and status-blind — see the module doc for both reasons.
fn stop(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Finished);
    report.lifecycle_if = Some(vec![AgentLifecycle::Working, AgentLifecycle::Waiting]);
    report.attention = AttentionOp::Set;
    report.severity = Severity::Info;
    report.title = TITLE.to_string();
    report.body = "Turn complete".to_string();
    report.detail = non_empty(field(payload, "status"))
        .unwrap_or("stop")
        .to_string();
    report
}

fn session_end(mut report: TabAgentReportParams) -> TabAgentReportParams {
    report.ownership_action = OwnershipAction::Release;
    report.lifecycle = Some(AgentLifecycle::Inactive);
    report.attention = AttentionOp::Clear;
    report
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    SessionStart,
    BeforeSubmitPrompt,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    AfterAgentResponse,
    Stop,
    SessionEnd,
}

impl EventKind {
    fn parse(event: &str) -> Option<EventKind> {
        parse_normalized(
            event,
            &[
                ("sessionstart", EventKind::SessionStart),
                ("beforesubmitprompt", EventKind::BeforeSubmitPrompt),
                ("pretooluse", EventKind::PreToolUse),
                ("posttoolusefailure", EventKind::PostToolUseFailure),
                ("posttooluse", EventKind::PostToolUse),
                ("afteragentresponse", EventKind::AfterAgentResponse),
                ("stop", EventKind::Stop),
                ("sessionend", EventKind::SessionEnd),
            ],
        )
    }

    const fn canonical(self) -> &'static str {
        match self {
            EventKind::SessionStart => "sessionStart",
            EventKind::BeforeSubmitPrompt => "beforeSubmitPrompt",
            EventKind::PreToolUse => "preToolUse",
            EventKind::PostToolUse => "postToolUse",
            EventKind::PostToolUseFailure => "postToolUseFailure",
            EventKind::AfterAgentResponse => "afterAgentResponse",
            EventKind::Stop => "stop",
            EventKind::SessionEnd => "sessionEnd",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same discipline as Claude's, grok's and codex's twin tests: a
    /// variant added to `EventKind` without a matching entry in
    /// `CURSOR_HOOK_EVENTS` would be understood by the adapter and never
    /// installed into any cursor config.
    #[test]
    fn every_event_kind_is_an_installed_hook_event() {
        let all = [
            EventKind::SessionStart,
            EventKind::BeforeSubmitPrompt,
            EventKind::PreToolUse,
            EventKind::PostToolUse,
            EventKind::PostToolUseFailure,
            EventKind::AfterAgentResponse,
            EventKind::Stop,
            EventKind::SessionEnd,
        ];
        for kind in all {
            match kind {
                EventKind::SessionStart
                | EventKind::BeforeSubmitPrompt
                | EventKind::PreToolUse
                | EventKind::PostToolUse
                | EventKind::PostToolUseFailure
                | EventKind::AfterAgentResponse
                | EventKind::Stop
                | EventKind::SessionEnd => {}
            }
            assert!(
                CURSOR_HOOK_EVENTS.contains(&kind.canonical()),
                "{} is mapped but never installed",
                kind.canonical()
            );
        }
        assert_eq!(all.len(), CURSOR_HOOK_EVENTS.len());
    }
}
