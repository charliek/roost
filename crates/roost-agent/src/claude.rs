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
//! SessionStart      source: "startup"|"resume"|"clear"|"compact"|"fork"
//!                   agent_type?  model?  session_title?
//! UserPromptSubmit  prompt
//!                   source?: "user"|"sdk"|"system"|"loop_wakeup"|"schedule_wakeup"
//! Notification      message, title?, notification_type   <- a free STRING, not an enum
//! Stop              stop_hook_active, last_assistant_message?
//!                   background_tasks?: [{id, type, status, description, command?,
//!                                        agent_type?}]
//!                   session_crons?:   [{id, schedule, recurring, prompt}]
//! StopFailure       error: <enum>, error_details?, last_assistant_message?
//!                       <- the field is `error`, NOT `error_type`
//! SessionEnd        reason: "clear"|"resume"|"logout"|"prompt_input_exit"
//!                           |"other"|"bypass_permissions_disabled"
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

use roost_ipc::agent::{
    AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::Value;

/// Ownership `source` for every report this adapter emits.
pub const SOURCE: &str = "claude";

/// The canonical `hook_event_name` spellings Roost maps. The one
/// vocabulary: `roostctl claude install` writes these names into
/// `claude-settings.json`, and [`canonical_hook_event`] resolves every
/// accepted spelling onto them.
pub const CLAUDE_HOOK_EVENTS: [&str; 6] = [
    EventKind::SessionStart.canonical(),
    EventKind::UserPromptSubmit.canonical(),
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
    // Recognize the event before touching the payload: an unmapped
    // event must cost nothing, whatever size string it arrived with.
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

fn notification(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    let kind = field(payload, "notification_type");

    // Only the four blocking types move the lifecycle. Every other value
    // — the known informational ones and any string this build has never
    // seen — fires the notification but leaves lifecycle alone. That
    // asymmetry is deliberate: `notification_type` is an open string, so
    // new values are expected, and a false `waiting` is worse than a
    // missed one because the state dot is sticky and would wrongly read
    // "blocked" while the notification reaches the user either way.
    report.lifecycle = match kind {
        "permission_prompt" | "idle_prompt" | "agent_needs_input" | "elicitation_dialog" => {
            Some(AgentLifecycle::Waiting)
        }
        _ => None,
    };
    report.severity = match report.lifecycle {
        Some(_) => Severity::Warn,
        None => Severity::Info,
    };
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
    Notification,
    Stop,
    StopFailure,
    SessionEnd,
}

impl EventKind {
    /// Matches without allocating, so an unrecognized event of any
    /// length costs only the comparison.
    fn parse(event: &str) -> Option<EventKind> {
        let eq = |want: &str| {
            let mut w = want.bytes();
            let matched = event
                .bytes()
                .filter(u8::is_ascii_alphanumeric)
                .all(|c| w.next() == Some(c.to_ascii_lowercase()));
            matched && w.next().is_none()
        };
        for (name, kind) in [
            ("sessionstart", EventKind::SessionStart),
            ("userpromptsubmit", EventKind::UserPromptSubmit),
            // `roostctl`'s own legacy spelling, which every settings file
            // an already-run `claude install` wrote still uses. It shares
            // no run of characters with `UserPromptSubmit`, so
            // normalization alone can't reach it.
            ("promptsubmit", EventKind::UserPromptSubmit),
            ("notification", EventKind::Notification),
            ("stopfailure", EventKind::StopFailure),
            ("stop", EventKind::Stop),
            ("sessionend", EventKind::SessionEnd),
        ] {
            if eq(name) {
                return Some(kind);
            }
        }
        None
    }

    const fn canonical(self) -> &'static str {
        match self {
            EventKind::SessionStart => "SessionStart",
            EventKind::UserPromptSubmit => "UserPromptSubmit",
            EventKind::Notification => "Notification",
            EventKind::Stop => "Stop",
            EventKind::StopFailure => "StopFailure",
            EventKind::SessionEnd => "SessionEnd",
        }
    }
}

fn field<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Present and non-null. `agent_id: null` is JSON's way of saying the
/// key isn't carrying a value, so it reads as absent.
fn has_field(payload: &Value, key: &str) -> bool {
    payload.get(key).is_some_and(|v| !v.is_null())
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn array_len(payload: &Value, key: &str) -> usize {
    payload
        .get(key)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}
