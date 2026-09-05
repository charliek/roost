//! One case per row of plan 046 §3.1's cursor table, plus the
//! malformed-input edges every adapter is required to survive.
//!
//! `postToolUseFailure` is the only row the probe never exercised (the
//! one shell command in the run succeeded); it is pinned here by a
//! synthetic payload built from the plan's table.

use roost_agent::cursor::{cursor_event_to_reports, CURSOR_HOOK_EVENTS, SOURCE};
use roost_ipc::agent::{
    validate_report, AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::{json, Value};

const TAB: i64 = 7;
const VERSION: &str = "2026.09.02-c22c1a3";

/// Every payload has to carry cursor's gate, so building one by hand is
/// the exception rather than the rule.
fn payload(extra: &Value) -> Value {
    let mut base = json!({ "session_id": "s-1", "cursor_version": VERSION });
    let object = base.as_object_mut().unwrap();
    for (key, value) in extra.as_object().unwrap() {
        object.insert(key.clone(), value.clone());
    }
    base
}

fn only(event: &str, payload: &Value) -> TabAgentReportParams {
    let reports = cursor_event_to_reports(event, payload, TAB);
    assert_eq!(reports.len(), 1, "{event} should map to exactly one report");
    let report = reports.into_iter().next().unwrap();
    assert_eq!(report.tab_id, TAB);
    assert_eq!(report.source, SOURCE);
    validate_report(&report).expect("every emitted report must be valid");
    report
}

fn none(event: &str, payload: &Value) {
    assert!(
        cursor_event_to_reports(event, payload, TAB).is_empty(),
        "{event} should map to no reports"
    );
}

// ---------------------------------------------------------------------
// §3.1 rows
// ---------------------------------------------------------------------

#[test]
fn session_start_claims_and_records_metadata() {
    let report = only(
        "sessionStart",
        &payload(&json!({ "model": "cursor-grok-4.6-high-fast" })),
    );
    assert_eq!(report.session_id, "s-1");
    assert_eq!(report.ownership_action, OwnershipAction::Claim);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert_eq!(report.attention, AttentionOp::Preserve);
    assert_eq!(report.detail, "session_start");
    assert_eq!(report.metadata["model"], "cursor-grok-4.6-high-fast");
    assert_eq!(report.metadata["cursor_version"], VERSION);
}

#[test]
fn a_session_start_without_a_session_id_is_dropped() {
    for extra in [
        json!({ "session_id": "" }),
        json!({ "session_id": 12345 }),
        json!({ "session_id": null }),
    ] {
        none("sessionStart", &payload(&extra));
    }
}

#[test]
fn before_submit_prompt_works_and_clears_attention() {
    let report = only("beforeSubmitPrompt", &payload(&json!({ "prompt": "hi" })));
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Working));
    assert_eq!(report.attention, AttentionOp::Clear);
    assert_eq!(report.detail, "before_submit_prompt");
}

/// `afterAgentResponse` belongs with the tool events, not with `stop`:
/// the probe fires it immediately *before* `stop`, so treating it as the
/// end of a turn would finish every turn twice.
#[test]
fn the_mid_turn_events_keep_the_turn_running_without_notifying() {
    for (event, detail) in [
        ("preToolUse", "pre_tool_use"),
        ("postToolUse", "post_tool_use"),
        ("postToolUseFailure", "post_tool_use_failure"),
        ("afterAgentResponse", "after_agent_response"),
    ] {
        let report = only(event, &payload(&json!({ "tool_name": "Shell" })));
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

/// Every status finishes the turn — see `cursor.rs`'s module doc for why
/// `error` cannot be read as a failure — and the raw status rides along
/// in `detail`.
#[test]
fn every_stop_status_finishes_the_turn_and_is_recorded() {
    for status in ["completed", "aborted", "error", "something_new"] {
        let report = only("stop", &payload(&json!({ "status": status })));
        assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished), "{status}");
        assert_eq!(report.attention, AttentionOp::Set, "{status}");
        assert_eq!(report.severity, Severity::Info, "{status}");
        assert_eq!(report.title, "Cursor", "{status}");
        assert_eq!(report.body, "Turn complete", "{status}");
        assert_eq!(report.detail, status, "{status}");
    }
}

/// The guard is the reason a turn's second and third `stop` are silent.
#[test]
fn stop_is_guarded_on_a_turn_that_is_actually_running() {
    let report = only("stop", &payload(&json!({ "status": "completed" })));
    assert_eq!(
        report.lifecycle_if,
        Some(vec![AgentLifecycle::Working, AgentLifecycle::Waiting]),
    );
}

#[test]
fn stop_without_a_status_still_finishes() {
    let report = only("stop", &payload(&json!({})));
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
    assert_eq!(report.detail, "stop");
}

#[test]
fn session_end_releases_and_clears() {
    let report = only("sessionEnd", &payload(&json!({ "reason": "completed" })));
    assert_eq!(report.ownership_action, OwnershipAction::Release);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert_eq!(report.attention, AttentionOp::Clear);
}

// ---------------------------------------------------------------------
// Event-name handling
// ---------------------------------------------------------------------

#[test]
fn every_canonical_event_is_recognized() {
    for event in CURSOR_HOOK_EVENTS {
        assert!(
            !cursor_event_to_reports(event, &payload(&json!({})), TAB).is_empty(),
            "{event}"
        );
    }
}

/// The three events the probe recorded that Roost does not register.
/// `beforeShellExecution` in particular is deliberately unmapped: it
/// fires 0.1 s before `afterShellExecution` on an auto-approved command,
/// so it cannot mean "blocked" (plan §4).
#[test]
fn the_events_roost_does_not_register_map_to_nothing() {
    for event in [
        "afterAgentThought",
        "beforeShellExecution",
        "afterShellExecution",
        "beforeReadFile",
        "",
        "🙂",
    ] {
        none(event, &payload(&json!({ "command": "touch /tmp/x" })));
    }
}

// ---------------------------------------------------------------------
// Foreign / malformed input
// ---------------------------------------------------------------------

/// The gate runs the other direction from Claude's and codex's: cursor's
/// own version stamp is *required*, because the keys those two reject on
/// are cursor's and the event names alone overlap all three vocabularies.
#[test]
fn a_payload_without_cursors_version_stamp_yields_no_reports() {
    for event in CURSOR_HOOK_EVENTS {
        none(event, &json!({ "session_id": "s-1", "model": "m" }));
        none(
            event,
            &json!({ "session_id": "s-1", "cursor_version": null }),
        );
        // A Claude payload, whose event names normalize onto cursor's.
        none(
            event,
            &json!({ "session_id": "s-1", "source": "startup", "tool_name": "Bash" }),
        );
    }
}

/// Present-and-non-null is not the gate; a non-empty **string** is.
/// `sessionStart` claims unconditionally, so a foreign payload that
/// happens to carry the key under any other shape would evict the tab's
/// real owner, and no release from that owner could ever match again.
#[test]
fn a_cursor_version_that_is_not_a_string_yields_no_reports() {
    for version in [
        json!(false),
        json!(true),
        json!(0),
        json!(2026),
        json!(""),
        json!([VERSION]),
        json!({ "version": VERSION }),
    ] {
        for event in CURSOR_HOOK_EVENTS {
            none(
                event,
                &json!({ "session_id": "foreign", "cursor_version": version }),
            );
        }
    }
}

/// …and the real thing still passes, read straight off the probe rather
/// than retyped: every record in the fixture carries a usable stamp.
#[test]
fn every_recorded_cursor_version_still_opens_the_gate() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cursor.jsonl");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut seen = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = serde_json::from_str(line).expect("cursor.jsonl parses");
        let version = record["payload"]["cursor_version"].clone();
        assert!(version.is_string(), "{version}");
        let value = json!({ "session_id": "s-1", "cursor_version": version });
        assert!(
            !cursor_event_to_reports("stop", &value, TAB).is_empty(),
            "{line}"
        );
        seen += 1;
    }
    assert_eq!(seen, 16, "the cursor probe recorded 16 usable records");
}

#[test]
fn a_payload_that_is_not_an_object_maps_to_nothing() {
    for raw in [
        json!("a bare string"),
        json!([{ "cursor_version": "v" }]),
        json!(7),
        json!(null),
        json!(true),
    ] {
        for event in CURSOR_HOOK_EVENTS {
            assert!(
                cursor_event_to_reports(event, &raw, TAB).is_empty(),
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
        payload(&json!({ "session_id": 7, "status": ["nested"], "model": {} })),
    ];
    for event in CURSOR_HOOK_EVENTS {
        for value in &payloads {
            for report in cursor_event_to_reports(event, value, TAB) {
                validate_report(&report).unwrap_or_else(|e| panic!("{event} on {value}: {e}"));
            }
        }
    }
}

#[test]
fn every_report_carries_source_cursor_and_the_payload_session_id() {
    let value = payload(&json!({
        "session_id": "s-42",
        "conversation_id": "s-42",
        "tool_name": "Shell",
        "status": "completed",
    }));
    for event in CURSOR_HOOK_EVENTS {
        let report = only(event, &value);
        assert_eq!(report.source, "cursor", "{event}");
        assert_eq!(report.session_id, "s-42", "{event}");
    }
}
