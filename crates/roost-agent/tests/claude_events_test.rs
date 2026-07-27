//! One case per row of plan 002 §3.8, plus the malformed-input and
//! open-vocabulary edges the mapping is required to survive.

use roost_agent::claude::{claude_event_to_reports, SOURCE};
use roost_ipc::agent::{
    validate_report, AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::{json, Value};

const TAB: i64 = 7;

fn only(event: &str, payload: &Value) -> TabAgentReportParams {
    let reports = claude_event_to_reports(event, payload, TAB);
    assert_eq!(reports.len(), 1, "{event} should map to exactly one report");
    let report = reports.into_iter().next().unwrap();
    assert_eq!(report.tab_id, TAB);
    assert_eq!(report.source, SOURCE);
    validate_report(&report).expect("every emitted report must be valid");
    report
}

fn none(event: &str, payload: &Value) {
    assert!(
        claude_event_to_reports(event, payload, TAB).is_empty(),
        "{event} should map to no reports"
    );
}

// ---------------------------------------------------------------------
// §3.8 rows
// ---------------------------------------------------------------------

#[test]
fn session_start_claims_and_records_metadata() {
    let report = only(
        "SessionStart",
        &json!({
            "session_id": "s-1",
            "source": "startup",
            "model": "claude-opus-5",
            "session_title": "roost plan 002",
            "agent_type": "ignored",
        }),
    );
    assert_eq!(report.session_id, "s-1");
    assert_eq!(report.ownership_action, OwnershipAction::Claim);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert_eq!(report.attention, AttentionOp::Preserve);
    assert_eq!(report.detail, "startup");
    assert_eq!(report.metadata["model"], "claude-opus-5");
    assert_eq!(report.metadata["source"], "startup");
    assert_eq!(report.metadata["session_title"], "roost plan 002");
    assert!(!report.metadata.contains_key("agent_type"));
}

#[test]
fn session_start_omits_absent_metadata() {
    let report = only("SessionStart", &json!({ "session_id": "s-1" }));
    assert_eq!(report.ownership_action, OwnershipAction::Claim);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert!(report.metadata.is_empty());
    assert_eq!(report.detail, "session_start");
}

#[test]
fn session_start_accepts_every_documented_source() {
    for source in ["startup", "resume", "clear", "compact", "fork"] {
        let report = only(
            "SessionStart",
            &json!({ "session_id": "s", "source": source }),
        );
        assert_eq!(report.metadata["source"], source);
        assert_eq!(report.detail, source);
        assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    }
}

#[test]
fn user_prompt_submit_works_and_clears_attention() {
    let report = only(
        "UserPromptSubmit",
        &json!({ "session_id": "s-1", "prompt": "hi", "source": "user" }),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Working));
    assert_eq!(report.attention, AttentionOp::Clear);
}

#[test]
fn blocking_notification_types_wait() {
    for kind in [
        "permission_prompt",
        "idle_prompt",
        "agent_needs_input",
        "elicitation_dialog",
    ] {
        let report = only(
            "Notification",
            &json!({ "session_id": "s-1", "message": "m", "notification_type": kind }),
        );
        assert_eq!(
            report.lifecycle,
            Some(AgentLifecycle::Waiting),
            "{kind} blocks the turn"
        );
        assert_eq!(report.severity, Severity::Warn, "{kind}");
        assert_eq!(report.attention, AttentionOp::Set, "{kind}");
        assert_eq!(report.ownership_action, OwnershipAction::Preserve, "{kind}");
        assert_eq!(report.detail, kind);
    }
}

#[test]
fn informational_notification_types_leave_lifecycle_unchanged() {
    for kind in [
        "auth_success",
        "elicitation_complete",
        "elicitation_response",
        "agent_completed",
    ] {
        let report = only(
            "Notification",
            &json!({ "session_id": "s-1", "message": "m", "notification_type": kind }),
        );
        assert_eq!(report.lifecycle, None, "{kind} must not move lifecycle");
        assert_eq!(report.severity, Severity::Info, "{kind}");
        assert_eq!(report.attention, AttentionOp::Set, "{kind}");
        assert_eq!(report.detail, kind);
    }
}

#[test]
fn unknown_notification_type_notifies_without_touching_lifecycle() {
    // `notification_type` is an open string; the binary already emits
    // values no doc lists. A missed `waiting` is recoverable, a false one
    // sticks on the dot as "blocked".
    for kind in [
        "worker_permission_prompt",
        "push_notification",
        "computer_use_enter",
        "a_type_invented_next_release",
    ] {
        let report = only(
            "Notification",
            &json!({ "session_id": "s-1", "message": "m", "notification_type": kind }),
        );
        assert_eq!(report.lifecycle, None, "{kind}");
        assert_eq!(report.severity, Severity::Info, "{kind}");
        assert_eq!(report.attention, AttentionOp::Set, "{kind}");
        assert_eq!(report.detail, kind);
    }
}

#[test]
fn notification_prefers_payload_title_and_message() {
    let report = only(
        "Notification",
        &json!({
            "session_id": "s-1",
            "title": "Permission needed",
            "message": "Claude wants to run rm -rf",
            "notification_type": "permission_prompt",
        }),
    );
    assert_eq!(report.title, "Permission needed");
    assert_eq!(report.body, "Claude wants to run rm -rf");
}

#[test]
fn notification_falls_back_when_title_message_and_type_are_absent() {
    let report = only("Notification", &json!({ "session_id": "s-1" }));
    assert_eq!(report.title, "Claude Code");
    assert_eq!(report.body, "Claude needs input");
    assert_eq!(report.detail, "notification");
    assert_eq!(report.lifecycle, None);
}

#[test]
fn stop_without_background_tasks_finishes() {
    for payload in [
        json!({ "session_id": "s-1", "stop_hook_active": false }),
        json!({ "session_id": "s-1", "background_tasks": [] }),
        json!({ "session_id": "s-1", "background_tasks": Value::Null }),
    ] {
        let report = only("Stop", &payload);
        assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
        assert_eq!(report.attention, AttentionOp::Set);
        assert_eq!(report.severity, Severity::Info);
        assert_eq!(report.title, "Claude Code");
        assert_eq!(report.body, "Turn complete");
        assert_eq!(report.detail, "stop");
        assert_eq!(report.metadata["background_tasks"], "0");
    }
}

#[test]
fn stop_with_background_tasks_stays_working() {
    let report = only(
        "Stop",
        &json!({
            "session_id": "s-1",
            "background_tasks": [
                { "id": "b1", "type": "shell", "status": "running",
                  "description": "cargo build", "command": "cargo build" },
                { "id": "b2", "type": "monitor", "status": "pending",
                  "description": "watch the log" },
            ],
        }),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Working));
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.severity, Severity::Info);
    assert_eq!(report.detail, "background_tasks:2");
    assert_eq!(report.body, "Waiting on 2 background tasks");
    assert_eq!(report.metadata["background_tasks"], "2");
}

#[test]
fn stop_does_not_filter_background_tasks_by_status() {
    // The array is in-flight work by definition, so even a status string
    // this build has never seen keeps the session `working`.
    let report = only(
        "Stop",
        &json!({
            "session_id": "s-1",
            "background_tasks": [
                { "id": "b1", "type": "workflow", "status": "some_future_status",
                  "description": "d" },
            ],
        }),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Working));
    assert_eq!(report.body, "Waiting on 1 background task");
    assert_eq!(report.detail, "background_tasks:1");
}

#[test]
fn session_crons_do_not_block_finished() {
    let report = only(
        "Stop",
        &json!({
            "session_id": "s-1",
            "background_tasks": [],
            "session_crons": [
                { "id": "c1", "schedule": "0 9 * * 1-5", "recurring": true, "prompt": "standup" },
                { "id": "c2", "schedule": "0 17 * * *", "recurring": false, "prompt": "wrap up" },
            ],
        }),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
    assert_eq!(report.detail, "stop");
    assert_eq!(report.metadata["session_crons"], "2");
    assert_eq!(report.metadata["background_tasks"], "0");
}

#[test]
fn stop_always_writes_both_counts_so_a_drained_one_cannot_go_stale() {
    let report = only("Stop", &json!({ "session_id": "s-1" }));
    assert_eq!(report.metadata["background_tasks"], "0");
    assert_eq!(report.metadata["session_crons"], "0");
}

#[test]
fn stop_failure_maps_every_documented_error() {
    for error in [
        "authentication_failed",
        "oauth_org_not_allowed",
        "billing_error",
        "rate_limit",
        "overloaded",
        "invalid_request",
        "model_not_found",
        "server_error",
        "unknown",
        "max_output_tokens",
    ] {
        let report = only(
            "StopFailure",
            &json!({ "session_id": "s-1", "error": error, "error_details": "boom" }),
        );
        assert_eq!(report.lifecycle, Some(AgentLifecycle::Failed), "{error}");
        assert_eq!(report.severity, Severity::Error, "{error}");
        assert_eq!(report.attention, AttentionOp::Set, "{error}");
        assert_eq!(
            report.ownership_action,
            OwnershipAction::Preserve,
            "{error}"
        );
        assert_eq!(report.detail, error);
        assert_eq!(report.body, "boom");
    }
}

#[test]
fn stop_failure_reads_error_not_error_type() {
    // The published docs implied `error_type`; the binary's payload key
    // is `error`. A payload carrying only `error_type` must not be
    // mistaken for a named failure.
    let report = only(
        "StopFailure",
        &json!({ "session_id": "s-1", "error_type": "rate_limit" }),
    );
    assert_eq!(report.detail, "unknown");
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Failed));
}

#[test]
fn stop_failure_without_details_describes_the_error() {
    let report = only(
        "StopFailure",
        &json!({ "session_id": "s-1", "error": "rate_limit" }),
    );
    assert_eq!(report.body, "Stopped: rate_limit");
    assert_eq!(report.detail, "rate_limit");
}

#[test]
fn session_end_releases_and_clears() {
    let report = only(
        "SessionEnd",
        &json!({ "session_id": "s-1", "reason": "prompt_input_exit" }),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Release);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert_eq!(report.attention, AttentionOp::Clear);
}

// ---------------------------------------------------------------------
// `agent_id` — the subagent filter
// ---------------------------------------------------------------------

#[test]
fn agent_id_keeps_the_notification_and_drops_the_lifecycle() {
    let report = only(
        "Notification",
        &json!({
            "session_id": "s-1",
            "agent_id": "sub-9",
            "message": "Subagent wants permission",
            "notification_type": "permission_prompt",
        }),
    );
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.body, "Subagent wants permission");
    assert_eq!(report.lifecycle, None, "a subagent must not move the owner");
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert!(report.detail.is_empty());
    assert!(report.metadata.is_empty());
}

#[test]
fn agent_id_on_stop_keeps_only_the_notification() {
    let report = only(
        "Stop",
        &json!({ "session_id": "s-1", "agent_id": "sub-9", "background_tasks": [] }),
    );
    assert_eq!(report.lifecycle, None);
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.body, "Turn complete");
    assert!(report.metadata.is_empty());
}

#[test]
fn agent_id_on_stop_failure_keeps_only_the_notification() {
    let report = only(
        "StopFailure",
        &json!({ "session_id": "s-1", "agent_id": "sub-9", "error": "overloaded" }),
    );
    assert_eq!(report.lifecycle, None);
    assert_eq!(report.severity, Severity::Error);
    assert_eq!(report.body, "Stopped: overloaded");
    assert!(report.detail.is_empty());
}

#[test]
fn agent_id_suppresses_ownership_only_events_entirely() {
    for (event, payload) in [
        (
            "SessionStart",
            json!({ "session_id": "s-1", "agent_id": "sub-9", "source": "startup" }),
        ),
        (
            "SessionEnd",
            json!({ "session_id": "s-1", "agent_id": "sub-9", "reason": "other" }),
        ),
        (
            "UserPromptSubmit",
            json!({ "session_id": "s-1", "agent_id": "sub-9", "prompt": "hi" }),
        ),
    ] {
        none(event, &payload);
    }
}

#[test]
fn an_empty_agent_id_still_means_subagent() {
    // The schema is `E.string().optional()` with no non-empty
    // constraint, so presence is the signal. Treating "" as absent let a
    // subagent SessionEnd release the main session's ownership.
    assert!(claude_event_to_reports(
        "SessionEnd",
        &json!({ "session_id": "s-1", "agent_id": "", "reason": "other" }),
        TAB,
    )
    .is_empty());

    assert!(claude_event_to_reports(
        "SessionStart",
        &json!({ "session_id": "s-1", "agent_id": "", "source": "resume" }),
        TAB,
    )
    .is_empty());
}

#[test]
fn a_null_agent_id_reads_as_absent() {
    let report = only(
        "SessionStart",
        &json!({ "session_id": "s-1", "agent_id": null, "source": "resume" }),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Claim);
}

// ---------------------------------------------------------------------
// Event-name handling
// ---------------------------------------------------------------------

#[test]
fn cli_style_aliases_reach_the_same_arm() {
    let payload = json!({ "session_id": "s-1" });
    for (alias, canonical) in [
        ("session-start", "SessionStart"),
        ("user_prompt_submit", "UserPromptSubmit"),
        ("notification", "Notification"),
        ("STOP", "Stop"),
        ("stop-failure", "StopFailure"),
        ("session-end", "SessionEnd"),
    ] {
        // Non-emptiness first: comparing two empty vectors would pass if
        // parsing regressed for every spelling at once.
        let via_alias = claude_event_to_reports(alias, &payload, TAB);
        assert!(!via_alias.is_empty(), "{alias} must map to a report");
        assert_eq!(
            via_alias,
            claude_event_to_reports(canonical, &payload, TAB),
            "{alias} vs {canonical}"
        );
    }
}

#[test]
fn unknown_event_names_map_to_nothing() {
    let payload = json!({ "session_id": "s-1" });
    for event in [
        "",
        "SubagentStop",
        "SubagentStart",
        "PreCompact",
        "PreToolUse",
        "prompt-submit",
        "Sto",
        "Stopped",
        "🙂",
    ] {
        none(event, &payload);
    }
}

// ---------------------------------------------------------------------
// Malformed input
// ---------------------------------------------------------------------

#[test]
fn an_unrecognized_event_does_not_copy_its_payload() {
    // Recognition happens before any field is read, so an unmapped event
    // costs nothing regardless of the size it arrived with. `roostctl`
    // already caps stdin at 1 MiB; this keeps the adapter itself total
    // rather than relying on that cap.
    let big = "x".repeat(4 * 1024 * 1024);
    let payload = json!({ "session_id": big, "message": big });
    assert!(claude_event_to_reports(&big, &payload, TAB).is_empty());
    assert!(claude_event_to_reports("SubagentStop", &payload, TAB).is_empty());
}

#[test]
fn malformed_payloads_do_not_panic() {
    let payloads = [
        Value::Null,
        json!([]),
        json!("a string"),
        json!(42),
        json!({}),
        json!({ "session_id": 7, "notification_type": ["nested"], "background_tasks": "not-array" }),
        json!({ "background_tasks": {}, "session_crons": 3, "error": false }),
    ];
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "Notification",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        for payload in &payloads {
            for report in claude_event_to_reports(event, payload, TAB) {
                validate_report(&report).unwrap_or_else(|e| panic!("{event} on {payload}: {e}"));
            }
        }
    }
}

#[test]
fn a_missing_session_id_still_reports_for_non_claiming_events() {
    // Dropping these would lose the notification too, and an empty
    // session id simply fails to match a live owner downstream — the
    // safe direction, because preserve and release both require a match.
    let report = only("Stop", &json!({ "background_tasks": [] }));
    assert_eq!(report.session_id, "");
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
}

#[test]
fn a_session_start_without_a_session_id_is_dropped() {
    // A claim supersedes any live owner unconditionally, so a
    // SessionStart that lost its session id would evict a healthy
    // session and install an owner that no release can ever match.
    // SessionStart carries no notification, so nothing is lost.
    for payload in [
        json!({ "source": "startup" }),
        json!({ "session_id": "", "source": "startup" }),
        json!({ "session_id": 12345, "source": "startup" }),
        json!(null),
    ] {
        assert!(
            claude_event_to_reports("SessionStart", &payload, TAB).is_empty(),
            "SessionStart claimed ownership with no session id: {payload}"
        );
    }
}

#[test]
fn a_non_string_session_id_degrades_to_empty() {
    let report = only("Stop", &json!({ "session_id": 12345 }));
    assert_eq!(report.session_id, "");
}

#[test]
fn every_report_carries_source_claude_and_the_payload_session_id() {
    let payload = json!({
        "session_id": "s-42",
        "source": "startup",
        "message": "m",
        "notification_type": "idle_prompt",
        "error": "overloaded",
    });
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "Notification",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        let report = only(event, &payload);
        assert_eq!(report.source, "claude", "{event}");
        assert_eq!(report.session_id, "s-42", "{event}");
    }
}
