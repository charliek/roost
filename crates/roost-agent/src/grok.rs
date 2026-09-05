//! grok / gx hook adapter — plan 046 §3.1.
//!
//! grok (xAI's CLI) and gx (a fork of the same product) share one
//! `$GROK_HOME` and are installed with the same hook file, so both
//! report through this module with `source = "grok"`; there is no
//! separate `gx` adapter or ownership source.
//!
//! # Verified hook contract
//!
//! Captured live against a real grok session on 2026-09-04 (plan 046's
//! probe; four sessions back-to-back, 44 records). The payload borrows
//! Claude's envelope but duplicates most fields under both casings:
//!
//! ```text
//! SessionStart       source  (camel+snake: hookEventName/hook_event_name,
//!                     sessionId/session_id, permissionMode/permission_mode)
//! UserPromptSubmit   prompt, promptId (camelCase only)
//! PreToolUse         toolName/tool_name, toolInput/tool_input, toolUseId/tool_use_id
//! PostToolUse        + isBackgrounded (camelCase only), toolResult (camelCase only,
//!                     carries the same value as the snake-only tool_response —
//!                     a different WORD, not a case transform: do not derive
//!                     one spelling from the other)
//! Stop               reason, stopHookActive (camelCase only),
//!                     backgroundTasks / sessionCrons (camelCase only, no
//!                     snake sibling at all)
//! StopCancelled      reason: "user_interrupt", cancelTrigger: "esc",
//!                     cancelledBy: "user" (all camelCase only) — the
//!                     Esc-interrupt signal; grok has no interrupt hook
//!                     as such, this *is* it
//! Notification       message, notificationType (camelCase only — no
//!                     snake_case `notification_type` exists at all)
//! SessionEnd         reason: "shutdown"
//! ```
//!
//! grok has no `PermissionRequest` hook. Its only blocked signal is
//! `Notification` with `notificationType: permission_prompt` — observed
//! live in the probe (session 3, plan mode) with `message: "Plan
//! approval requested"`.
//!
//! Every session in the probe ends with `SessionEnd` immediately
//! followed by a trailing `Stop{reason: shutdown}`. Ownership is already
//! released by the time that `Stop` arrives, so the server drops it on
//! the ownership mismatch (`apply_report`'s `owner_matches`) — asserted
//! in the fixture replay rather than special-cased here.
//!
//! `PermissionDenied`, `PostToolUseFailure` and `StopFailure` are in
//! grok's registered event list but were not observed in the probe
//! (auto-approve was on, nothing failed); they are mapped from the
//! table below and pinned by synthetic payloads, not fixture evidence.

use roost_ipc::agent::{
    AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::Value;

use crate::common::{array_len, field, field_alias, has_field, non_empty, parse_normalized};

pub const SOURCE: &str = "grok";

/// Listed in probe order. No `PermissionRequest` — grok does not have
/// one (see the module doc).
pub const GROK_HOOK_EVENTS: [&str; 11] = [
    EventKind::SessionStart.canonical(),
    EventKind::UserPromptSubmit.canonical(),
    EventKind::PreToolUse.canonical(),
    EventKind::PostToolUse.canonical(),
    EventKind::PostToolUseFailure.canonical(),
    EventKind::PermissionDenied.canonical(),
    EventKind::Stop.canonical(),
    EventKind::StopFailure.canonical(),
    EventKind::StopCancelled.canonical(),
    EventKind::Notification.canonical(),
    EventKind::SessionEnd.canonical(),
];

const TITLE: &str = "Grok";

/// Map one grok/gx hook event to the reports it implies. See
/// [`crate::claude::claude_event_to_reports`] for the shared discipline
/// this mirrors (pure, total, malformed input costs nothing).
pub fn grok_event_to_reports(
    event: &str,
    payload: &Value,
    tab_id: i64,
) -> Vec<TabAgentReportParams> {
    // grok/gx always duplicates `hook_event_name` under the camelCase
    // `hookEventName` twin (every one of the 44 probe records carries
    // both); Claude's and codex's own payloads are snake_case only. That
    // makes this key's presence the positive discriminator that keeps a
    // Claude or codex payload from claiming a tab through this adapter —
    // and, as a side effect, it also degrades a non-object payload
    // safely: `Value::get` on anything but an object/array returns
    // `None`, so this check alone rejects those too.
    if !has_field(payload, "hookEventName") {
        return Vec::new();
    }

    let Some(kind) = EventKind::parse(event) else {
        return Vec::new();
    };

    let session_id = field_alias(payload, "session_id", "sessionId");
    if matches!(kind, EventKind::SessionStart) && session_id.is_empty() {
        // Same reasoning as Claude's adapter: a claim supersedes any live
        // owner unconditionally, so a SessionStart missing its session id
        // must be dropped rather than installing an owner nothing can
        // release.
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
        EventKind::PostToolUseFailure => tool_progress(base, "post_tool_use_failure"),
        EventKind::PermissionDenied => tool_progress(base, "permission_denied"),
        EventKind::StopCancelled => stop_cancelled(base),
        EventKind::Notification => notification(base, payload),
        EventKind::Stop => stop(base, payload),
        EventKind::StopFailure => stop_failure(base, payload),
        EventKind::SessionEnd => session_end(base),
    };

    vec![report]
}

fn session_start(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    let source = non_empty(field(payload, "source")).unwrap_or("session_start");
    report.ownership_action = OwnershipAction::Claim;
    report.lifecycle = Some(AgentLifecycle::Inactive);
    report.detail = source.to_string();
    // grok's SessionStart carries no `model`/`session_title` in the
    // probe, but the accessor degrades to "absent" rather than assuming
    // so, in case a future grok build adds them under the same names.
    for key in ["model", "source", "session_title"] {
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

/// `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionDenied`
/// — the turn is running, exactly like Claude's four tool events.
fn tool_progress(mut report: TabAgentReportParams, detail: &str) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Working);
    report.detail = detail.to_string();
    report
}

/// The Esc-interrupt signal. grok has no dedicated interrupt hook; this
/// *is* it, and ownership continues (the session is still live, just
/// mid-cancel) — no banner, matching every other interrupt in this
/// plan (Claude's Esc has no hook at all; codex's `Interrupt` behaves
/// the same way).
fn stop_cancelled(mut report: TabAgentReportParams) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Finished);
    report.attention = AttentionOp::Clear;
    report.detail = "stop_cancelled".to_string();
    report
}

fn notification(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    // `notification_type` has no snake_case form for grok at all — the
    // fallback is not defensive hedging here, it is the only way this
    // field is ever populated.
    let kind = field_alias(payload, "notification_type", "notificationType");

    // Mirrors Claude's `permission_prompt`/`idle_prompt` guards
    // (§3.1): both are timer/state signals that can legally arrive
    // after the turn already moved on, so they're guarded on the
    // lifecycle they're allowed to override rather than applied
    // unconditionally. grok has no `agent_needs_input`/
    // `elicitation_dialog` analogue in its vocabulary.
    let (lifecycle, lifecycle_if, severity) = match kind {
        "permission_prompt" => (
            Some(AgentLifecycle::Waiting),
            Some(vec![AgentLifecycle::Working]),
            Severity::Warn,
        ),
        "idle_prompt" => (
            Some(AgentLifecycle::Finished),
            Some(vec![AgentLifecycle::Working]),
            Severity::Info,
        ),
        _ => (None, None, Severity::Info),
    };
    report.lifecycle = lifecycle;
    report.lifecycle_if = lifecycle_if;
    report.severity = severity;
    report.attention = AttentionOp::Set;
    report.title = non_empty(field(payload, "title"))
        .unwrap_or(TITLE)
        .to_string();
    report.body = non_empty(field(payload, "message"))
        .unwrap_or("Grok needs input")
        .to_string();
    report.detail = non_empty(kind).unwrap_or("notification").to_string();
    report
}

fn stop(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    // `backgroundTasks`/`sessionCrons` are camelCase-only for grok — no
    // snake_case sibling exists, so no fallback is needed here.
    let in_flight = array_len(payload, "backgroundTasks");
    let crons = array_len(payload, "sessionCrons");

    report.lifecycle = Some(if in_flight > 0 {
        AgentLifecycle::Working
    } else {
        AgentLifecycle::Finished
    });
    report.attention = AttentionOp::Set;
    report.severity = Severity::Info;
    report.title = TITLE.to_string();
    if in_flight > 0 {
        let plural = if in_flight == 1 { "" } else { "s" };
        report.body = format!("Waiting on {in_flight} background task{plural}");
        report.detail = format!("background_tasks:{in_flight}");
    } else {
        report.body = "Turn complete".to_string();
        report.detail = "stop".to_string();
    }
    report
        .metadata
        .insert("background_tasks".to_string(), in_flight.to_string());
    report
        .metadata
        .insert("session_crons".to_string(), crons.to_string());
    report
}

/// Not observed in the probe (nothing failed during the run) — mapped
/// from the plan's table and pinned by a synthetic payload.
fn stop_failure(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    let error = non_empty(field(payload, "error")).unwrap_or("unknown");
    report.lifecycle = Some(AgentLifecycle::Failed);
    report.attention = AttentionOp::Set;
    report.severity = Severity::Error;
    report.title = TITLE.to_string();
    report.body = non_empty(field(payload, "error_details"))
        .map(str::to_string)
        .unwrap_or_else(|| format!("Stopped: {error}"));
    report.detail = error.to_string();
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
    PostToolUse,
    PostToolUseFailure,
    PermissionDenied,
    Stop,
    StopFailure,
    StopCancelled,
    Notification,
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
                ("posttooluse", EventKind::PostToolUse),
                ("posttoolusefailure", EventKind::PostToolUseFailure),
                ("permissiondenied", EventKind::PermissionDenied),
                ("stopfailure", EventKind::StopFailure),
                ("stopcancelled", EventKind::StopCancelled),
                ("stop", EventKind::Stop),
                ("notification", EventKind::Notification),
                ("sessionend", EventKind::SessionEnd),
            ],
        )
    }

    const fn canonical(self) -> &'static str {
        match self {
            EventKind::SessionStart => "SessionStart",
            EventKind::UserPromptSubmit => "UserPromptSubmit",
            EventKind::PreToolUse => "PreToolUse",
            EventKind::PostToolUse => "PostToolUse",
            EventKind::PostToolUseFailure => "PostToolUseFailure",
            EventKind::PermissionDenied => "PermissionDenied",
            EventKind::Stop => "Stop",
            EventKind::StopFailure => "StopFailure",
            EventKind::StopCancelled => "StopCancelled",
            EventKind::Notification => "Notification",
            EventKind::SessionEnd => "SessionEnd",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same discipline as Claude's twin test: a variant added to
    /// `EventKind` without a matching entry in `GROK_HOOK_EVENTS` would
    /// be understood by the adapter and never installed into any grok
    /// config.
    #[test]
    fn every_event_kind_is_an_installed_hook_event() {
        let all = [
            EventKind::SessionStart,
            EventKind::UserPromptSubmit,
            EventKind::PreToolUse,
            EventKind::PostToolUse,
            EventKind::PostToolUseFailure,
            EventKind::PermissionDenied,
            EventKind::Stop,
            EventKind::StopFailure,
            EventKind::StopCancelled,
            EventKind::Notification,
            EventKind::SessionEnd,
        ];
        for kind in all {
            match kind {
                EventKind::SessionStart
                | EventKind::UserPromptSubmit
                | EventKind::PreToolUse
                | EventKind::PostToolUse
                | EventKind::PostToolUseFailure
                | EventKind::PermissionDenied
                | EventKind::Stop
                | EventKind::StopFailure
                | EventKind::StopCancelled
                | EventKind::Notification
                | EventKind::SessionEnd => {}
            }
            assert!(
                GROK_HOOK_EVENTS.contains(&kind.canonical()),
                "{} is mapped but never installed",
                kind.canonical()
            );
        }
        assert_eq!(all.len(), GROK_HOOK_EVENTS.len());
    }
}
