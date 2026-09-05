//! Codex hook adapter — plan 046 §3.1.
//!
//! # Verified hook contract
//!
//! Probed against Codex `0.153.2` on 2026-09-04 (plan 046's probe, one
//! session, 8 records). Codex's hook contract is **internal, not public
//! API** — there is no published schema, so this is a snapshot of one
//! build's observed behaviour, not a stable interface; re-probe a live
//! `codex` build before trusting this against a materially newer one.
//!
//! ```text
//! SessionStart       source, model, permission_mode
//! UserPromptSubmit   prompt, turn_id
//! PreToolUse         tool_name, tool_input, tool_use_id, turn_id     <- not observed live
//! PermissionRequest  tool_name, tool_input, turn_id                 <- not observed live
//! PostToolUse        tool_name, tool_input, tool_response, turn_id
//! Stop               stop_hook_active, last_assistant_message, turn_id
//! Interrupt          turn_id                                        <- the Esc signal
//! SessionEnd         reason
//! ```
//!
//! Every field name is `snake_case`; there is no camelCase alias
//! anywhere in the payload (verified across all 8 records) — the same
//! shape family as Claude's own native hooks, plus the `turn_id` field
//! Claude does not have.
//!
//! The probe's live session never triggered an approval dialog
//! (Charlie's `codex` config runs with approvals off), so neither
//! `PreToolUse` nor `PermissionRequest` was observed; both are mapped
//! from the plan's table below and pinned by synthetic payloads in the
//! test suite, not by fixture evidence.
//!
//! **Codex has no failure hook.** A turn that errors out still only
//! fires `Stop` — a failed turn is indistinguishable from a successful
//! one on this wire, and reads as `finished`. This is a known gap, not
//! something this adapter can fix from its side.
//!
//! **The codex↔Claude pair is not fully discriminated.** `turn_id`
//! separates every event the two share except `SessionStart` and
//! `SessionEnd`, which look identical in both agents' native shape
//! (same field names, same values, no distinguishing key). Nothing
//! installs codex's command into Claude's `settings.json` or Claude's
//! into codex's `hooks.json` (plan §3.1's isolation table only names
//! grok and cursor as agents that execute *Claude's* hook format), so
//! the two never actually run each other's payloads through this
//! function in production — but a fixture replayed through the wrong
//! adapter can still claim a tab. That gap is documented and tested
//! honestly (`fixture_replay_test.rs`) rather than papered over with a
//! discriminator that cannot actually distinguish the one event that
//! matters (`SessionStart`).

use roost_ipc::agent::{
    AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::Value;

use crate::common::{field, non_empty, parse_normalized};

pub const SOURCE: &str = "codex";

/// Listed in the order codex's own `hooks.json` registers them (plan
/// 046's probe config).
pub const CODEX_HOOK_EVENTS: [&str; 8] = [
    EventKind::SessionStart.canonical(),
    EventKind::UserPromptSubmit.canonical(),
    EventKind::PreToolUse.canonical(),
    EventKind::PermissionRequest.canonical(),
    EventKind::PostToolUse.canonical(),
    EventKind::Stop.canonical(),
    EventKind::Interrupt.canonical(),
    EventKind::SessionEnd.canonical(),
];

const TITLE: &str = "Codex";

/// Map one codex hook event to the reports it implies.
pub fn codex_event_to_reports(
    event: &str,
    payload: &Value,
    tab_id: i64,
) -> Vec<TabAgentReportParams> {
    // grok/gx always carry the camelCase `hookEventName` twin; cursor
    // always carries `conversation_id`/`cursor_version`. Codex's own
    // payload has none of the three, and neither does Claude's — this
    // rejects the first two agents outright and leaves the codex↔Claude
    // gap the module doc names, rather than pretending a discriminator
    // closes it.
    for foreign in ["hookEventName", "conversation_id", "cursor_version"] {
        if payload.get(foreign).is_some() {
            return Vec::new();
        }
    }

    if !payload.is_object() {
        return Vec::new();
    }

    let Some(kind) = EventKind::parse(event) else {
        return Vec::new();
    };

    let session_id = field(payload, "session_id");
    if matches!(kind, EventKind::SessionStart) && session_id.is_empty() {
        return Vec::new();
    }

    let base = TabAgentReportParams {
        session_id: session_id.to_string(),
        ..TabAgentReportParams::sessionless(tab_id, SOURCE, OwnershipAction::Preserve, None)
    };

    let report = match kind {
        EventKind::SessionStart => session_start(base, payload),
        EventKind::UserPromptSubmit => user_prompt_submit(base),
        EventKind::PreToolUse => tool_progress(base, "pre_tool_use"),
        EventKind::PostToolUse => tool_progress(base, "post_tool_use"),
        EventKind::PermissionRequest => permission_request(base, payload),
        EventKind::Stop => stop(base),
        EventKind::Interrupt => interrupt(base),
        EventKind::SessionEnd => session_end(base),
    };

    vec![report]
}

fn session_start(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    let source = non_empty(field(payload, "source")).unwrap_or("session_start");
    report.ownership_action = OwnershipAction::Claim;
    report.lifecycle = Some(AgentLifecycle::Inactive);
    report.detail = source.to_string();
    for key in ["model", "permission_mode"] {
        if let Some(value) = non_empty(field(payload, key)) {
            report.metadata.insert(key.to_string(), value.to_string());
        }
    }
    report
}

fn user_prompt_submit(mut report: TabAgentReportParams) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Working);
    report.attention = AttentionOp::Clear;
    report.detail = "user_prompt_submit".to_string();
    report
}

/// `PreToolUse`/`PostToolUse` — the turn is running. Not gated on the
/// tool succeeding: codex has no failure hook, so a tool result is
/// always read as ordinary progress.
fn tool_progress(mut report: TabAgentReportParams, detail: &str) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Working);
    report.detail = detail.to_string();
    report
}

/// Not observed live (Charlie's `codex` runs with approvals off) —
/// mapped from the plan's table and pinned by a synthetic payload.
fn permission_request(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Waiting);
    report.attention = AttentionOp::Set;
    report.severity = Severity::Warn;
    report.title = TITLE.to_string();
    report.body = match non_empty(field(payload, "tool_name")) {
        Some(tool) => format!("Needs permission to use `{tool}`"),
        None => "Needs permission to use a tool".to_string(),
    };
    report.detail = "permission_request".to_string();
    report
}

/// Codex has no failure hook and no background-task concept on `Stop` —
/// every turn that reaches `Stop` reads as finished, a failed one
/// included (module doc).
fn stop(mut report: TabAgentReportParams) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Finished);
    report.attention = AttentionOp::Set;
    report.severity = Severity::Info;
    report.title = TITLE.to_string();
    report.body = "Turn complete".to_string();
    report.detail = "stop".to_string();
    report
}

/// The Esc-interrupt signal. Ownership continues — the session is still
/// live — so this only ends the turn and clears attention, same as
/// grok's `StopCancelled`.
fn interrupt(mut report: TabAgentReportParams) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Finished);
    report.attention = AttentionOp::Clear;
    report.detail = "interrupt".to_string();
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
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    Stop,
    Interrupt,
    SessionEnd,
}

impl EventKind {
    fn parse(event: &str) -> Option<EventKind> {
        parse_normalized(
            event,
            &[
                ("sessionstart", EventKind::SessionStart),
                ("userpromptsubmit", EventKind::UserPromptSubmit),
                ("pretooluse", EventKind::PreToolUse),
                ("permissionrequest", EventKind::PermissionRequest),
                ("posttooluse", EventKind::PostToolUse),
                ("stop", EventKind::Stop),
                ("interrupt", EventKind::Interrupt),
                ("sessionend", EventKind::SessionEnd),
            ],
        )
    }

    const fn canonical(self) -> &'static str {
        match self {
            EventKind::SessionStart => "SessionStart",
            EventKind::UserPromptSubmit => "UserPromptSubmit",
            EventKind::PreToolUse => "PreToolUse",
            EventKind::PermissionRequest => "PermissionRequest",
            EventKind::PostToolUse => "PostToolUse",
            EventKind::Stop => "Stop",
            EventKind::Interrupt => "Interrupt",
            EventKind::SessionEnd => "SessionEnd",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same discipline as Claude's and grok's twin test: a variant added
    /// to `EventKind` without a matching entry in `CODEX_HOOK_EVENTS`
    /// would be understood by the adapter and never installed into any
    /// codex config.
    #[test]
    fn every_event_kind_is_an_installed_hook_event() {
        let all = [
            EventKind::SessionStart,
            EventKind::UserPromptSubmit,
            EventKind::PreToolUse,
            EventKind::PermissionRequest,
            EventKind::PostToolUse,
            EventKind::Stop,
            EventKind::Interrupt,
            EventKind::SessionEnd,
        ];
        for kind in all {
            match kind {
                EventKind::SessionStart
                | EventKind::UserPromptSubmit
                | EventKind::PreToolUse
                | EventKind::PermissionRequest
                | EventKind::PostToolUse
                | EventKind::Stop
                | EventKind::Interrupt
                | EventKind::SessionEnd => {}
            }
            assert!(
                CODEX_HOOK_EVENTS.contains(&kind.canonical()),
                "{} is mapped but never installed",
                kind.canonical()
            );
        }
        assert_eq!(all.len(), CODEX_HOOK_EVENTS.len());
    }
}
