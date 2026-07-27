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
