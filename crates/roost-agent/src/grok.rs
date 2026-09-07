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
//! probe; four sessions back-to-back, 44 records) and extended by a gx
//! probe on 2026-09-07 (plan 051; the `gx-gate` and `gx-failure`
//! fixtures). The payload borrows Claude's envelope but duplicates most
//! fields under both casings:
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
//! Stop               reason (a free string, not an enum: "end_turn" at a
//!                     turn end, "shutdown" / "channel_closed" at session
//!                     end), stopHookActive (a JSON bool),
//!                     backgroundTasks / sessionCrons — all camelCase
//!                     only, no snake sibling at all, and the session-end
//!                     fire omits both arrays
//! StopFailure        error (a classified label: rate_limit,
//!                     authentication_failed, invalid_request, server_error,
//!                     max_output_tokens, unknown), errorDetails (camelCase
//!                     only, clipped by gx at 1000 chars), lastAssistantMessage
//! StopCancelled      reason: "user_interrupt", cancelTrigger: "esc",
//!                     cancelledBy: "user" (all camelCase only) — the
//!                     Esc-interrupt signal; grok has no interrupt hook
//!                     as such, this *is* it
//! Notification       message, notificationType (camelCase only — no
//!                     snake_case `notification_type` exists at all);
//!                     observed types: permission_prompt, idle_prompt,
//!                     agent_error
//! SessionEnd         reason: "shutdown"
//! (any event)        gxRemote (camelCase only — gx stamps this once its
//!                     lane binds, so it can appear mid-session; a
//!                     token-free loopback base URL, e.g.
//!                     "http://127.0.0.1:2421")
//! ```
//!
//! grok has no `PermissionRequest` hook. Its only blocked signal is
//! `Notification` with `notificationType: permission_prompt` — observed
//! live in the probe (session 3, plan mode) with `message: "Plan
//! approval requested"`.
//!
//! `Stop` is a *gate*, not merely a turn end — see [`stop`], which is
//! the one place that reads `reason` and `stopHookActive`.
//!
//! Every session in the 2026-09-04 probe ends with `SessionEnd`
//! immediately followed by a trailing `Stop{reason: shutdown}`, and gx
//! still awaits `SessionEnd`'s hooks before dispatching that fire.
//! Ownership is already released by then, so the server drops it on the
//! ownership mismatch (`apply_report`'s `owner_matches`) — asserted in
//! the fixture replay. [`stop`]'s own `reason` check is a second line of
//! defense against a delivery order that puts the fire first, not a
//! replacement for the server's.
//!
//! A subagent turn (`spawn_subagent`) runs under its **own** session id
//! and stamps `subagentType` on every event, with no `SessionStart` of
//! its own. Its reports therefore never match the tab's owner and the
//! server drops them, so there is no `subagentType` filter here and
//! none is needed; the `gx-gate` replay pins that.
//!
//! `PermissionDenied` and `PostToolUseFailure` are in grok's registered
//! event list but have not been observed in either probe (auto-approve
//! was on, no tool failed); they are mapped from the table below and
//! pinned by synthetic payloads, not fixture evidence.

use roost_ipc::agent::{
    AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::Value;

use crate::common::{bool_field, field, field_alias, has_field, non_empty, parse_normalized};

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

    let mut report = match kind {
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

    // gx's lane binds asynchronously after the leader is up, so
    // `gxRemote` can appear mid-session on any event kind rather than
    // only on `SessionStart` — checked here, after the match, rather
    // than per-kind. The value is a discovery hint, never liveness (see
    // the module doc and plan 051 §3.3), which is exactly why the shape
    // check below is load-bearing: it is what lets `doctor.rs` allowlist
    // `gx.remote` for verbatim display without re-validating it there.
    // Checking on `SessionEnd` too is harmless — `Release` drops the
    // record regardless of what got stamped into it.
    if let Some(url) = non_empty(field(payload, "gxRemote")).filter(|url| loopback_base_url(url)) {
        report
            .metadata
            .insert("gx.remote".to_string(), url.to_string());
    }

    vec![report]
}

/// `true` for a `http://` URL whose host is `127.0.0.1`, `localhost` or
/// `[::1]`, followed by `:` and a decimal port in `1..=65535` and
/// nothing else — no userinfo, path, query, or fragment. Anything else,
/// including an absent/empty/non-string `gxRemote`, is handled by the
/// caller via [`non_empty`]; this only judges the shape once a
/// non-empty string is in hand.
fn loopback_base_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("http://") else {
        return false;
    };
    for host in ["127.0.0.1", "localhost", "[::1]"] {
        let Some(after_host) = rest.strip_prefix(host) else {
            continue;
        };
        let Some(port) = after_host.strip_prefix(':') else {
            continue;
        };
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        return matches!(port.parse::<u32>(), Ok(p) if (1..=65535).contains(&p));
    }
    false
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
    let (lifecycle, lifecycle_if, severity, attention) = match kind {
        "permission_prompt" => (
            Some(AgentLifecycle::Waiting),
            Some(vec![AgentLifecycle::Working]),
            Severity::Warn,
            AttentionOp::Set,
        ),
        "idle_prompt" => (
            Some(AgentLifecycle::Finished),
            Some(vec![AgentLifecycle::Working]),
            Severity::Info,
            AttentionOp::Set,
        ),
        // Silent: gx fires this immediately before the `StopFailure` for
        // the same turn, which carries the same text with the severity
        // that belongs on it. A generic `Info` banner here would just
        // show the failure twice, the first time understated.
        "agent_error" => (None, None, Severity::Info, AttentionOp::Preserve),
        _ => (None, None, Severity::Info, AttentionOp::Set),
    };
    report.lifecycle = lifecycle;
    report.lifecycle_if = lifecycle_if;
    report.severity = severity;
    report.attention = attention;
    report.title = non_empty(field(payload, "title"))
        .unwrap_or(TITLE)
        .to_string();
    report.body = non_empty(field(payload, "message"))
        .unwrap_or("Grok needs input")
        .to_string();
    report.detail = non_empty(kind).unwrap_or("notification").to_string();
    report
}

/// `Stop` is a gate, so a fire is not proof the turn ended.
///
/// A blocking Stop hook makes gx continue and fire `Stop` again next
/// round, and `stopHookActive` is true for *every* fire of a continued
/// turn — the final one included. A passive observer cannot tell a
/// continuation from the end, so a continued fire reports the honest
/// `Working` and lets the `idle_prompt` Notification (already
/// `Finished` guarded on `Working`) settle the turn ~60 s after it
/// really ends. Calling it `Finished` instead would show a working tab
/// as idle and re-banner "Turn complete" every round.
///
/// Three rules, first match wins. They cannot actually overlap — a
/// session-end fire always carries `stopHookActive: false` — so the
/// order is chosen for readability, not correctness.
fn stop(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    // `backgroundTasks`/`sessionCrons` are camelCase-only for grok — no
    // snake_case sibling exists, so no fallback is needed here. An
    // absent array means "this fire says nothing about them" rather than
    // zero: the session-end fire omits both and metadata has no delete
    // channel, so stamping a `0` would overwrite a live count for good.
    let in_flight = payload
        .get("backgroundTasks")
        .and_then(Value::as_array)
        .map(Vec::len);
    let crons = payload
        .get("sessionCrons")
        .and_then(Value::as_array)
        .map(Vec::len);
    for (count, metadata_key) in [(in_flight, "background_tasks"), (crons, "session_crons")] {
        if let Some(count) = count {
            report
                .metadata
                .insert(metadata_key.to_string(), count.to_string());
        }
    }

    // 1. Anything but a turn end. `shutdown` and `channel_closed` are
    //    gx's session-end fires; a reason gx adds later gets the same
    //    no-op, because gx's own guidance to hook authors is to gate on
    //    `reason == "end_turn"` and the backstops (`idle_prompt`,
    //    `SessionEnd`) bound what that costs. Absent or empty is treated
    //    as a turn end — a producer that drops the field is likelier to
    //    predate it than to mean something new by it.
    let reason = field(payload, "reason");
    if !reason.is_empty() && reason != "end_turn" {
        report.detail = format!("stop:{reason}");
        return report;
    }

    // 2. The continued fire. `Clear` mirrors `user_prompt_submit` — a
    //    continuation round is functionally a new prompt from the gate —
    //    and drops the stale dot a residual first fire may have lit.
    if bool_field(payload, "stopHookActive") {
        report.lifecycle = Some(AgentLifecycle::Working);
        report.attention = AttentionOp::Clear;
        report.detail = "stop:continued".to_string();
        return report;
    }

    // 3. The first fire, which is the whole no-gate majority.
    let in_flight = in_flight.unwrap_or(0);
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
}

fn stop_failure(mut report: TabAgentReportParams, payload: &Value) -> TabAgentReportParams {
    let error = non_empty(field(payload, "error")).unwrap_or("unknown");
    report.lifecycle = Some(AgentLifecycle::Failed);
    report.attention = AttentionOp::Set;
    report.severity = Severity::Error;
    report.title = TITLE.to_string();
    // gx emits only the camelCase spelling and clips it at 1000 chars;
    // roost passes it through and leaves truncation to the renderers.
    report.body = non_empty(field_alias(payload, "error_details", "errorDetails"))
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
