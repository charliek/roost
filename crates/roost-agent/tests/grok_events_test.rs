//! One case per row of plan 046 §3.1's grok/gx table, plus the
//! malformed-input edges every adapter is required to survive.
//!
//! `PermissionDenied`, `PostToolUseFailure` and `StopFailure` were not
//! observed in the probe (nothing failed during the run); those three
//! are pinned here by synthetic payloads, not fixture evidence.

use roost_agent::grok::{grok_event_to_reports, GROK_HOOK_EVENTS, SOURCE};
use roost_ipc::agent::{
    validate_report, AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::{json, Value};

const TAB: i64 = 7;

/// Every grok/gx payload carries this key (verified across all 53
/// fixture records between the two agents) — every helper below adds it
/// so a hand-built payload passes the adapter's own gate the same way a
/// real one does.
fn grok(fields: Value) -> Value {
    let mut payload = fields;
    if let Some(object) = payload.as_object_mut() {
        object.entry("hookEventName").or_insert_with(|| json!("x"));
    }
    payload
}

fn only(event: &str, payload: &Value) -> TabAgentReportParams {
    let reports = grok_event_to_reports(event, payload, TAB);
    assert_eq!(reports.len(), 1, "{event} should map to exactly one report");
    let report = reports.into_iter().next().unwrap();
    assert_eq!(report.tab_id, TAB);
    assert_eq!(report.source, SOURCE);
    validate_report(&report).expect("every emitted report must be valid");
    report
}

fn none(event: &str, payload: &Value) {
    assert!(
        grok_event_to_reports(event, payload, TAB).is_empty(),
        "{event} should map to no reports"
    );
}

// ---------------------------------------------------------------------
// §3.1 rows
// ---------------------------------------------------------------------

#[test]
fn session_start_claims() {
    let report = only(
        "SessionStart",
        &grok(json!({ "session_id": "s-1", "source": "new" })),
    );
    assert_eq!(report.session_id, "s-1");
    assert_eq!(report.ownership_action, OwnershipAction::Claim);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert_eq!(report.attention, AttentionOp::Preserve);
    assert_eq!(report.detail, "new");
    assert_eq!(report.metadata["source"], "new");
}

#[test]
fn session_start_reads_session_id_from_either_casing() {
    let camel_only = grok(json!({ "sessionId": "s-camel", "source": "new" }));
    assert_eq!(only("SessionStart", &camel_only).session_id, "s-camel");

    let both = grok(json!({ "session_id": "s-snake", "sessionId": "s-camel", "source": "new" }));
    assert_eq!(only("SessionStart", &both).session_id, "s-snake");
}

#[test]
fn a_session_start_without_a_session_id_is_dropped() {
    for payload in [
        grok(json!({ "source": "new" })),
        grok(json!({ "session_id": "", "sessionId": "", "source": "new" })),
    ] {
        none("SessionStart", &payload);
    }
}

#[test]
fn user_prompt_submit_works_and_clears_attention() {
    let report = only(
        "UserPromptSubmit",
        &grok(json!({ "session_id": "s-1", "prompt": "hi" })),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Working));
    assert_eq!(report.attention, AttentionOp::Clear);
}

#[test]
fn tool_events_keep_the_turn_running_without_notifying() {
    for (event, detail) in [
        ("PreToolUse", "pre_tool_use"),
        ("PostToolUse", "post_tool_use"),
        // Not observed in the probe — pinned by the plan's table alone.
        ("PostToolUseFailure", "post_tool_use_failure"),
        ("PermissionDenied", "permission_denied"),
    ] {
        let report = only(
            event,
            &grok(json!({ "session_id": "s-1", "toolName": "run_terminal_command" })),
        );
        assert_eq!(
            report.ownership_action,
            OwnershipAction::Preserve,
            "{event}"
        );
        assert_eq!(report.lifecycle, Some(AgentLifecycle::Working), "{event}");
        assert_eq!(report.lifecycle_if, None, "{event}");
        assert_eq!(report.attention, AttentionOp::Preserve, "{event}");
        assert_eq!(report.detail, detail, "{event}");
    }
}

/// grok has no `PermissionRequest` hook — its only blocked signal is
/// this notification, observed live in session 3 of the probe
/// (`message: "Plan approval requested"`).
#[test]
fn permission_prompt_notification_waits_but_only_from_working() {
    let report = only(
        "Notification",
        &grok(json!({
            "session_id": "s-1",
            "message": "Plan approval requested",
            "notificationType": "permission_prompt",
        })),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Waiting));
    assert_eq!(report.lifecycle_if, Some(vec![AgentLifecycle::Working]));
    assert_eq!(report.severity, Severity::Warn);
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.body, "Plan approval requested");
    assert_eq!(report.detail, "permission_prompt");
}

#[test]
fn notification_type_has_no_snake_case_form_so_only_the_camel_key_is_read() {
    let report = only(
        "Notification",
        &grok(json!({
            "session_id": "s-1",
            "message": "Waiting for your next prompt",
            "notificationType": "idle_prompt",
        })),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
    assert_eq!(report.lifecycle_if, Some(vec![AgentLifecycle::Working]));
    assert_eq!(report.severity, Severity::Info);
    assert_eq!(report.detail, "idle_prompt");
}

#[test]
fn other_notification_types_leave_lifecycle_unchanged() {
    let report = only(
        "Notification",
        &grok(json!({ "session_id": "s-1", "message": "m", "notificationType": "something_else" })),
    );
    assert_eq!(report.lifecycle, None);
    assert_eq!(report.lifecycle_if, None);
    assert_eq!(report.severity, Severity::Info);
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.detail, "something_else");
}

#[test]
fn stop_without_background_tasks_finishes() {
    for payload in [
        grok(json!({ "session_id": "s-1" })),
        grok(json!({ "session_id": "s-1", "backgroundTasks": [] })),
    ] {
        let report = only("Stop", &payload);
        assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
        assert_eq!(report.attention, AttentionOp::Set);
        assert_eq!(report.body, "Turn complete");
        assert_eq!(report.detail, "stop");
        assert_eq!(report.metadata["background_tasks"], "0");
        assert_eq!(report.metadata["session_crons"], "0");
    }
}

#[test]
fn stop_with_background_tasks_stays_working() {
    let report = only(
        "Stop",
        &grok(json!({
            "session_id": "s-1",
            "backgroundTasks": [{ "id": "b1" }, { "id": "b2" }],
            "sessionCrons": [{ "id": "c1" }],
        })),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Working));
    assert_eq!(report.body, "Waiting on 2 background tasks");
    assert_eq!(report.detail, "background_tasks:2");
    assert_eq!(report.metadata["background_tasks"], "2");
    assert_eq!(report.metadata["session_crons"], "1");
}

/// Not observed in the probe — pinned by the plan's table alone.
#[test]
fn stop_failure_maps_to_failed() {
    let report = only(
        "StopFailure",
        &grok(json!({ "session_id": "s-1", "error": "rate_limit" })),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Failed));
    assert_eq!(report.severity, Severity::Error);
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.body, "Stopped: rate_limit");
    assert_eq!(report.detail, "rate_limit");
}

/// The Esc-interrupt signal — grok has no dedicated interrupt hook, this
/// is it. Ownership continues; no banner.
#[test]
fn stop_cancelled_finishes_and_clears_attention_without_a_banner() {
    let report = only(
        "StopCancelled",
        &grok(json!({ "session_id": "s-1", "reason": "user_interrupt", "cancelTrigger": "esc" })),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
    assert_eq!(report.attention, AttentionOp::Clear);
    assert_eq!(report.detail, "stop_cancelled");
}

#[test]
fn session_end_releases_and_clears() {
    let report = only(
        "SessionEnd",
        &grok(json!({ "session_id": "s-1", "reason": "shutdown" })),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Release);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert_eq!(report.attention, AttentionOp::Clear);
}

// ---------------------------------------------------------------------
// Event-name handling
// ---------------------------------------------------------------------

#[test]
fn every_canonical_event_is_recognized() {
    for event in GROK_HOOK_EVENTS {
        assert!(
            !grok_event_to_reports(event, &grok(json!({ "session_id": "s-1" })), TAB).is_empty(),
            "{event}"
        );
    }
}

#[test]
fn unknown_event_names_map_to_nothing() {
    for event in ["", "SubagentStop", "PreCompact", "🙂"] {
        none(event, &grok(json!({ "session_id": "s-1" })));
    }
}

// ---------------------------------------------------------------------
// Foreign / malformed input
// ---------------------------------------------------------------------

/// Claude's and codex's native payloads have no camelCase keys at all —
/// this is the discriminator that keeps them from claiming a tab
/// through this adapter.
#[test]
fn a_payload_with_no_camelcase_hook_event_name_maps_to_nothing() {
    for event in GROK_HOOK_EVENTS {
        none(
            event,
            &json!({ "session_id": "s-1", "source": "new", "hook_event_name": event }),
        );
    }
}

#[test]
fn malformed_payloads_do_not_panic() {
    let payloads = [
        Value::Null,
        json!([]),
        json!("a string"),
        json!(42),
        json!({}),
        grok(json!({ "session_id": 7, "backgroundTasks": "not-array" })),
        grok(json!({ "backgroundTasks": {}, "sessionCrons": 3, "error": false })),
    ];
    for event in GROK_HOOK_EVENTS {
        for payload in &payloads {
            for report in grok_event_to_reports(event, payload, TAB) {
                validate_report(&report).unwrap_or_else(|e| panic!("{event} on {payload}: {e}"));
            }
        }
    }
}

#[test]
fn every_report_carries_source_grok_and_the_payload_session_id() {
    let payload = grok(json!({
        "session_id": "s-42",
        "source": "new",
        "message": "m",
        "notificationType": "idle_prompt",
        "error": "overloaded",
    }));
    for event in GROK_HOOK_EVENTS {
        let report = only(event, &payload);
        assert_eq!(report.source, "grok", "{event}");
        assert_eq!(report.session_id, "s-42", "{event}");
    }
}
