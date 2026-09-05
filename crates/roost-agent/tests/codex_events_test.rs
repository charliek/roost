//! One case per row of plan 046 §3.1's codex table, plus the
//! malformed-input edges every adapter is required to survive.
//!
//! The probe's live session ran with approvals off, so it never
//! exercised `PreToolUse` or `PermissionRequest`; both are pinned here
//! by synthetic payloads built from the plan's table, not by fixture
//! evidence (`codex.rs`'s module doc says so too).

use roost_agent::codex::{codex_event_to_reports, CODEX_HOOK_EVENTS, SOURCE};
use roost_ipc::agent::{
    validate_report, AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::{json, Value};

const TAB: i64 = 7;

fn only(event: &str, payload: &Value) -> TabAgentReportParams {
    let reports = codex_event_to_reports(event, payload, TAB);
    assert_eq!(reports.len(), 1, "{event} should map to exactly one report");
    let report = reports.into_iter().next().unwrap();
    assert_eq!(report.tab_id, TAB);
    assert_eq!(report.source, SOURCE);
    validate_report(&report).expect("every emitted report must be valid");
    report
}

fn none(event: &str, payload: &Value) {
    assert!(
        codex_event_to_reports(event, payload, TAB).is_empty(),
        "{event} should map to no reports"
    );
}

// ---------------------------------------------------------------------
// §3.1 rows
// ---------------------------------------------------------------------

#[test]
fn session_start_claims_and_records_metadata() {
    let report = only(
        "SessionStart",
        &json!({
            "session_id": "s-1",
            "source": "startup",
            "model": "gpt-6-astra",
            "permission_mode": "default",
        }),
    );
    assert_eq!(report.session_id, "s-1");
    assert_eq!(report.ownership_action, OwnershipAction::Claim);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert_eq!(report.attention, AttentionOp::Preserve);
    assert_eq!(report.detail, "startup");
    assert_eq!(report.metadata["model"], "gpt-6-astra");
    assert_eq!(report.metadata["permission_mode"], "default");
}

#[test]
fn session_start_omits_absent_metadata() {
    let report = only("SessionStart", &json!({ "session_id": "s-1" }));
    assert_eq!(report.ownership_action, OwnershipAction::Claim);
    assert!(report.metadata.is_empty());
    assert_eq!(report.detail, "session_start");
}

#[test]
fn a_session_start_without_a_session_id_is_dropped() {
    for payload in [
        json!({ "source": "startup" }),
        json!({ "session_id": "", "source": "startup" }),
        json!({ "session_id": 12345, "source": "startup" }),
    ] {
        none("SessionStart", &payload);
    }
}

#[test]
fn user_prompt_submit_works_and_clears_attention() {
    let report = only(
        "UserPromptSubmit",
        &json!({ "session_id": "s-1", "prompt": "hi", "turn_id": "t-1" }),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Working));
    assert_eq!(report.attention, AttentionOp::Clear);
}

/// Not observed live (approvals off) — synthetic, per the plan's table.
#[test]
fn tool_events_keep_the_turn_running_without_notifying() {
    for (event, detail) in [
        ("PreToolUse", "pre_tool_use"),
        ("PostToolUse", "post_tool_use"),
    ] {
        let report = only(
            event,
            &json!({ "session_id": "s-1", "tool_name": "Bash", "turn_id": "t-1" }),
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

/// Not observed live (Charlie's `codex` runs with approvals off) —
/// synthetic, per the plan's table.
#[test]
fn permission_request_blocks_and_names_the_tool() {
    let report = only(
        "PermissionRequest",
        &json!({ "session_id": "s-1", "tool_name": "Bash", "turn_id": "t-1" }),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Waiting));
    assert_eq!(report.lifecycle_if, None);
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.severity, Severity::Warn);
    assert_eq!(report.title, "Codex");
    assert_eq!(report.body, "Needs permission to use `Bash`");
    assert_eq!(report.detail, "permission_request");
}

#[test]
fn permission_request_without_a_tool_name_still_reads_as_a_sentence() {
    let report = only("PermissionRequest", &json!({ "session_id": "s-1" }));
    assert_eq!(report.body, "Needs permission to use a tool");
}

#[test]
fn stop_finishes_with_no_background_task_concept() {
    let report = only(
        "Stop",
        &json!({ "session_id": "s-1", "stop_hook_active": false, "last_assistant_message": "done" }),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.severity, Severity::Info);
    assert_eq!(report.title, "Codex");
    assert_eq!(report.body, "Turn complete");
    assert_eq!(report.detail, "stop");
    assert!(report.metadata.is_empty());
}

/// Codex has no failure hook: a turn that errored still only fires
/// `Stop`, and reads as finished — a known, accepted gap (module doc),
/// not something this test papers over.
#[test]
fn stop_reads_as_finished_even_when_the_turn_actually_failed() {
    let report = only(
        "Stop",
        &json!({ "session_id": "s-1", "last_assistant_message": "error: rate limited" }),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
    assert_eq!(report.body, "Turn complete");
}

/// The Esc-interrupt signal. Ownership continues; no banner.
#[test]
fn interrupt_finishes_and_clears_attention_without_a_banner() {
    let report = only(
        "Interrupt",
        &json!({ "session_id": "s-1", "turn_id": "t-1" }),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
    assert_eq!(report.attention, AttentionOp::Clear);
    assert_eq!(report.detail, "interrupt");
}

#[test]
fn session_end_releases_and_clears() {
    let report = only(
        "SessionEnd",
        &json!({ "session_id": "s-1", "reason": "other" }),
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
    for event in CODEX_HOOK_EVENTS {
        assert!(
            !codex_event_to_reports(event, &json!({ "session_id": "s-1" }), TAB).is_empty(),
            "{event}"
        );
    }
}

#[test]
fn unknown_event_names_map_to_nothing() {
    for event in ["", "SubagentStop", "PreCompact", "🙂"] {
        none(event, &json!({ "session_id": "s-1" }));
    }
}

// ---------------------------------------------------------------------
// Foreign / malformed input
// ---------------------------------------------------------------------

/// grok/gx always carry the camelCase `hookEventName` twin; cursor
/// always carries `conversation_id`/`cursor_version`. Neither ever
/// appears in a real codex payload.
#[test]
fn a_payload_from_grok_or_cursor_yields_no_reports() {
    for discriminator in ["hookEventName", "conversation_id", "cursor_version"] {
        for event in CODEX_HOOK_EVENTS {
            let mut payload =
                json!({ "session_id": "s-1", "source": "startup", "tool_name": "Bash" });
            payload[discriminator] = json!("x");
            none(event, &payload);
        }
    }
}

#[test]
fn a_payload_that_is_not_an_object_maps_to_nothing() {
    for raw in [
        json!("a bare string"),
        json!([{"session_id": "s1"}]),
        json!(7),
        json!(null),
        json!(true),
    ] {
        for event in CODEX_HOOK_EVENTS {
            assert!(
                codex_event_to_reports(event, &raw, TAB).is_empty(),
                "{event} produced reports for {raw}"
            );
        }
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
        json!({ "session_id": 7, "tool_name": ["nested"] }),
    ];
    for event in CODEX_HOOK_EVENTS {
        for payload in &payloads {
            for report in codex_event_to_reports(event, payload, TAB) {
                validate_report(&report).unwrap_or_else(|e| panic!("{event} on {payload}: {e}"));
            }
        }
    }
}

#[test]
fn every_report_carries_source_codex_and_the_payload_session_id() {
    let payload = json!({
        "session_id": "s-42",
        "source": "startup",
        "tool_name": "Bash",
        "turn_id": "t-1",
    });
    for event in CODEX_HOOK_EVENTS {
        let report = only(event, &payload);
        assert_eq!(report.source, "codex", "{event}");
        assert_eq!(report.session_id, "s-42", "{event}");
    }
}
