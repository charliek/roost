//! One case per row of plan 046 §3.1's opencode table, plus the
//! malformed-input edges every adapter is required to survive.
//!
//! `question.asked` / `question.replied` and `dispose` were never
//! observed by the probe; they are pinned here by synthetic payloads
//! built from the plan's table, which `opencode.rs`'s module doc also
//! says.

use roost_agent::opencode::{opencode_event_to_reports, OPENCODE_HOOK_EVENTS, SOURCE};
use roost_ipc::agent::{
    validate_report, AgentLifecycle, AttentionOp, OwnershipAction, Severity, TabAgentReportParams,
};
use serde_json::{json, Value};

const TAB: i64 = 7;
const SESSION: &str = "ses_1";

fn only(event: &str, payload: &Value) -> TabAgentReportParams {
    let reports = opencode_event_to_reports(event, payload, TAB);
    assert_eq!(reports.len(), 1, "{event} should map to exactly one report");
    let report = reports.into_iter().next().unwrap();
    assert_eq!(report.tab_id, TAB);
    assert_eq!(report.source, SOURCE);
    validate_report(&report).expect("every emitted report must be valid");
    report
}

fn none(event: &str, payload: &Value) {
    assert!(
        opencode_event_to_reports(event, payload, TAB).is_empty(),
        "{event} should map to no reports"
    );
}

// ---------------------------------------------------------------------
// §3.1 rows
// ---------------------------------------------------------------------

#[test]
fn session_created_claims_and_records_metadata() {
    let report = only(
        "session.created",
        &json!({
            "sessionID": SESSION,
            "info": {
                "id": SESSION,
                "version": "1.18.23",
                "agent": "build",
                "model": { "id": "glm-5.3", "providerID": "zai-coding-plan" },
            },
        }),
    );
    assert_eq!(report.session_id, SESSION);
    assert_eq!(report.ownership_action, OwnershipAction::Claim);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert_eq!(report.attention, AttentionOp::Preserve);
    assert_eq!(report.detail, "session_created");
    assert_eq!(report.metadata["model"], "glm-5.3");
    assert_eq!(report.metadata["agent"], "build");
    assert_eq!(report.metadata["version"], "1.18.23");
}

/// The plugin stamps the root session onto every event it forwards; the
/// bus event's own `sessionID` is the fallback that lets the raw probe
/// log replay unchanged.
#[test]
fn the_plugins_root_session_id_wins_over_the_bus_events_own() {
    let report = only(
        "session.created",
        &json!({ "session_id": "ses_root", "sessionID": SESSION }),
    );
    assert_eq!(report.session_id, "ses_root");

    let report = only("session.idle", &json!({ "sessionID": SESSION }));
    assert_eq!(report.session_id, SESSION);
}

#[test]
fn a_session_created_without_a_session_id_is_dropped() {
    for payload in [
        json!({}),
        json!({ "sessionID": "" }),
        json!({ "sessionID": 12345 }),
    ] {
        none("session.created", &payload);
    }
}

/// A claim supersedes any live owner, so a subagent session must never
/// make one — it would evict the session the user is looking at.
#[test]
fn a_child_session_created_is_dropped() {
    for payload in [
        json!({ "sessionID": "ses_child", "parentID": "ses_root" }),
        json!({ "sessionID": "ses_child", "info": { "parentID": "ses_root" } }),
    ] {
        none("session.created", &payload);
    }
}

#[test]
fn chat_message_works_and_clears_attention() {
    let report = only(
        "chat.message",
        &json!({ "sessionID": SESSION, "agent": "build" }),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Working));
    assert_eq!(report.attention, AttentionOp::Clear);
    assert_eq!(report.detail, "chat_message");
}

#[test]
fn a_busy_session_status_works_and_clears_attention() {
    let report = only(
        "session.status",
        &json!({ "sessionID": SESSION, "status": { "type": "busy" } }),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Working));
    assert_eq!(report.attention, AttentionOp::Clear);
    assert_eq!(report.detail, "session_status");
}

/// `session.status` is a level, not an edge. `session.idle` is what
/// ends a turn; reporting `idle` here as well would finish it twice, and
/// would do so *before* the `session.error` that carries an interrupt.
#[test]
fn every_other_session_status_maps_to_nothing() {
    for status in [
        json!({ "type": "idle" }),
        json!({ "type": "retrying" }),
        json!({}),
        json!("busy"),
        Value::Null,
    ] {
        none(
            "session.status",
            &json!({ "sessionID": SESSION, "status": status }),
        );
    }
    none("session.status", &json!({ "sessionID": SESSION }));
}

#[test]
fn permission_asked_blocks_and_names_the_permission() {
    let report = only(
        "permission.asked",
        &json!({
            "id": "per_1",
            "sessionID": SESSION,
            "permission": "external_directory",
            "metadata": { "command": "touch /tmp/x" },
        }),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Waiting));
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.severity, Severity::Warn);
    assert_eq!(report.title, "OpenCode");
    assert_eq!(report.body, "Needs permission: `external_directory`");
    assert_eq!(report.detail, "permission_asked");
}

#[test]
fn permission_asked_without_a_permission_still_reads_as_a_sentence() {
    let report = only("permission.asked", &json!({ "sessionID": SESSION }));
    assert_eq!(report.body, "Needs permission to continue");
}

/// Not observed in the probe — synthetic, built from opencode's own
/// `QuestionRequest` / `QuestionInfo` types: the text lives in
/// `questions[]`, never at the top level.
#[test]
fn question_asked_blocks_and_carries_the_question() {
    let report = only(
        "question.asked",
        &json!({
            "id": "qst_1",
            "sessionID": SESSION,
            "questions": [{
                "question": "Which branch should I base the change on, main or the release branch?",
                "header": "Which branch?",
                "options": [
                    { "label": "main", "description": "the default branch" },
                    { "label": "release", "description": "the release branch" },
                ],
            }],
        }),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Waiting));
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.severity, Severity::Warn);
    assert_eq!(report.title, "OpenCode");
    // The header, not the complete question: opencode defines it as the
    // short label (max 30 chars) and a banner is one line.
    assert_eq!(report.body, "Which branch?");
    assert_eq!(report.detail, "question_asked");
}

/// A `QuestionInfo` is only required to carry `question`; `header` is
/// what Roost prefers, not what it depends on.
#[test]
fn question_asked_falls_back_through_the_question_to_a_sentence() {
    let report = only(
        "question.asked",
        &json!({ "sessionID": SESSION, "questions": [{ "question": "Proceed?" }] }),
    );
    assert_eq!(report.body, "Proceed?");

    for payload in [
        json!({ "sessionID": SESSION }),
        json!({ "sessionID": SESSION, "questions": [] }),
        json!({ "sessionID": SESSION, "questions": [{}] }),
        json!({ "sessionID": SESSION, "questions": [{ "header": "", "question": "" }] }),
        json!({ "sessionID": SESSION, "questions": "Which branch?" }),
        json!({ "sessionID": SESSION, "questions": [7] }),
        // The pre-SDK guess: a top-level `question` is not a field
        // opencode sends, and reading one would be reading nothing.
        json!({ "sessionID": SESSION, "question": "Which branch?" }),
    ] {
        assert_eq!(
            only("question.asked", &payload).body,
            "Has a question",
            "{payload}"
        );
    }
}

/// A reply resumes the turn but leaves attention alone: the next
/// `chat.message` or busy `session.status` clears it, the same way every
/// other adapter's mid-turn events behave.
#[test]
fn a_reply_resumes_the_turn_without_touching_attention() {
    for (event, detail) in [
        ("permission.replied", "permission_replied"),
        ("question.replied", "question_replied"),
    ] {
        let report = only(
            event,
            &json!({ "sessionID": SESSION, "requestID": "per_1", "reply": "once" }),
        );
        assert_eq!(
            report.ownership_action,
            OwnershipAction::Preserve,
            "{event}"
        );
        assert_eq!(report.lifecycle, Some(AgentLifecycle::Working), "{event}");
        assert_eq!(report.attention, AttentionOp::Preserve, "{event}");
        assert_eq!(report.detail, detail, "{event}");
    }
}

#[test]
fn session_idle_finishes_the_turn() {
    let report = only("session.idle", &json!({ "sessionID": SESSION }));
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
    // Guarded like cursor's `stop`: opencode fires `session.idle` again
    // after an interrupt has already ended the turn, and an interrupt
    // must not banner.
    assert_eq!(
        report.lifecycle_if,
        Some(vec![AgentLifecycle::Working, AgentLifecycle::Waiting])
    );
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.severity, Severity::Info);
    assert_eq!(report.title, "OpenCode");
    assert_eq!(report.body, "Turn complete");
    assert_eq!(report.detail, "session_idle");
}

/// The Esc interrupt arrives on the same channel as a real failure and
/// is the one error name that must not paint the tab red.
#[test]
fn an_aborted_message_finishes_quietly() {
    let report = only(
        "session.error",
        &json!({
            "sessionID": SESSION,
            "error": { "name": "MessageAbortedError", "data": { "message": "Aborted" } },
        }),
    );
    assert_eq!(report.ownership_action, OwnershipAction::Preserve);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Finished));
    assert_eq!(report.attention, AttentionOp::Clear);
    assert_eq!(report.detail, "message_aborted");
}

#[test]
fn any_other_session_error_fails_the_turn() {
    let report = only(
        "session.error",
        &json!({
            "sessionID": SESSION,
            "error": { "name": "ProviderAuthError", "data": { "message": "no credentials" } },
        }),
    );
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Failed));
    assert_eq!(report.attention, AttentionOp::Set);
    assert_eq!(report.severity, Severity::Error);
    assert_eq!(report.title, "OpenCode");
    assert_eq!(report.body, "no credentials");
    assert_eq!(report.detail, "ProviderAuthError");
}

#[test]
fn a_session_error_with_no_detail_degrades_into_the_vocabulary() {
    let report = only("session.error", &json!({ "sessionID": SESSION }));
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Failed));
    assert_eq!(report.body, "Stopped: unknown");
    assert_eq!(report.detail, "unknown");
}

/// The plugin's teardown hook, not a bus event. Declared in opencode's
/// `Hooks` type but never observed by the probe.
#[test]
fn dispose_releases_and_clears() {
    let report = only("dispose", &json!({ "session_id": SESSION }));
    assert_eq!(report.ownership_action, OwnershipAction::Release);
    assert_eq!(report.lifecycle, Some(AgentLifecycle::Inactive));
    assert_eq!(report.attention, AttentionOp::Clear);
}

// ---------------------------------------------------------------------
// Event-name handling
// ---------------------------------------------------------------------

#[test]
fn every_forwarded_event_is_recognized() {
    for event in OPENCODE_HOOK_EVENTS {
        let payload = json!({ "sessionID": SESSION, "status": { "type": "busy" } });
        assert!(
            !opencode_event_to_reports(event, &payload, TAB).is_empty(),
            "{event}"
        );
    }
}

/// The bus events the plugin's whitelist filters out. They are cost,
/// not policy — this adapter drops them too, so a future opencode build
/// that forwards more cannot silently start driving a tab.
#[test]
fn the_bus_events_the_plugin_never_forwards_map_to_nothing() {
    for event in [
        "message.part.delta",
        "message.part.updated",
        "message.updated",
        "session.updated",
        "session.diff",
        "plugin.added",
        "plugin.load",
        "catalog.updated",
        "reference.updated",
        "integration.updated",
        "",
        "🙂",
    ] {
        none(event, &json!({ "sessionID": SESSION }));
    }
}

// ---------------------------------------------------------------------
// Foreign / malformed input
// ---------------------------------------------------------------------

/// opencode needs no marker key: no other probed agent's event name
/// normalizes onto one of its own. `fixture_replay_test.rs` asserts that
/// disjointness against all four event lists; this pins the payload
/// side — even a payload wearing every other agent's discriminator maps
/// through, because the event name is the only thing that decides.
#[test]
fn a_payload_wearing_every_foreign_marker_still_maps_on_the_event_name() {
    let payload = json!({
        "sessionID": SESSION,
        "hookEventName": "x",
        "conversation_id": "c-1",
        "cursor_version": "v",
    });
    assert_eq!(only("session.idle", &payload).session_id, SESSION);
    none("SessionStart", &payload);
    none("Stop", &payload);
}

#[test]
fn a_payload_that_is_not_an_object_maps_to_nothing() {
    for raw in [
        json!("a bare string"),
        json!([{ "sessionID": SESSION }]),
        json!(7),
        json!(null),
        json!(true),
    ] {
        for event in OPENCODE_HOOK_EVENTS {
            assert!(
                opencode_event_to_reports(event, &raw, TAB).is_empty(),
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
        json!({ "sessionID": 7, "info": "not an object", "error": ["nested"] }),
        json!({ "sessionID": SESSION, "status": 7, "permission": {} }),
    ];
    for event in OPENCODE_HOOK_EVENTS {
        for payload in &payloads {
            for report in opencode_event_to_reports(event, payload, TAB) {
                validate_report(&report).unwrap_or_else(|e| panic!("{event} on {payload}: {e}"));
            }
        }
    }
}

#[test]
fn every_report_carries_source_opencode_and_the_root_session_id() {
    let payload = json!({
        "session_id": "ses_root",
        "sessionID": "ses_child",
        "status": { "type": "busy" },
        "permission": "external_directory",
    });
    for event in OPENCODE_HOOK_EVENTS {
        let report = only(event, &payload);
        assert_eq!(report.source, "opencode", "{event}");
        assert_eq!(report.session_id, "ses_root", "{event}");
    }
}
