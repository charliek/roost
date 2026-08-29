//! Host-session wire types (plan 033 §D4): `SessionIdentify`,
//! `EventBatch`, `AttachPayloadKind`, `SESSION_PROTOCOL_VERSION`.
//!
//! No op serves these yet — HS-1 does — so the only thing pinning
//! their shape is this file plus the golden vectors
//! (`tests/ipc-vectors/session.identify.response.json` and
//! `events.batch.json`), which the Swift mirror in
//! `mac/Sources/Roost/IPCMessages.swift` consumes too. The
//! assertions are deliberately byte-exact against literal JSON:
//! a field rename or a reordering that a `round_trip` would happily
//! accept is a cross-language break.

use std::fs;
use std::path::PathBuf;

use roost_ipc::messages::{
    AttachPayloadKind, EventBatch, EventEnvelope, SessionIdentify, SessionIdentifyParams,
    SessionStopParams, SessionStopResult, SESSION_PROTOCOL_VERSION,
};

fn vectors_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(p.pop()); // pop "roost-ipc"
    assert!(p.pop()); // pop "crates"
    p.push("tests");
    p.push("ipc-vectors");
    p
}

fn read_vector(name: &str) -> String {
    let mut path = vectors_dir();
    path.push(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn sample_identify() -> SessionIdentify {
    SessionIdentify {
        app_version: "0.0.18".into(),
        session_protocol: SESSION_PROTOCOL_VERSION,
        payload_kinds: vec![
            AttachPayloadKind::GHOSTTY_SNAPSHOT.into(),
            AttachPayloadKind::VT.into(),
        ],
        libghostty_build: "ghostty-3f6b1c9a4d2e5f80+snapshot.v1".into(),
        session_id: "01K3S8TQ4F0Q9YB2K6WZ5D7XN".into(),
        started_at: "2026-08-27T14:03:11Z".into(),
    }
}

fn sample_batch() -> EventBatch {
    EventBatch {
        revision: 42,
        events: vec![
            EventEnvelope {
                event: "tab.closed".into(),
                data: serde_json::json!({"tab_id": "5"}),
            },
            EventEnvelope {
                event: "project.deleted".into(),
                data: serde_json::json!({"project_id": "1"}),
            },
        ],
    }
}

fn round_trip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
{
    let json = serde_json::to_string(value).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(value, &back, "round-trip mismatch via {json}");
}

#[test]
fn session_protocol_version_is_one() {
    assert_eq!(SESSION_PROTOCOL_VERSION, 1);
    // Distinct constant from the request/response wire version on
    // purpose: they version different things and move independently.
    assert_eq!(roost_ipc::PROTOCOL_VERSION, 1);
}

#[test]
fn attach_payload_kind_round_trips_and_is_a_bare_string() {
    round_trip(&AttachPayloadKind::from(
        AttachPayloadKind::GHOSTTY_SNAPSHOT,
    ));
    round_trip(&AttachPayloadKind::from(AttachPayloadKind::VT));

    let kind = AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT);
    assert_eq!(
        serde_json::to_string(&kind).unwrap(),
        r#""ghostty-snapshot""#
    );
    let decoded: AttachPayloadKind = serde_json::from_str(r#""vt""#).unwrap();
    assert_eq!(decoded.as_str(), "vt");
    assert_eq!(decoded, AttachPayloadKind::from(AttachPayloadKind::VT));
}

/// The whole reason the kind is a newtype over `String` rather than an
/// enum: a client one release behind must be able to read a newer
/// host's `payload_kinds`, keep the values it doesn't recognize, and
/// hand them back unchanged.
#[test]
fn unknown_attach_payload_kind_survives_a_round_trip() {
    let decoded: AttachPayloadKind = serde_json::from_str(r#""sixel-mosaic-v9""#).unwrap();
    assert_eq!(decoded.as_str(), "sixel-mosaic-v9");
    assert_eq!(
        serde_json::to_string(&decoded).unwrap(),
        r#""sixel-mosaic-v9""#
    );

    let identify: SessionIdentify = serde_json::from_str(
        r#"{"app_version":"9.9.9","session_protocol":7,
            "payload_kinds":["ghostty-snapshot","sixel-mosaic-v9"],
            "libghostty_build":"future","session_id":"s","started_at":"t"}"#,
    )
    .unwrap();
    assert_eq!(
        identify.payload_kinds,
        vec![
            AttachPayloadKind::from("ghostty-snapshot"),
            AttachPayloadKind::from("sixel-mosaic-v9"),
        ]
    );
    round_trip(&identify);
}

#[test]
fn session_identify_matches_its_golden_json() {
    const GOLDEN: &str = concat!(
        r#"{"app_version":"0.0.18","session_protocol":1,"#,
        r#""payload_kinds":["ghostty-snapshot","vt"],"#,
        r#""libghostty_build":"ghostty-3f6b1c9a4d2e5f80+snapshot.v1","#,
        r#""session_id":"01K3S8TQ4F0Q9YB2K6WZ5D7XN","#,
        r#""started_at":"2026-08-27T14:03:11Z"}"#,
    );

    let value = sample_identify();
    round_trip(&value);
    assert_eq!(serde_json::to_string(&value).unwrap(), GOLDEN);
    let decoded: SessionIdentify = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn event_batch_matches_its_golden_json() {
    const GOLDEN: &str = concat!(
        r#"{"revision":42,"events":["#,
        r#"{"event":"tab.closed","data":{"tab_id":"5"}},"#,
        r#"{"event":"project.deleted","data":{"project_id":"1"}}"#,
        r#"]}"#,
    );

    let value = sample_batch();
    round_trip(&value);
    assert_eq!(serde_json::to_string(&value).unwrap(), GOLDEN);
    let decoded: EventBatch = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(decoded, value);
}

/// A batch is the unit of loss detection, so the envelopes inside it
/// must arrive in the order the server published them — a set would
/// let a `tab.closed` land before the `tab.opened` that created it.
#[test]
fn event_batch_preserves_event_order() {
    let batch = EventBatch {
        revision: 9,
        events: (0..8)
            .map(|i| EventEnvelope {
                event: format!("evt.{i}"),
                data: serde_json::json!({ "i": i }),
            })
            .collect(),
    };
    let json = serde_json::to_string(&batch).unwrap();
    let back: EventBatch = serde_json::from_str(&json).unwrap();
    let names: Vec<&str> = back.events.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(
        names,
        vec!["evt.0", "evt.1", "evt.2", "evt.3", "evt.4", "evt.5", "evt.6", "evt.7"]
    );
    assert_eq!(back, batch);
}

/// Both types are read by clients, so an older client must survive a
/// newer host adding fields (the same permissive-response rule the
/// rest of the module follows).
#[test]
fn unknown_fields_are_tolerated_on_decode() {
    let identify: SessionIdentify = serde_json::from_str(
        r#"{"app_version":"0.0.18","session_protocol":1,"payload_kinds":["vt"],
            "libghostty_build":"b","session_id":"s","started_at":"t",
            "capabilities":["mosh"],"future_field":1}"#,
    )
    .unwrap();
    assert_eq!(identify.payload_kinds, vec![AttachPayloadKind::from("vt")]);

    let batch: EventBatch = serde_json::from_str(
        r#"{"revision":3,"events":[{"event":"tab.closed","data":{},"seq":11}],"dropped":false}"#,
    )
    .unwrap();
    assert_eq!(batch.revision, 3);
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].event, "tab.closed");

    // An empty batch is legal and defaults its event list, so a host
    // that publishes a bare revision fence still decodes.
    let fence: EventBatch = serde_json::from_str(r#"{"revision":4}"#).unwrap();
    assert_eq!(fence.revision, 4);
    assert!(fence.events.is_empty());
}

#[test]
fn session_identify_vector_decodes_into_its_typed_result() {
    let raw = read_vector("session.identify.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: SessionIdentify =
        serde_json::from_value(resp.result.expect("result body")).expect("decode session identify");
    assert_eq!(result, sample_identify());
    assert_eq!(result.session_protocol, SESSION_PROTOCOL_VERSION);
}

fn sample_stop_report() -> SessionStopResult {
    SessionStopResult {
        reaped: vec![3, 5],
        killed: vec![8],
        // Past 2^53: the reason every id is string-encoded.
        abandoned: vec![9_007_199_254_740_993],
    }
}

#[test]
fn session_stop_result_matches_its_golden_json() {
    const GOLDEN: &str = concat!(
        r#"{"reaped":["3","5"],"killed":["8"],"#,
        r#""abandoned":["9007199254740993"]}"#,
    );

    let value = sample_stop_report();
    round_trip(&value);
    assert_eq!(serde_json::to_string(&value).unwrap(), GOLDEN);
    let decoded: SessionStopResult = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(decoded, value);
}

/// Both session ops take no params today. They are still typed structs
/// so an unknown field is a decode error rather than a silently ignored
/// option — the same contract every other op's params have.
#[test]
fn session_params_are_empty_and_reject_unknown_fields() {
    assert_eq!(
        serde_json::to_string(&SessionIdentifyParams {}).unwrap(),
        "{}"
    );
    assert_eq!(serde_json::to_string(&SessionStopParams {}).unwrap(), "{}");
    serde_json::from_str::<SessionIdentifyParams>("{}").unwrap();
    serde_json::from_str::<SessionStopParams>("{}").unwrap();
    assert!(serde_json::from_str::<SessionStopParams>(r#"{"force":true}"#).is_err());
}

#[test]
fn session_stop_vector_decodes_into_its_typed_result() {
    let raw = read_vector("session.stop.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: SessionStopResult =
        serde_json::from_value(resp.result.expect("result body")).expect("decode stop report");
    assert_eq!(result, sample_stop_report());
}

#[test]
fn event_batch_vector_decodes_into_its_typed_shape() {
    let raw = read_vector("events.batch.json");
    let batch: EventBatch = serde_json::from_str(&raw).expect("decode event batch");
    assert_eq!(batch.revision, 42);
    let names: Vec<&str> = batch.events.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(names, vec!["tab.opened", "active.changed"]);

    // The envelopes inside a batch are the same ones the standalone
    // event vectors carry, so their `data` must decode into the same
    // typed events.
    let opened: roost_ipc::messages::TabOpenedEvent =
        serde_json::from_value(batch.events[0].data.clone()).expect("decode tab.opened data");
    assert_eq!(opened.tab.id, 5);
    let active: roost_ipc::messages::ActiveChangedEvent =
        serde_json::from_value(batch.events[1].data.clone()).expect("decode active.changed data");
    assert_eq!(active.project_id, 1);
    assert_eq!(active.tab_id, 5);
}
