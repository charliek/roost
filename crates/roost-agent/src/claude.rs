//! Claude Code hook adapter — plan 002 §3.8.
//!
//! # Verified hook contract
//!
//! **Verified against Claude Code `2.1.220`** (re-verified 2026-07-27 for
//! C4) by extracting the Zod schemas from the installed binary at
//! `~/.local/share/claude/versions/2.1.220`. The published docs were
//! wrong on three of these points, so the binary is the source of truth;
//! re-verify there, not in the docs, when this moves.
//!
//! Fields common to every event: `session_id`, `transcript_path`, `cwd`,
//! `prompt_id?`, `permission_mode?`, `agent_id?`.
//!
//! ```text
//! SessionStart       source: "startup"|"resume"|"clear"|"compact"|"fork"
//!                    agent_type?  model?  session_title?
//! UserPromptSubmit   prompt
//!                    source?: "user"|"sdk"|"system"|"loop_wakeup"|"schedule_wakeup"
//! PreToolUse         tool_name, tool_input, tool_use_id
//! PermissionRequest  tool_name, tool_input, permission_suggestions?
//! PermissionDenied   tool_name
//! PostToolUse        tool_name, tool_input, tool_response, duration_ms
//! PostToolUseFailure tool_name
//! Notification       message, title?, notification_type  <- a free STRING, not an enum
//! Stop               stop_hook_active, last_assistant_message?
//!                    background_tasks?: [{id, type, status, description, command?,
//!                                         agent_type?}]
//!                    session_crons?:   [{id, schedule, recurring, prompt}]
//! StopFailure        error: <enum>, error_details?, last_assistant_message?
//!                        <- the field is `error`, NOT `error_type`
//! SessionEnd         reason: "clear"|"resume"|"logout"|"prompt_input_exit"
//!                            |"other"|"bypass_permissions_disabled"
//! ```
//!
//! `StopFailure.error` is one of `authentication_failed`,
//! `oauth_org_not_allowed`, `billing_error`, `rate_limit`, `overloaded`,
//! `invalid_request`, `model_not_found`, `server_error`, `unknown`,
//! `max_output_tokens`.
//!
//! `background_tasks` is documented as "In-flight background work
//! (running/pending + backgrounded) … Empty array when nothing is in
//! flight", so non-empty is by itself the in-flight signal.
//!
//! `notification_type` is an unconstrained string. The binary emits at
//! least `permission_prompt`, `idle_prompt`, `agent_needs_input`,
//! `elicitation_dialog`, `auth_success`, `elicitation_complete`,
//! `elicitation_response`, `agent_completed`, `worker_permission_prompt`,
//! `push_notification`, `computer_use_enter` and `computer_use_exit` —
//! more than any doc lists, which is why [`notification`] treats unknown
//! values as a first-class case rather than an error.
//!
//! Subagent completion arrives as `SubagentStop`, a distinct event Roost
//! does not register. `agent_id` is handled here as defense in depth, not
//! as a fix for a reachable bug.
//!
//! The tool events (plan 046 §3.1) were added from the 2026-09-04 probe,
//! which pinned the order `PreToolUse → PermissionRequest → (dialog) →
//! PostToolUse`. There is no second `PreToolUse` after an approval, so
//! the tab returns to `working` when the approved tool *finishes*, not
//! when it starts — the honest consequence of the contract, documented
//! rather than papered over.

use roost_ipc::agent::{
    AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::Value;

use crate::common::{array_len, field, has_field, non_empty, parse_normalized};

/// Ownership `source` for every report this adapter emits.
pub const SOURCE: &str = "claude";

/// The canonical `hook_event_name` spellings Roost maps. The one
/// vocabulary: `roostctl claude install` writes these names into
/// `claude-settings.json`, and [`canonical_hook_event`] resolves every
/// accepted spelling onto them.
///
/// Listed in the order a turn fires them, which is also the order the
/// probe recorded.
pub const CLAUDE_HOOK_EVENTS: [&str; 11] = [
    EventKind::SessionStart.canonical(),
    EventKind::UserPromptSubmit.canonical(),
    EventKind::PreToolUse.canonical(),
    EventKind::PermissionRequest.canonical(),
    EventKind::PermissionDenied.canonical(),
    EventKind::PostToolUse.canonical(),
    EventKind::PostToolUseFailure.canonical(),
    EventKind::Notification.canonical(),
    EventKind::Stop.canonical(),
    EventKind::StopFailure.canonical(),
    EventKind::SessionEnd.canonical(),
];

const TITLE: &str = "Claude Code";

/// Resolve any accepted spelling of a hook event to its canonical
/// `hook_event_name`, or `None` when it isn't one this adapter maps.
///
/// Accepts Claude's own spelling (`SessionStart`), CLI-style variants
/// (`session-start`, `SESSION_END`), and the legacy `prompt-submit`
/// alias every already-installed `claude-settings.json` still carries.
pub fn canonical_hook_event(input: &str) -> Option<&'static str> {
    EventKind::parse(input).map(EventKind::canonical)
}

/// Map one Claude Code hook event to the reports it implies.
///
/// `event` accepts every spelling [`canonical_hook_event`] does, so
/// `roostctl`'s subcommand spelling and Claude's wire spelling reach the
/// same arm.
///
/// Returns an empty vector for an event this adapter has no mapping for,
/// including a malformed payload. Nothing here fails: a hook that cannot
/// be interpreted must not break the turn it fired from.
pub fn claude_event_to_reports(
    event: &str,
    payload: &Value,
    tab_id: i64,
) -> Vec<TabAgentReportParams> {
    // grok and cursor execute Claude-format hooks too — grok when a
    // Claude-shaped settings file is configured, cursor unconditionally
    // through its `claudeUserHooks` path — so with Roost's Claude
    // entries installed either would run `agent-hook claude` on its own
    // events and alternate ownership with its real adapter. That is not
    // cosmetic: a claim is unconditional, so a stray one evicts the real
    // owner and no release from that owner can ever match again.
    //
    // Claude's own payloads are snake_case only (verified across the
    // whole probe log), which makes grok's camelCase `hookEventName`
    // twin a positive discriminator rather than a heuristic;
    // `conversation_id` and `cursor_version` are cursor's.
    for foreign in ["hookEventName", "conversation_id", "cursor_version"] {
        if payload.get(foreign).is_some() {
            return Vec::new();
        }
    }

    // A payload that is not an object carries no discriminator to
    // reject and no `session_id` to scope by, so every accessor below
    // would read as "absent" and a `SessionStart` would claim the tab
    // for an id nothing can release. Foreign or corrupt traffic must
    // cost nothing, so it maps to nothing.
    if !payload.is_object() {
        return Vec::new();
    }

    // Recognize the event before touching the rest of the payload: an
    // unmapped event must cost nothing, whatever size string it arrived
    // with.
    let Some(kind) = EventKind::parse(event) else {
        return Vec::new();
    };

    let session_id = field(payload, "session_id");
    if matches!(kind, EventKind::SessionStart) && session_id.is_empty() {
        // A claim supersedes any live owner unconditionally (plan §3.3),
        // so a SessionStart that lost its session id would evict a
        // healthy session and install an owner no release can ever
        // match. Dropping it is the safe direction: the event carries no
        // notification, so nothing is lost.
        return Vec::new();
    }

    let base = TabAgentReportParams {
        session_id: session_id.to_string(),
        ..TabAgentReportParams::sessionless(tab_id, SOURCE, OwnershipAction::Preserve, None)
    };

    let mut report = match kind {
        EventKind::SessionStart => session_start(base, payload),
        EventKind::UserPromptSubmit => user_prompt_submit(base),
        EventKind::PreToolUse => tool_progress(base, "pre_tool_use"),
        EventKind::PostToolUse => tool_progress(base, "post_tool_use"),
        EventKind::PostToolUseFailure => tool_progress(base, "post_tool_use_failure"),
        EventKind::PermissionDenied => tool_progress(base, "permission_denied"),
        EventKind::PermissionRequest => permission_request(base, payload),
        EventKind::Notification => notification(base, payload),
        EventKind::Stop => stop(base, payload),
        EventKind::StopFailure => stop_failure(base, payload),
        EventKind::SessionEnd => session_end(base),
    };

    // Presence, not non-emptiness: the schema is `E.string().optional()`
    // with no non-empty constraint, so `agent_id: ""` still means the
    // event fired inside a subagent.
    if has_field(payload, "agent_id") {
        // The event fired inside a subagent, so it describes the
        // subagent's turn, not the tab owner's. Keep the notification —
        // the user still wants to hear about it — and drop everything
        // that would mutate the owner record. Cheap defense in depth:
        // subagent completion arrives as `SubagentStop`, which Roost does
        // not register, so no shipped path reaches this today.
        if report.attention != AttentionOp::Set {
            return Vec::new();
        }
        report.ownership_action = OwnershipAction::Preserve;
        report.lifecycle = None;
        // With no lifecycle left to patch, a surviving guard would only
        // gate the notification — the one thing this branch is keeping.
        report.lifecycle_if = None;
        report.detail.clear();
        report.metadata.clear();
    }

    vec![report]
}

fn session_start(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    let source = non_empty(field(payload, "source")).unwrap_or("session_start");
    report.ownership_action = OwnershipAction::Claim;
    report.lifecycle = Some(AgentLifecycle::Inactive);
    report.detail = source.to_string();
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
/// — the turn is running. A denied or failed tool is not a failed turn:
/// Claude keeps going and reports the outcome itself.
fn tool_progress(mut report: TabAgentReportParams, detail: &str) -> TabAgentReportParams {
    report.lifecycle = Some(AgentLifecycle::Working);
    report.detail = detail.to_string();
    report
}

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

fn notification(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    let kind = field(payload, "notification_type");

    // `notification_type` is an open string, so new values are expected
    // and the default is deliberately timid: fire the notification,
    // leave lifecycle alone. A false `waiting` is worse than a missed
    // one, because the state dot is sticky and would wrongly read
    // "blocked" while the notification reaches the user either way.
    //
    // The two guarded rows exist because these notifications are timers,
    // not transitions, and Claude will happily fire one at a turn that
    // has already moved on:
    //
    // * `permission_prompt` arrives ~6 s after `PermissionRequest`
    //   already moved the tab to `waiting`, so in the normal order it
    //   fires vetoed and does not banner a second time. It still does
    //   the whole job on a legacy settings file that has no
    //   `PermissionRequest` hook, where `working` is the live state.
    // * `idle_prompt` is a ~60 s nag, but it is also the *only* later
    //   signal after an Esc interrupt (Claude has no interrupt hook).
    //   Guarding it on `working` lets it end an interrupted turn while
    //   leaving a real `waiting` or `failed` alone — the reason the
    //   pre-046 mapping refused to touch lifecycle here at all.
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
        "agent_needs_input" | "elicitation_dialog" => {
            (Some(AgentLifecycle::Waiting), None, Severity::Warn)
        }
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
        .unwrap_or("Claude needs input")
        .to_string();
    report.detail = non_empty(kind).unwrap_or("notification").to_string();
    report
}

fn stop(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    // `background_tasks` is defined as in-flight work only, so its length
    // is the whole signal — filtering on each item's `status` would
    // re-derive something the producer already guarantees. `session_crons`
    // is the opposite: a scheduled future wake is not in-flight work, so
    // it never blocks `finished`.
    let in_flight = array_len(payload, "background_tasks");
    let crons = array_len(payload, "session_crons");

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

    // Written on every `Stop`, zeros included: `apply_report` merges
    // metadata and has no delete channel, so writing only non-zero counts
    // would strand a stale one after the work drains.
    report
        .metadata
        .insert("background_tasks".to_string(), in_flight.to_string());
    report
        .metadata
        .insert("session_crons".to_string(), crons.to_string());
    report
}

fn stop_failure(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    // `unknown` is a real member of the payload's own error enum, so an
    // absent field degrades into the vocabulary rather than out of it.
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

/// The events this adapter maps. Parsed from either the payload's own
/// `hook_event_name` (`SessionStart`) or a CLI-style alias
/// (`session-start`): separators and case are normalized away, so
/// `roostctl`'s subcommand spelling and Claude's wire spelling reach the
/// same arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PermissionDenied,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    Stop,
    StopFailure,
    SessionEnd,
}

impl EventKind {
    fn parse(event: &str) -> Option<EventKind> {
        parse_normalized(
            event,
            &[
                ("sessionstart", EventKind::SessionStart),
                ("userpromptsubmit", EventKind::UserPromptSubmit),
                // `roostctl`'s own legacy spelling, which every settings
                // file an already-run `claude install` wrote still uses.
                // It shares no run of characters with
                // `UserPromptSubmit`, so normalization alone can't reach
                // it.
                ("promptsubmit", EventKind::UserPromptSubmit),
                ("pretooluse", EventKind::PreToolUse),
                ("permissionrequest", EventKind::PermissionRequest),
                ("permissiondenied", EventKind::PermissionDenied),
                ("posttooluse", EventKind::PostToolUse),
                ("posttoolusefailure", EventKind::PostToolUseFailure),
                ("notification", EventKind::Notification),
                ("stopfailure", EventKind::StopFailure),
                ("stop", EventKind::Stop),
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
            EventKind::PermissionDenied => "PermissionDenied",
            EventKind::PostToolUse => "PostToolUse",
            EventKind::PostToolUseFailure => "PostToolUseFailure",
            EventKind::Notification => "Notification",
            EventKind::Stop => "Stop",
            EventKind::StopFailure => "StopFailure",
            EventKind::SessionEnd => "SessionEnd",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mapped event must also be an *installed* one. A variant
    /// added to `EventKind` but left out of `CLAUDE_HOOK_EVENTS` would
    /// be understood by the adapter and never written into any agent's
    /// config, so it could only ever arrive by hand. The match below
    /// stops compiling when a variant is added, which is what forces
    /// this list to be updated alongside the enum.
    #[test]
    fn every_event_kind_is_an_installed_hook_event() {
        let all = [
            EventKind::SessionStart,
            EventKind::UserPromptSubmit,
            EventKind::PreToolUse,
            EventKind::PermissionRequest,
            EventKind::PermissionDenied,
            EventKind::PostToolUse,
            EventKind::PostToolUseFailure,
            EventKind::Notification,
            EventKind::Stop,
            EventKind::StopFailure,
            EventKind::SessionEnd,
        ];
        for kind in all {
            match kind {
                EventKind::SessionStart
                | EventKind::UserPromptSubmit
                | EventKind::PreToolUse
                | EventKind::PermissionRequest
                | EventKind::PermissionDenied
                | EventKind::PostToolUse
                | EventKind::PostToolUseFailure
                | EventKind::Notification
                | EventKind::Stop
                | EventKind::StopFailure
                | EventKind::SessionEnd => {}
            }
            assert!(
                CLAUDE_HOOK_EVENTS.contains(&kind.canonical()),
                "{} is mapped but never installed",
                kind.canonical()
            );
        }
        assert_eq!(all.len(), CLAUDE_HOOK_EVENTS.len());
    }
}
