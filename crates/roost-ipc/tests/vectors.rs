//! Golden-vector loader. Walks `tests/ipc-vectors/*.json` at the
//! workspace root and asserts each file:
//!
//! * Parses as valid JSON (any shape).
//! * Round-trips via `serde_json::Value` (decode → re-encode →
//!   semantically equal).
//!
//! The Swift companion test (added in M4 with the XCTest target)
//! will load the same files and assert the same invariants. Drift
//! between Rust and Swift surfaces immediately because both sides
//! consume the *same* fixture bytes.
//!
//! This file deliberately stays schema-agnostic (it doesn't decode
//! into typed structs) so adding a new vector file doesn't require
//! touching test code. Typed-decode coverage lives in
//! `tests/roundtrip.rs`.

use std::fs;
use std::path::{Path, PathBuf};

fn vectors_dir() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/roost-ipc`; walk up two levels
    // to reach the workspace root, then descend into tests/ipc-vectors.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(p.pop()); // pop "roost-ipc"
    assert!(p.pop()); // pop "crates"
    p.push("tests");
    p.push("ipc-vectors");
    p
}

fn collect_vectors(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for entry in fs::read_dir(dir).expect("read ipc-vectors") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn vectors_directory_is_non_empty() {
    let dir = vectors_dir();
    let v = collect_vectors(&dir);
    assert!(
        !v.is_empty(),
        "no JSON vectors found in {} — did you delete them?",
        dir.display()
    );
}

#[test]
fn every_vector_round_trips_through_serde_json() {
    let dir = vectors_dir();
    let vectors = collect_vectors(&dir);
    let mut errors: Vec<String> = vec![];
    for path in &vectors {
        let raw =
            fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let value: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{}: parse: {e}", path.display()));
                continue;
            }
        };
        // Actually exercise the serializer: encode the parsed Value
        // back to compact JSON (the wire form), parse it again, and
        // assert the two parses are semantically equal. This catches
        // any value that round-trips lossy through the serializer
        // (e.g. NaN/Infinity, which serde_json rejects at encode
        // time). Byte-equal vs. the source file would require the
        // fixtures to be in the canonical compact wire form, but
        // they're intentionally pretty-printed for human readers —
        // semantic equality is the meaningful contract for the IPC.
        let encoded = match serde_json::to_string(&value) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("{}: re-encode: {e}", path.display()));
                continue;
            }
        };
        let reparsed: serde_json::Value = match serde_json::from_str(&encoded) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{}: re-parse after encode: {e}", path.display()));
                continue;
            }
        };
        if reparsed != value {
            errors.push(format!("{}: decode→encode→decode drift", path.display()));
        }
    }
    if !errors.is_empty() {
        panic!("vector failures:\n{}", errors.join("\n"));
    }
}

/// Each request file must declare an `id` (string-wrapped int64) and
/// an `op` (dotted-lowercase string). Lightweight schema check that
/// catches accidental copy-paste between fixtures.
#[test]
fn request_vectors_have_required_envelope_shape() {
    let dir = vectors_dir();
    for path in collect_vectors(&dir) {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !stem.ends_with(".request") {
            continue;
        }
        let raw =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: read: {e}", path.display()));
        let v: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: parse: {e}", path.display()));
        let obj = v
            .as_object()
            .unwrap_or_else(|| panic!("{}: not a JSON object", path.display()));
        assert!(
            obj.get("id").map(|v| v.is_string()).unwrap_or(false),
            "{}: missing string `id`",
            path.display()
        );
        assert!(
            obj.get("op").map(|v| v.is_string()).unwrap_or(false),
            "{}: missing string `op`",
            path.display()
        );
    }
}

/// `tab.agent_report` is the one op whose params carry a state machine
/// rather than a flat payload, so its vector is also checked against the
/// typed struct: a vector that only round-trips as a `Value` could still
/// be missing `ownership_action` or spell `lifecycle` wrong and nothing
/// would notice until an adapter shipped. The rest of the corpus stays
/// deliberately schema-agnostic (see the module docs).
#[test]
fn agent_report_vector_decodes_into_its_typed_params() {
    use roost_ipc::agent::{AgentLifecycle, AttentionOp, OwnershipAction, TabAgentReportParams};
    use roost_ipc::messages::{ops, RawRequest, TabAgentReportResult};

    let mut path = vectors_dir();
    path.push("tab.agent_report.request.json");
    let raw = fs::read_to_string(&path).expect("read request vector");
    let req: RawRequest = serde_json::from_str(&raw).expect("decode envelope");
    assert_eq!(req.op, ops::TAB_AGENT_REPORT);
    let params: TabAgentReportParams =
        serde_json::from_value(req.params).expect("decode agent_report params");
    assert_eq!(params.ownership_action, OwnershipAction::Preserve);
    assert_eq!(params.lifecycle, Some(AgentLifecycle::Waiting));
    assert_eq!(params.attention, AttentionOp::Set);
    assert!(roost_ipc::agent::validate_report(&params).is_ok());

    let mut path = vectors_dir();
    path.push("tab.agent_report.response.json");
    let raw = fs::read_to_string(&path).expect("read response vector");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    let result: TabAgentReportResult =
        serde_json::from_value(resp.result.expect("result body")).expect("decode result");
    assert!(result.accepted);
    // The wire `state` / `hook_active` must be what the axes derive to,
    // or the vector documents a state the server cannot produce.
    let agent = result.tab.agent_state();
    assert_eq!(result.tab.state, roost_ipc::agent::effective(&agent));
    assert_eq!(result.tab.hook_active, roost_ipc::agent::is_live(&agent));
}

/// `app.sidebar_dump` is the newest read-only UI-state op; generic
/// vector round-tripping (above) only proves the fixture is valid
/// JSON, not that it matches `SidebarDumpResult`'s field names/types
/// (string ids, `lifecycle` spelling, nesting). Decode it into the
/// typed struct so a schema drift fails here, not in an adapter.
#[test]
fn sidebar_dump_vector_decodes_into_its_typed_params() {
    use roost_ipc::agent::AgentLifecycle;
    use roost_ipc::messages::{ops, RawRequest, SidebarDumpParams, SidebarDumpResult};

    let mut path = vectors_dir();
    path.push("app.sidebar_dump.request.json");
    let raw = fs::read_to_string(&path).expect("read request vector");
    let req: RawRequest = serde_json::from_str(&raw).expect("decode envelope");
    assert_eq!(req.op, ops::SIDEBAR_DUMP);
    let _params: SidebarDumpParams =
        serde_json::from_value(req.params).expect("decode sidebar_dump params");

    let mut path = vectors_dir();
    path.push("app.sidebar_dump.response.json");
    let raw = fs::read_to_string(&path).expect("read response vector");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    let result: SidebarDumpResult =
        serde_json::from_value(resp.result.expect("result body")).expect("decode result");

    assert!(result.agents_visible);
    assert_eq!(result.projects.len(), 2);
    assert_eq!(result.projects[0].project_id, 1);
    assert_eq!(result.projects[0].agents.len(), 1);
    let row = &result.projects[0].agents[0];
    assert_eq!(row.tab_id, 7);
    assert_eq!(row.name, "slauth-refactor");
    assert_eq!(row.lifecycle, AgentLifecycle::Waiting);
    assert_eq!(row.status_text, "Waiting for input");
    assert_eq!(row.time_text, "2m");
    assert!(!row.is_active);
    assert_eq!(result.projects[1].project_id, 2);
    assert!(result.projects[1].agents.is_empty());
}

/// `app.render_stats` is the one op whose *every* result field is a
/// string-wrapped int64. Generic round-tripping would happily accept a
/// vector that wrote them as JSON numbers, which is exactly the drift
/// this convention exists to prevent — so decode it into the typed
/// struct.
#[test]
fn render_stats_vector_decodes_into_its_typed_params() {
    use roost_ipc::messages::{ops, AppRenderStatsParams, AppRenderStatsResult, RawRequest};

    let mut path = vectors_dir();
    path.push("app.render_stats.request.json");
    let raw = fs::read_to_string(&path).expect("read request vector");
    let req: RawRequest = serde_json::from_str(&raw).expect("decode envelope");
    assert_eq!(req.op, ops::APP_RENDER_STATS);
    let params: AppRenderStatsParams =
        serde_json::from_value(req.params).expect("decode render_stats params");
    assert!(!params.reset);

    let mut path = vectors_dir();
    path.push("app.render_stats.response.json");
    let raw = fs::read_to_string(&path).expect("read response vector");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    let result: AppRenderStatsResult =
        serde_json::from_value(resp.result.expect("result body")).expect("decode result");
    assert_eq!(result.refresh_calls, 412);
    assert_eq!(result.refresh_nanos, 51_500_000);
    assert_eq!(result.rows_rebuilt, 9_888);
    assert_eq!(result.cells_walked, 790_400);
    assert_eq!(result.draw_calls, 377);
    assert_eq!(result.draw_nanos, 94_250_000);
    assert_eq!(result.fill_text_calls, 9_048);
    // The fixture deliberately omits view_*/elide_* — it models the mac
    // Swift handler's response, which doesn't send them — so decoding it
    // must default those fields to 0 rather than fail.
    assert_eq!(result.view_calls, 0);
    assert_eq!(result.view_nanos, 0);
    assert_eq!(result.elide_calls, 0);
    assert_eq!(result.elide_nanos, 0);
}

#[test]
fn event_vectors_have_required_envelope_shape() {
    let dir = vectors_dir();
    for path in collect_vectors(&dir) {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !stem.ends_with(".event") {
            continue;
        }
        let raw =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: read: {e}", path.display()));
        let v: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: parse: {e}", path.display()));
        let obj = v
            .as_object()
            .unwrap_or_else(|| panic!("{}: not a JSON object", path.display()));
        assert!(
            obj.get("event").map(|v| v.is_string()).unwrap_or(false),
            "{}: missing string `event`",
            path.display()
        );
        assert!(
            obj.contains_key("data"),
            "{}: missing `data` field",
            path.display()
        );
        assert!(
            !obj.contains_key("id"),
            "{}: events must not carry an `id`",
            path.display()
        );
    }
}

/// The `events.subscribe` ack and the two `tab.list` shapes it fences.
///
/// Three things generic round-tripping cannot see: `revision` is a JSON
/// *number* on both (revisions are counters, not ids, so the
/// string-int64 convention deliberately does not apply); the UI-socket
/// vector has no `revision` key at all rather than a `null`; and the
/// session-socket vector decodes into the same `TabListResult` an older
/// client would use.
#[test]
fn the_revision_fence_vectors_decode_into_their_typed_results() {
    use roost_ipc::messages::{EventsSubscribeResult, Response, TabListResult};

    let mut path = vectors_dir();
    path.push("events.subscribe.response.json");
    let raw = fs::read_to_string(&path).expect("read events.subscribe vector");
    let resp: Response = serde_json::from_str(&raw).expect("decode response envelope");
    let body = resp.result.expect("result body");
    assert!(body["revision"].is_u64(), "revision must be a JSON number");
    let ack: EventsSubscribeResult = serde_json::from_value(body).expect("decode ack");
    assert_eq!(ack.revision, 42);

    let mut path = vectors_dir();
    path.push("tab.list.session.response.json");
    let raw = fs::read_to_string(&path).expect("read session tab.list vector");
    let resp: Response = serde_json::from_str(&raw).expect("decode response envelope");
    let fenced: TabListResult =
        serde_json::from_value(resp.result.expect("result body")).expect("decode result");
    assert_eq!(fenced.revision, Some(42));
    assert_eq!(fenced.projects.len(), 1);

    let mut path = vectors_dir();
    path.push("tab.list.response.json");
    let raw = fs::read_to_string(&path).expect("read UI tab.list vector");
    let resp: Response = serde_json::from_str(&raw).expect("decode response envelope");
    let body = resp.result.expect("result body");
    assert!(
        !body
            .as_object()
            .expect("result object")
            .contains_key("revision"),
        "a UI socket's tab.list must not carry the key at all"
    );
    let plain: TabListResult = serde_json::from_value(body).expect("decode result");
    assert_eq!(plain.revision, None);
    // Re-encoding an unfenced result must not invent the key back.
    let re = serde_json::to_value(&plain).expect("re-encode");
    assert!(!re.as_object().unwrap().contains_key("revision"));
}

/// The two reorder events joined the wire catalog with the push
/// implementation (plan 035 C4). Their ids are string-encoded *inside a
/// list*, which is the encoding a JSON client is most likely to get
/// wrong — a bare `Number` there rounds any id past 2^53.
#[test]
fn the_reorder_event_vectors_keep_string_encoded_id_lists() {
    use roost_ipc::messages::{ops, EventEnvelope};

    let mut path = vectors_dir();
    path.push("tabs.reordered.event.json");
    let raw = fs::read_to_string(&path).expect("read tabs.reordered vector");
    let ev: EventEnvelope = serde_json::from_str(&raw).expect("decode envelope");
    assert_eq!(ev.event, ops::EVENT_TABS_REORDERED);
    assert_eq!(ev.data["project_id"], "1");
    assert_eq!(
        ev.data["tab_ids"],
        serde_json::json!(["7", "5", "9007199254740993"])
    );

    let mut path = vectors_dir();
    path.push("projects.reordered.event.json");
    let raw = fs::read_to_string(&path).expect("read projects.reordered vector");
    let ev: EventEnvelope = serde_json::from_str(&raw).expect("decode envelope");
    assert_eq!(ev.event, ops::EVENT_PROJECTS_REORDERED);
    assert_eq!(ev.data["project_ids"], serde_json::json!(["2", "1"]));
}
