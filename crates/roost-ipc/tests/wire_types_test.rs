//! Host-session wire types (plan 033 §D4, plan 036 §D11):
//! `SessionIdentify`, `EventBatch`, `AttachPayloadKind`, the
//! `session.connect` / `tab.attach` shapes, the attach handshake and
//! its reply, and `SESSION_PROTOCOL_VERSION`.
//!
//! What pins their shape is this file plus the golden vectors under
//! `tests/ipc-vectors/` (the identify vector per generation, see
//! `identify_vector_name`), which the Swift mirror in
//! `mac/Sources/Roost/IPCMessages.swift` consumes too. The
//! assertions are deliberately byte-exact against literal JSON:
//! a field rename or a reordering that a `round_trip` would happily
//! accept is a cross-language break. These fixtures are a
//! compatibility contract — never edit an existing vector to bless a
//! wire change (additive changes add new vectors); see
//! `docs/reference/ipc-compatibility.md`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use roost_ipc::messages::{
    ops, AgentHooksMode, AttachAccepted, AttachHandshake, AttachHandshakeReply, AttachMode,
    AttachPayloadKind, ClipboardEffectTarget, ClipboardWriteParams, EventBatch, EventEnvelope,
    EventsSubscribeParams, ProjectReorderParams, ResponseError, RetrySchedule, SentFile,
    SessionBinaryIdentity, SessionConnectParams, SessionConnectResult, SessionDriverChangedEvent,
    SessionIdentify, SessionIdentifyParams, SessionPutFileParams, SessionPutFileResult,
    SessionSetAgentHooksParams, SessionSetAgentHooksResult, SessionSetFocusParams,
    SessionSetThemeParams, SessionSetThemeResult, SessionStopParams, SessionStopResult,
    SessionStoppingEvent, SkippedFile, TabAttachParams, TabAttachResult, TabEffect, TabEffectEvent,
    TabReorderParams, TabSendFileParams, TabSendFileResult, TabWriteParams, WireProjectRef,
    WireTabRef, MAX_PUT_FILE_BYTES, SESSION_DRIVER_CHANGED_EVENT, SESSION_FEATURES,
    SESSION_PROTOCOL_VERSION, SESSION_STOPPING_EVENT,
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
        features: SESSION_FEATURES.iter().map(|f| (*f).to_string()).collect(),
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
    assert_eq!(
        value, &back,
        "round-trip mismatch via {json} — these vectors are a compatibility \
         contract, never edit an existing one to bless a wire change (additive \
         changes add new vectors); see docs/reference/ipc-compatibility.md"
    );
}

/// Plan 049 R1's bump: the lease changed axis (leaseless classified
/// `events.subscribe`, lease-gated session-socket `tab.write`,
/// non-terminal `session.driver_changed`), which no `3` peer can be
/// served. The request/response wire version did not move with it — the
/// two version different things.
#[test]
fn session_protocol_version_is_four() {
    assert_eq!(SESSION_PROTOCOL_VERSION, 4);
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
        r#"{"app_version":"0.0.18","session_protocol":4,"#,
        r#""payload_kinds":["ghostty-snapshot","vt"],"#,
        r#""features":["put_file","events_resume"],"#,
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

/// `roost-session identify`'s wire shape (plan 039 §3.1) — a binary's
/// offline identity, not a running session's. Distinct from
/// `SessionIdentify` above: three required fields, no `payload_kinds` /
/// `session_id` / `started_at`.
#[test]
fn session_binary_identity_matches_its_golden_json() {
    const GOLDEN: &str = concat!(
        r#"{"app_version":"0.0.19","session_protocol":4,"#,
        r#""libghostty_build":"ghostty-abcdef0123456789+snapshot.v1"}"#,
    );

    let value = SessionBinaryIdentity {
        app_version: "0.0.19".into(),
        session_protocol: SESSION_PROTOCOL_VERSION,
        libghostty_build: "ghostty-abcdef0123456789+snapshot.v1".into(),
    };
    round_trip(&value);
    assert_eq!(serde_json::to_string(&value).unwrap(), GOLDEN);
    let decoded: SessionBinaryIdentity = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(decoded, value);

    // A JSON-value key assertion, not just the golden-string compare
    // above: an accidental extra `#[serde]` field still produces valid
    // JSON that a looser check could miss.
    let as_value = serde_json::to_value(&value).unwrap();
    let mut keys: Vec<&str> = as_value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["app_version", "libghostty_build", "session_protocol"]
    );
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

/// `session.identify`'s response embeds the protocol integer, so its
/// vector is versioned per generation (`docs/reference/ipc-compatibility.md`,
/// "Generation-bearing vectors are versioned"). The filename is built from
/// the constant, never spelled out: a bump that forgets to add the new
/// generation's vector fails here instead of silently testing the old one.
fn identify_vector_name(generation: u32) -> String {
    format!("session.identify.response.v{generation}.json")
}

fn decode_identify_vector(name: &str) -> SessionIdentify {
    let raw = read_vector(name);
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok, "{name}: not an ok response");
    serde_json::from_value(resp.result.expect("result body"))
        .unwrap_or_else(|e| panic!("{name}: decode session identify: {e}"))
}

/// A duplicate-free view of a feature list, or `None` if it repeats
/// itself — a list compared as a set has to *be* one.
fn feature_set<'a>(features: impl IntoIterator<Item = &'a str>) -> Option<BTreeSet<&'a str>> {
    let mut set = BTreeSet::new();
    for feature in features {
        if !set.insert(feature) {
            return None;
        }
    }
    Some(set)
}

/// The vector is frozen, so this proves it is a **valid** view of this
/// generation, not the *current* one: every field matches except
/// `features`, which is only asserted to be a subset of what this build
/// advertises.
///
/// That is the executable form of the policy in
/// `docs/reference/ipc-compatibility.md` — features are additive and
/// monotonic within a generation, so the generation's vector pins the
/// floor and a build that has grown one since is still serving exactly
/// this shape. A capability that *disappeared* would fail here, which is
/// the half that matters: removing one takes a
/// `SESSION_PROTOCOL_VERSION` bump and a new vector.
#[test]
fn session_identify_vector_decodes_into_its_typed_result() {
    let result = decode_identify_vector(&identify_vector_name(SESSION_PROTOCOL_VERSION));
    assert_eq!(result.session_protocol, SESSION_PROTOCOL_VERSION);
    assert_eq!(
        result,
        SessionIdentify {
            features: result.features.clone(),
            ..sample_identify()
        },
        "only `features` may differ from this build's identity"
    );

    let vector = feature_set(result.features.iter().map(String::as_str))
        .expect("the vector's features repeat");
    let current = feature_set(SESSION_FEATURES.iter().copied()).expect("SESSION_FEATURES repeats");
    assert!(
        vector.is_subset(&current),
        "the v{SESSION_PROTOCOL_VERSION} vector advertises {:?}, which this build no longer \
         does — removing a feature is a generation bump, not an edit",
        vector.difference(&current).collect::<Vec<_>>()
    );
}

/// Every prior generation's vector stays on disk and keeps decoding —
/// the compatibility policy's "an old client's view still parses",
/// as a test rather than a claim. `2` is the first versioned
/// generation (the corpus was backfilled at the `2` → `3` bump).
#[test]
fn every_prior_session_identify_generation_still_decodes() {
    const FIRST_VERSIONED: u32 = 2;
    // Compile-time: the loop below must have something to walk.
    const { assert!(SESSION_PROTOCOL_VERSION > FIRST_VERSIONED) };
    for generation in FIRST_VERSIONED..SESSION_PROTOCOL_VERSION {
        let result = decode_identify_vector(&identify_vector_name(generation));
        assert_eq!(result.session_protocol, generation);
        assert_ne!(result.session_protocol, SESSION_PROTOCOL_VERSION);
    }
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

// ============================================================================
// Leases + attach (plan 036 §D4/D5/D6)
// ============================================================================

const LEASE: &str = "9f2c1d7a4b6e08315c0d9a72e4f16b83";
const TOKEN: &str = "1a0be5c37d924f68b1c05e3a7f2d8496";
const EPOCH: u64 = 6_032_428_321_756_423_947;

fn sample_attach_params() -> TabAttachParams {
    TabAttachParams {
        lease: LEASE.into(),
        tab_id: 5,
        kinds: vec![AttachPayloadKind::GHOSTTY_SNAPSHOT.into()],
        cols: 120,
        rows: 40,
        cell_w_px: 9,
        cell_h_px: 18,
        libghostty_build: "ghostty-3f6b1c9a4d2e5f80+snapshot.v1".into(),
    }
}

fn sample_attach_result() -> TabAttachResult {
    TabAttachResult {
        attach_token: TOKEN.into(),
        kind: AttachPayloadKind::GHOSTTY_SNAPSHOT.into(),
        server_epoch: EPOCH,
        tab_generation: 3,
    }
}

#[test]
fn session_connect_params_default_to_no_takeover() {
    assert_eq!(
        serde_json::to_string(&SessionConnectParams::default()).unwrap(),
        r#"{"takeover":false}"#
    );
    // Absent is false: the safe answer, since a takeover kicks whoever
    // is connected.
    let decoded: SessionConnectParams = serde_json::from_str("{}").unwrap();
    assert!(!decoded.takeover);
    let decoded: SessionConnectParams = serde_json::from_str(r#"{"takeover":true}"#).unwrap();
    assert!(decoded.takeover);
    round_trip(&decoded);
    // Params are strict, like every other op's.
    assert!(serde_json::from_str::<SessionConnectParams>(r#"{"force":true}"#).is_err());
}

/// The label rides `session.connect`, not `identify`, and is omitted
/// unless the client actually states one — an older session
/// `deny_unknown_fields`-rejects a key it has never heard of, so an
/// unlabeled connect must stay byte-identical to what it always sent.
#[test]
fn session_connect_params_omit_an_unset_client_label() {
    let bare = SessionConnectParams {
        takeover: true,
        client_label: None,
    };
    assert_eq!(
        serde_json::to_string(&bare).unwrap(),
        r#"{"takeover":true}"#
    );

    let labeled = SessionConnectParams {
        takeover: true,
        client_label: Some("kestrel.local".into()),
    };
    assert_eq!(
        serde_json::to_string(&labeled).unwrap(),
        r#"{"takeover":true,"client_label":"kestrel.local"}"#
    );
    round_trip(&labeled);

    let decoded: SessionConnectParams = serde_json::from_str(r#"{"takeover":false}"#).unwrap();
    assert_eq!(decoded.client_label, None);
}

/// Same omit-when-unset contract on the write path, and the reason is
/// the sharper one: a lease-less `roostctl` must keep talking to a UI
/// socket that predates the key entirely.
#[test]
fn tab_write_params_omit_an_unset_lease() {
    let bare = TabWriteParams {
        tab_id: 5,
        data: b"ls\n".to_vec(),
        lease: None,
    };
    assert_eq!(
        serde_json::to_string(&bare).unwrap(),
        r#"{"tab_id":"5","data":"bHMK"}"#
    );

    let leased = TabWriteParams {
        tab_id: 5,
        data: b"ls\n".to_vec(),
        lease: Some(LEASE.into()),
    };
    assert_eq!(
        serde_json::to_string(&leased).unwrap(),
        format!(r#"{{"tab_id":"5","data":"bHMK","lease":"{LEASE}"}}"#)
    );
    round_trip(&leased);

    let decoded: TabWriteParams = serde_json::from_str(r#"{"tab_id":"5","data":"bHMK"}"#).unwrap();
    assert_eq!(decoded.lease, None);
}

#[test]
fn session_connect_result_matches_its_golden_json() {
    const GOLDEN: &str = r#"{"lease":"9f2c1d7a4b6e08315c0d9a72e4f16b83","revision":42}"#;

    let value = SessionConnectResult {
        lease: LEASE.into(),
        revision: 42,
    };
    round_trip(&value);
    assert_eq!(serde_json::to_string(&value).unwrap(), GOLDEN);
    let decoded: SessionConnectResult = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn tab_attach_params_match_their_golden_json() {
    const GOLDEN: &str = concat!(
        r#"{"lease":"9f2c1d7a4b6e08315c0d9a72e4f16b83","tab_id":"5","#,
        r#""kinds":["ghostty-snapshot"],"cols":120,"rows":40,"#,
        r#""cell_w_px":9,"cell_h_px":18,"#,
        r#""libghostty_build":"ghostty-3f6b1c9a4d2e5f80+snapshot.v1"}"#,
    );

    let value = sample_attach_params();
    round_trip(&value);
    assert_eq!(serde_json::to_string(&value).unwrap(), GOLDEN);
    let decoded: TabAttachParams = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(decoded, value);
}

/// A headless client has no cell metrics to report, so the pixel
/// geometry defaults away — but `cols`/`rows` do not, because a zero
/// viewport is not a thing a tab can be resized to.
#[test]
fn tab_attach_params_default_the_pixel_geometry_only() {
    let decoded: TabAttachParams = serde_json::from_str(
        r#"{"lease":"l","tab_id":"7","kinds":["vt"],"cols":80,"rows":24,
            "libghostty_build":"b"}"#,
    )
    .unwrap();
    assert_eq!((decoded.cell_w_px, decoded.cell_h_px), (0, 0));
    assert_eq!(decoded.tab_id, 7);

    assert!(serde_json::from_str::<TabAttachParams>(
        r#"{"lease":"l","tab_id":"7","kinds":[],"rows":24,"libghostty_build":"b"}"#
    )
    .is_err());
}

#[test]
fn tab_attach_result_matches_its_golden_json() {
    const GOLDEN: &str = concat!(
        r#"{"attach_token":"1a0be5c37d924f68b1c05e3a7f2d8496","#,
        r#""kind":"ghostty-snapshot","server_epoch":6032428321756423947,"#,
        r#""tab_generation":3}"#,
    );

    let value = sample_attach_result();
    round_trip(&value);
    assert_eq!(serde_json::to_string(&value).unwrap(), GOLDEN);
    let decoded: TabAttachResult = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(decoded, value);
    // Past 2^53 and a bare JSON number, like every other counter on
    // this wire (`EventBatch.revision`), not a string-encoded id — so
    // the golden above is also a precision guard.
    const _: () = assert!(EPOCH > (1u64 << 53));
}

#[test]
fn attach_handshake_matches_its_golden_json() {
    const SNAPSHOT: &str = r#"{"attach":"1a0be5c37d924f68b1c05e3a7f2d8496","protocol_version":4}"#;
    const RESUME: &str = concat!(
        r#"{"attach":"1a0be5c37d924f68b1c05e3a7f2d8496","protocol_version":4,"#,
        r#""resume_from_seq":901,"server_epoch":6032428321756423947,"#,
        r#""tab_generation":3}"#,
    );

    let fresh = AttachHandshake {
        attach: TOKEN.into(),
        protocol_version: SESSION_PROTOCOL_VERSION,
        ..AttachHandshake::default()
    };
    round_trip(&fresh);
    assert_eq!(serde_json::to_string(&fresh).unwrap(), SNAPSHOT);

    let resuming = AttachHandshake {
        attach: TOKEN.into(),
        protocol_version: SESSION_PROTOCOL_VERSION,
        resume_from_seq: Some(901),
        server_epoch: Some(EPOCH),
        tab_generation: Some(3),
    };
    round_trip(&resuming);
    assert_eq!(serde_json::to_string(&resuming).unwrap(), RESUME);
    assert_eq!(
        serde_json::from_str::<AttachHandshake>(RESUME).unwrap(),
        resuming
    );
}

/// The handshake is the one line a client of a *newer* build might send
/// with fields this build has never heard of; refusing it over one
/// would turn an additive change into a hard incompatibility.
#[test]
fn attach_handshake_tolerates_unknown_fields() {
    let decoded: AttachHandshake = serde_json::from_str(
        r#"{"attach":"t","protocol_version":2,"resume_from_seq":5,
            "viewport_hint":{"top":0},"future_field":true}"#,
    )
    .unwrap();
    assert_eq!(decoded.attach, "t");
    assert_eq!(decoded.resume_from_seq, Some(5));
    assert_eq!(decoded.server_epoch, None);
}

#[test]
fn attach_handshake_reply_matches_its_golden_json_on_both_arms() {
    const ACCEPTED: &str = concat!(
        r#"{"ok":true,"kind":"ghostty-snapshot","mode":"snapshot","seq":900,"#,
        r#""server_epoch":6032428321756423947,"tab_generation":3}"#,
    );
    const REJECTED: &str =
        r#"{"ok":false,"error":{"code":"invalid-token","message":"unknown or expired token"}}"#;

    let accepted = AttachHandshakeReply::Accepted(AttachAccepted {
        kind: AttachPayloadKind::GHOSTTY_SNAPSHOT.into(),
        mode: AttachMode::Snapshot,
        seq: 900,
        server_epoch: EPOCH,
        tab_generation: 3,
    });
    round_trip(&accepted);
    assert_eq!(serde_json::to_string(&accepted).unwrap(), ACCEPTED);
    assert_eq!(
        serde_json::from_str::<AttachHandshakeReply>(ACCEPTED).unwrap(),
        accepted
    );

    let rejected = AttachHandshakeReply::rejected("invalid-token", "unknown or expired token");
    round_trip(&rejected);
    assert_eq!(serde_json::to_string(&rejected).unwrap(), REJECTED);
    assert_eq!(
        serde_json::from_str::<AttachHandshakeReply>(REJECTED).unwrap(),
        rejected
    );
    assert_eq!(
        rejected,
        AttachHandshakeReply::Rejected(ResponseError {
            code: "invalid-token".into(),
            message: "unknown or expired token".into(),
        })
    );
}

/// `ok` is the discriminant, so an accepted arm missing a field it
/// promises must fail loudly rather than decode as a rejection with no
/// error body.
#[test]
fn a_truncated_handshake_reply_is_a_decode_error() {
    assert!(serde_json::from_str::<AttachHandshakeReply>(r#"{"ok":true,"seq":1}"#).is_err());
    assert!(serde_json::from_str::<AttachHandshakeReply>(r#"{"ok":false}"#).is_err());
}

#[test]
fn attach_mode_is_a_lowercase_string() {
    assert_eq!(
        serde_json::to_string(&AttachMode::Snapshot).unwrap(),
        r#""snapshot""#
    );
    assert_eq!(
        serde_json::to_string(&AttachMode::Resume).unwrap(),
        r#""resume""#
    );
    assert_eq!(
        serde_json::from_str::<AttachMode>(r#""resume""#).unwrap(),
        AttachMode::Resume
    );
    assert!(serde_json::from_str::<AttachMode>(r#""Snapshot""#).is_err());
}

#[test]
fn session_connect_vectors_decode_into_their_typed_shapes() {
    let raw = read_vector("session.connect.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(request.op, roost_ipc::messages::ops::SESSION_CONNECT);
    let params: SessionConnectParams =
        serde_json::from_value(request.params).expect("decode connect params");
    assert!(params.takeover);

    let raw = read_vector("session.connect.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: SessionConnectResult =
        serde_json::from_value(resp.result.expect("result body")).expect("decode connect result");
    assert_eq!(result.lease, LEASE);
    assert_eq!(result.revision, 42);
}

#[test]
fn tab_attach_vectors_decode_into_their_typed_shapes() {
    let raw = read_vector("tab.attach.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(request.op, roost_ipc::messages::ops::TAB_ATTACH);
    let params: TabAttachParams =
        serde_json::from_value(request.params).expect("decode attach params");
    assert_eq!(params, sample_attach_params());

    let raw = read_vector("tab.attach.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: TabAttachResult =
        serde_json::from_value(resp.result.expect("result body")).expect("decode attach result");
    assert_eq!(result, sample_attach_result());
}

#[test]
fn session_stopping_vector_decodes_into_its_typed_shape() {
    let raw = read_vector("session.stopping.event.json");
    let envelope: EventEnvelope = serde_json::from_str(&raw).expect("decode event envelope");
    assert_eq!(envelope.event, SESSION_STOPPING_EVENT);
    let data: SessionStoppingEvent =
        serde_json::from_value(envelope.data).expect("decode stopping data");
    assert_eq!(data.reason, "stop");

    // The other half of the published vocabulary. Both are terminal;
    // only the retry advice differs.
    let taken_over: SessionStoppingEvent =
        serde_json::from_str(r#"{"reason":"taken-over"}"#).unwrap();
    assert_eq!(taken_over.reason, "taken-over");
    round_trip(&taken_over);
}

/// Non-terminal by construction: it carries no revision (so the gap
/// check skips it) and the stream is defined to continue after it —
/// the whole point of the R1 re-cut.
#[test]
fn session_driver_changed_vector_decodes_into_its_typed_shape() {
    let raw = read_vector("session.driver_changed.event.json");
    let envelope: EventEnvelope = serde_json::from_str(&raw).expect("decode event envelope");
    assert_eq!(envelope.event, SESSION_DRIVER_CHANGED_EVENT);
    let data: SessionDriverChangedEvent =
        serde_json::from_value(envelope.data).expect("decode driver-changed data");
    assert_eq!(data.taken_by, "kestrel.local");
    round_trip(&data);
    assert_eq!(
        serde_json::to_string(&data).unwrap(),
        r#"{"taken_by":"kestrel.local"}"#
    );

    // A claimant that stated no label still produces an envelope; the
    // server substitutes the fallback rather than omitting the key.
    let unlabeled: SessionDriverChangedEvent =
        serde_json::from_str(r#"{"taken_by":"unknown client"}"#).unwrap();
    assert_eq!(unlabeled.taken_by, "unknown client");
}

/// The labeled variant of the connect request — an additive vector
/// beside the unlabeled one rather than an edit of it, which is what
/// "never edit an existing vector" means in practice.
#[test]
fn labeled_session_connect_vector_decodes_into_its_typed_shape() {
    let raw = read_vector("session.connect.labeled.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(request.op, roost_ipc::messages::ops::SESSION_CONNECT);
    let params: SessionConnectParams =
        serde_json::from_value(request.params).expect("decode connect params");
    assert!(params.takeover);
    assert_eq!(params.client_label.as_deref(), Some("kestrel.local"));
}

/// `features` is an open list with a client-side default: a `3`
/// session sends no such key and must still decode, and a newer
/// session's unrecognized entries survive rather than becoming a
/// decode error (the `payload_kinds` contract, applied to ops).
#[test]
fn session_identify_features_default_when_absent_and_preserve_unknowns() {
    let v3 = decode_identify_vector(&identify_vector_name(SESSION_PROTOCOL_VERSION - 1));
    assert!(
        v3.features.is_empty(),
        "a pre-features generation must decode, not fail"
    );

    let newer: SessionIdentify = serde_json::from_str(
        r#"{"app_version":"0.0.99","session_protocol":9,"payload_kinds":["vt"],
            "features":["put_file","teleport"],"libghostty_build":"b",
            "session_id":"s","started_at":"t"}"#,
    )
    .unwrap();
    assert_eq!(newer.features, vec!["put_file", "teleport"]);
    round_trip(&newer);
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

// ============================================================================
// HS-2 server additions (plan 037 §3.6): effects + theme reseed
// ============================================================================

/// The two effect spellings a client switches on. Kebab-case is not
/// serde's default rendering, so the wire strings are stated here rather
/// than inferred — renaming a variant must break this file, not a
/// client.
#[test]
fn tab_effect_names_are_their_wire_strings() {
    assert_eq!(
        serde_json::to_string(&TabEffect::Bell).unwrap(),
        r#""bell""#
    );
    assert_eq!(
        serde_json::to_string(&TabEffect::ClipboardWrite).unwrap(),
        r#""clipboard-write""#
    );
    assert_eq!(
        serde_json::to_string(&ClipboardEffectTarget::System).unwrap(),
        r#""system""#
    );
    assert_eq!(
        serde_json::to_string(&ClipboardEffectTarget::Selection).unwrap(),
        r#""selection""#
    );
    // An effect this build has never heard of fails to decode rather
    // than landing on a default: a client that cannot tell what happened
    // must ignore the envelope, and the decode is how it finds out.
    assert!(serde_json::from_str::<TabEffect>(r#""pointer-shape""#).is_err());
}

/// A bell carries no payload at all — the optional fields are absent
/// from the wire rather than present and null, so a client reading
/// `data` unconditionally fails loudly on a bell instead of pasting
/// "null" into a clipboard.
#[test]
fn a_bell_effect_omits_its_payload_fields() {
    let bell = TabEffectEvent {
        tab_id: 5,
        effect: TabEffect::Bell,
        data: None,
        target: None,
    };
    assert_eq!(
        serde_json::to_string(&bell).unwrap(),
        r#"{"tab_id":"5","effect":"bell"}"#
    );
    round_trip(&bell);
}

#[test]
fn tab_effect_vector_decodes_into_its_typed_shape() {
    let raw = read_vector("tab.effect.event.json");
    let envelope: EventEnvelope = serde_json::from_str(&raw).expect("decode event envelope");
    assert_eq!(envelope.event, roost_ipc::messages::ops::EVENT_TAB_EFFECT);
    let data: TabEffectEvent = serde_json::from_value(envelope.data).expect("decode effect data");
    assert_eq!(
        data,
        TabEffectEvent {
            tab_id: 5,
            effect: TabEffect::ClipboardWrite,
            // base64 of "hello": the payload rides encoded like every
            // other bytes field on this wire.
            data: Some("aGVsbG8=".into()),
            target: Some(ClipboardEffectTarget::System),
        }
    );
    round_trip(&data);
}

#[test]
fn session_set_theme_vectors_decode_into_their_typed_shapes() {
    let raw = read_vector("session.set_theme.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(request.op, roost_ipc::messages::ops::SESSION_SET_THEME);
    let params: SessionSetThemeParams =
        serde_json::from_value(request.params).expect("decode set_theme params");
    assert_eq!(params.lease, LEASE);
    assert_eq!(params.osc_colors.foreground, "#ffffff");
    assert_eq!(params.osc_colors.background, "#1c1c1c");
    assert_eq!(params.osc_colors.cursor, "#98989d");
    // A full palette or nothing — the server refuses a short one rather
    // than applying half a theme, so the vector states all 256.
    assert_eq!(params.osc_colors.palette.len(), 256);
    assert_eq!(params.osc_colors.palette[0], "#000000");
    assert_eq!(params.osc_colors.palette[255], "#ffffff");
    round_trip(&params);

    let raw = read_vector("session.set_theme.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: SessionSetThemeResult =
        serde_json::from_value(resp.result.expect("result body")).expect("decode set_theme result");
    assert_eq!(result.tabs, 3);
}

/// Strict on the server side, like every other request type: an unknown
/// field is a rejected request, not one applied with a typo in it.
#[test]
fn session_set_theme_params_reject_unknown_fields() {
    let colors = serde_json::json!({
        "foreground": "#ffffff",
        "background": "#000000",
        "cursor": "#ffffff",
        "palette": vec!["#000000"; 256],
    });
    assert!(
        serde_json::from_value::<SessionSetThemeParams>(serde_json::json!({
            "lease": LEASE,
            "osc_colors": colors.clone(),
        }))
        .is_ok()
    );
    assert!(
        serde_json::from_value::<SessionSetThemeParams>(serde_json::json!({
            "lease": LEASE,
            "osc_colors": colors,
            "tab_id": "5",
        }))
        .is_err()
    );
}

// ---------------------------------------------------------------------------
// session.set_focus (plan 038 C6)
// ---------------------------------------------------------------------------

#[test]
fn session_set_focus_vectors_decode_into_their_typed_shapes() {
    let raw = read_vector("session.set_focus.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(request.op, roost_ipc::messages::ops::SESSION_SET_FOCUS);
    let params: SessionSetFocusParams =
        serde_json::from_value(request.params).expect("decode set_focus params");
    assert_eq!(params.lease, LEASE);
    // The wire spelling is `string_int64`, like every other tab id.
    assert_eq!(params.focused_tab_id, Some(5));
    round_trip(&params);

    let raw = read_vector("session.set_focus.none.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    let params: SessionSetFocusParams =
        serde_json::from_value(request.params).expect("decode a null focus");
    assert_eq!(params.focused_tab_id, None);
    round_trip(&params);

    // The result is an empty object, not `null`: the op reports nothing
    // beyond "applied".
    let raw = read_vector("session.set_focus.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    assert_eq!(resp.result, Some(serde_json::json!({})));
}

/// `focused_tab_id` is REQUIRED and nullable, and the two are not the
/// same thing: `null` says "nothing on this session is focused", while
/// an omitted field is a client that never said — and defaulting that to
/// either answer would silently re-create the mute this op exists to
/// fix.
#[test]
fn session_set_focus_requires_the_field_it_lets_be_null() {
    let null: SessionSetFocusParams = serde_json::from_value(serde_json::json!({
        "lease": LEASE,
        "focused_tab_id": null,
    }))
    .expect("an explicit null is a statement");
    assert_eq!(null.focused_tab_id, None);

    let missing = serde_json::from_value::<SessionSetFocusParams>(serde_json::json!({
        "lease": LEASE,
    }))
    .expect_err("an omitted focused_tab_id must not decode");
    assert!(
        missing.to_string().contains("missing field"),
        "the refusal has to name the missing field so the server answers \
         `missing-param`: {missing}"
    );

    // Serialization keeps the field present in both shapes, so a client
    // built from this type cannot emit the omission either.
    assert_eq!(
        serde_json::to_value(&null).expect("serialize"),
        serde_json::json!({"lease": LEASE, "focused_tab_id": null}),
    );
    let some = SessionSetFocusParams {
        lease: LEASE.into(),
        focused_tab_id: Some(7),
    };
    assert_eq!(
        serde_json::to_value(&some).expect("serialize"),
        serde_json::json!({"lease": LEASE, "focused_tab_id": "7"}),
    );
}

/// Strict like its siblings, and a non-numeric id is a refusal rather
/// than a zero.
#[test]
fn session_set_focus_params_reject_unknown_fields_and_junk_ids() {
    assert!(
        serde_json::from_value::<SessionSetFocusParams>(serde_json::json!({
            "lease": LEASE,
            "focused_tab_id": "5",
            "project_id": "1",
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<SessionSetFocusParams>(serde_json::json!({
            "lease": LEASE,
            "focused_tab_id": "h3.7",
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<SessionSetFocusParams>(serde_json::json!({
            "lease": LEASE,
            "focused_tab_id": 5,
        }))
        .is_err()
    );
}

// ---------------------------------------------------------------------------
// session.set_agent_hooks (plan 046 C8)
// ---------------------------------------------------------------------------

#[test]
fn session_set_agent_hooks_vectors_decode_into_their_typed_shapes() {
    let raw = read_vector("session.set_agent_hooks.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(
        request.op,
        roost_ipc::messages::ops::SESSION_SET_AGENT_HOOKS
    );
    let params: SessionSetAgentHooksParams =
        serde_json::from_value(request.params).expect("decode set_agent_hooks params");
    assert_eq!(params.lease, LEASE);
    assert_eq!(params.mode, AgentHooksMode::Auto);
    assert_eq!(params.skip, vec!["cursor".to_string()]);
    assert_eq!(params.client, "charlie-mbp");
    round_trip(&params);

    // The other half of the pin: `off` on the client is `off` on the
    // host, and it travels as a value of this same op rather than as an
    // absence of it.
    let raw = read_vector("session.set_agent_hooks.off.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    let params: SessionSetAgentHooksParams =
        serde_json::from_value(request.params).expect("decode an off request");
    assert_eq!(params.mode, AgentHooksMode::Off);
    assert!(params.skip.is_empty());
    round_trip(&params);

    let raw = read_vector("session.set_agent_hooks.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: SessionSetAgentHooksResult = serde_json::from_value(resp.result.expect("result"))
        .expect("decode set_agent_hooks result");
    assert_eq!(
        result.wired,
        vec!["claude".to_string(), "codex".to_string()]
    );
    assert!(result.refreshed.is_empty());
    assert!(result.removed.is_empty());
    assert!(result.errors.is_empty());
    let reasons: Vec<(&str, &str)> = result
        .skipped
        .iter()
        .map(|s| (s.agent.as_str(), s.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        vec![("cursor", "skip-list"), ("grok", "not installed")]
    );
    round_trip(&result);
}

/// `mode` is the whole decision, so it is a closed set rather than a
/// string: a typo has to be `invalid-param` on the wire, not a silent
/// `auto` that wires a host the user asked to leave alone — nor a silent
/// `off` that strips one.
#[test]
fn session_set_agent_hooks_params_are_strict_about_mode_and_shape() {
    let ok = serde_json::json!({
        "lease": LEASE,
        "mode": "off",
        "skip": [],
        "client": "charlie-mbp",
    });
    assert!(serde_json::from_value::<SessionSetAgentHooksParams>(ok).is_ok());

    for bad in [
        serde_json::json!({"lease": LEASE, "mode": "Auto", "client": "c"}),
        serde_json::json!({"lease": LEASE, "mode": "on", "client": "c"}),
        serde_json::json!({"lease": LEASE, "mode": true, "client": "c"}),
    ] {
        assert!(
            serde_json::from_value::<SessionSetAgentHooksParams>(bad.clone()).is_err(),
            "{bad} must not decode"
        );
    }

    // `client` is required — the host's record has to name who asked.
    let missing = serde_json::from_value::<SessionSetAgentHooksParams>(serde_json::json!({
        "lease": LEASE,
        "mode": "auto",
    }))
    .expect_err("an omitted client must not decode");
    assert!(missing.to_string().contains("missing field"), "{missing}");

    // `skip` is the one field a client may omit: no skip list is the
    // ordinary case, and demanding an empty array would break nothing
    // except hand-written requests.
    let bare: SessionSetAgentHooksParams = serde_json::from_value(serde_json::json!({
        "lease": LEASE,
        "mode": "auto",
        "client": "charlie-mbp",
    }))
    .expect("an omitted skip list is an empty one");
    assert!(bare.skip.is_empty());

    assert!(
        serde_json::from_value::<SessionSetAgentHooksParams>(serde_json::json!({
            "lease": LEASE,
            "mode": "auto",
            "client": "charlie-mbp",
            "tab_id": "5",
        }))
        .is_err(),
        "strict like every other request type"
    );
}

// ============================================================================
// The reorder ops' host-qualified form (plan 044 §3.1 d6)
// ============================================================================

/// `WireProjectRef` is `WireTabRef`'s twin down to the parser's
/// strictness: `parse(s)?.to_string() == s`, so the local instance is
/// always bare and nothing is normalized on the way in.
#[test]
fn wire_project_ref_round_trips_exactly() {
    for text in ["0", "4", "-1", "9223372036854775807", "h1.4", "h3.0"] {
        let parsed = WireProjectRef::parse(text).unwrap_or_else(|| panic!("{text} must parse"));
        assert_eq!(parsed.to_string(), text);
        let json = serde_json::to_value(parsed).expect("serialize");
        assert_eq!(json, serde_json::Value::String(text.into()));
        let back: WireProjectRef = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, parsed);
    }

    assert_eq!(WireProjectRef::default(), WireProjectRef::Local(0));
    assert_eq!(WireProjectRef::Local(4).local(), Some(4));
    assert_eq!(
        WireProjectRef::Host {
            host: 3,
            project: 4
        }
        .local(),
        None,
        "a qualified ref narrows to nothing, which is what every \
         host-unaware consumer checks"
    );
}

/// The rejections are the round-trip rule doing its work: a spelling
/// that would come back out differently never comes in.
#[test]
fn wire_project_ref_rejects_non_canonical_spellings() {
    for text in [
        "h0.4",  // the local instance is always bare
        "+4",    // parses as 4, prints as "4"
        "04",    // leading zero
        "h1.04", // ... on either half
        "h01.4", "h3",  // no id
        "h.4", // no host
        "h3.", "", "four", "h-1.4", // a host id is unsigned
        "3.4",   // the `h` is not optional
    ] {
        assert!(
            WireProjectRef::parse(text).is_none(),
            "{text} must not parse"
        );
        assert!(
            serde_json::from_value::<WireProjectRef>(serde_json::Value::String(text.into()))
                .is_err(),
            "{text} must not decode"
        );
    }
    assert!(
        serde_json::from_value::<WireProjectRef>(serde_json::json!(4)).is_err(),
        "a bare number is not the wire form; ids are string-wrapped"
    );
}

/// The bare form is byte-identical to what it was before the qualified
/// one existed — string-wrapped ids, same field names, no extra keys.
/// This is the whole compatibility claim for local traffic.
#[test]
fn local_reorder_params_are_byte_identical() {
    let tabs = TabReorderParams {
        project_id: WireProjectRef::Local(1),
        tab_ids: vec![
            WireTabRef::Local(5),
            WireTabRef::Local(3),
            WireTabRef::Local(1),
        ],
    };
    assert_eq!(
        serde_json::to_string(&tabs).expect("serialize"),
        r#"{"project_id":"1","tab_ids":["5","3","1"]}"#
    );
    assert_eq!(
        serde_json::from_str::<TabReorderParams>(r#"{"project_id":"1","tab_ids":["5","3","1"]}"#)
            .expect("decode"),
        tabs
    );

    let projects = ProjectReorderParams {
        project_ids: vec![WireProjectRef::Local(2), WireProjectRef::Local(1)],
    };
    assert_eq!(
        serde_json::to_string(&projects).expect("serialize"),
        r#"{"project_ids":["2","1"]}"#
    );
    assert_eq!(
        serde_json::from_str::<ProjectReorderParams>(r#"{"project_ids":["2","1"]}"#)
            .expect("decode"),
        projects
    );
}

/// The host form on the wire, and the strictness that comes with it:
/// unknown fields are still refused, and a junk ref is a decode failure
/// rather than a zero.
#[test]
fn host_qualified_reorder_params_decode() {
    let tabs: TabReorderParams =
        serde_json::from_str(r#"{"project_id":"h3.4","tab_ids":["h3.9","h3.7"]}"#).expect("decode");
    assert_eq!(
        tabs.project_id,
        WireProjectRef::Host {
            host: 3,
            project: 4
        }
    );
    assert_eq!(
        tabs.tab_ids,
        vec![
            WireTabRef::Host { host: 3, tab: 9 },
            WireTabRef::Host { host: 3, tab: 7 }
        ]
    );
    assert_eq!(
        serde_json::to_string(&tabs).expect("re-serialize"),
        r#"{"project_id":"h3.4","tab_ids":["h3.9","h3.7"]}"#
    );

    let projects: ProjectReorderParams =
        serde_json::from_str(r#"{"project_ids":["h3.4","h3.2"]}"#).expect("decode");
    assert_eq!(
        projects.project_ids,
        vec![
            WireProjectRef::Host {
                host: 3,
                project: 4
            },
            WireProjectRef::Host {
                host: 3,
                project: 2
            }
        ]
    );

    // The mixed form decodes — it is the *engine* that refuses it, with
    // a message naming the rule, so the refusal can say which rule.
    assert!(
        serde_json::from_str::<TabReorderParams>(r#"{"project_id":"1","tab_ids":["h3.7"]}"#)
            .is_ok()
    );

    assert!(
        serde_json::from_str::<TabReorderParams>(r#"{"project_id":"1","tab_ids":[],"extra":true}"#)
            .is_err(),
        "deny_unknown_fields survives the type change"
    );
    assert!(
        serde_json::from_str::<ProjectReorderParams>(r#"{"project_ids":["h0.4"]}"#).is_err(),
        "a non-canonical ref is a decode failure, not a silent Local(0)"
    );
}

/// A negative *id* on a remote instance (`h3.-4`) parses, on both
/// twins. The choice, stated: engine ids are `i64` and the parser's job
/// is canonical spelling, not range — a ref no instance ever minted is
/// refused where ids are actually resolved (`not-found` from the
/// session), which is the same answer `h3.999999` gets. A negative
/// *host* does not parse, because incarnations are `u32`.
#[test]
fn a_negative_id_is_the_answering_instances_business() {
    assert_eq!(
        WireProjectRef::parse("h3.-4"),
        Some(WireProjectRef::Host {
            host: 3,
            project: -4
        })
    );
    assert_eq!(
        WireTabRef::parse("h3.-4"),
        Some(WireTabRef::Host { host: 3, tab: -4 })
    );
    assert_eq!(WireProjectRef::parse("-4"), Some(WireProjectRef::Local(-4)));

    // The host half is unsigned, so its negative spelling is refused by
    // the parser rather than deferred.
    assert!(WireProjectRef::parse("h-3.4").is_none());
    assert!(WireTabRef::parse("h-3.4").is_none());
}

/// `retry.reason` is additive and optional (plan 044 §3.3, #399): a
/// payload written before this field existed still decodes, and a
/// schedule without a family still serializes to exactly the bytes it
/// did — a decoder pinned to the old shape sees no change.
///
/// The field carries **why** the rung is armed, which the sibling
/// `HostStatus::reason` cannot: while a rung is armed that one has to
/// read `reconnecting in 8s (3/10)`, because the sidebar's rollup is
/// derived from it.
#[test]
fn a_retry_schedules_reason_is_additive() {
    let armed = RetrySchedule {
        delay_ms: 8_000,
        attempt: Some(3),
        budget: Some(10),
        armed_at: Some("2026-09-01T18:02:11Z".into()),
        reason: Some(
            "connecting to workbox failed: ssh: connect to host workbox port 22: \
             Connection refused"
                .into(),
        ),
    };
    round_trip(&armed);
    assert_eq!(
        serde_json::to_value(&armed).unwrap(),
        serde_json::json!({
            "delay_ms": 8_000,
            "attempt": 3,
            "budget": 10,
            "armed_at": "2026-09-01T18:02:11Z",
            "reason": "connecting to workbox failed: ssh: connect to host workbox \
                       port 22: Connection refused",
        })
    );

    // The pre-#399 payload, byte for byte: it decodes, and the missing
    // family reads as absent rather than as an empty string.
    let old: RetrySchedule = serde_json::from_str(
        r#"{"delay_ms":8000,"attempt":3,"budget":10,"armed_at":"2026-09-01T18:02:11Z"}"#,
    )
    .expect("a payload from before the field existed still decodes");
    assert_eq!(old.reason, None);
    assert_eq!(
        serde_json::to_string(&old).unwrap(),
        r#"{"delay_ms":8000,"attempt":3,"budget":10,"armed_at":"2026-09-01T18:02:11Z"}"#,
        "a rung with no family re-encodes to the bytes it decoded from"
    );

    // And the localhost form stays the one field it has always been.
    assert_eq!(
        serde_json::to_string(&RetrySchedule {
            delay_ms: 250,
            ..RetrySchedule::default()
        })
        .unwrap(),
        r#"{"delay_ms":250}"#
    );
}

// ============================================================================
// Files across a host boundary (plan 047 §3.1, §3.4, §3.5)
// ============================================================================

/// The raw cap has to leave room for its own base64 inside a frame, or
/// the op would need chunking it deliberately does not have.
#[test]
fn the_put_file_cap_base64s_inside_one_frame() {
    assert_eq!(MAX_PUT_FILE_BYTES, 10 * 1024 * 1024);
    let encoded = MAX_PUT_FILE_BYTES.div_ceil(3) * 4;
    assert!(
        (encoded as usize) < roost_ipc::MAX_FRAME_BYTES,
        "{encoded} encoded bytes must fit a {} byte frame with the envelope",
        roost_ipc::MAX_FRAME_BYTES
    );
}

#[test]
fn session_put_file_shapes_match_their_golden_json() {
    const PARAMS: &str = concat!(
        r#"{"lease":"9f2c1d7a4b6e08315c0d9a72e4f16b83","#,
        r#""name":"shot.png","data":"aGVsbG8="}"#,
    );
    let params = SessionPutFileParams {
        lease: LEASE.into(),
        name: "shot.png".into(),
        data: b"hello".to_vec(),
    };
    round_trip(&params);
    assert_eq!(serde_json::to_string(&params).unwrap(), PARAMS);
    assert_eq!(
        serde_json::from_str::<SessionPutFileParams>(PARAMS).unwrap(),
        params
    );

    // Strict like every other request type: a caller that misspells a
    // field is refused, not served with the field ignored.
    assert!(serde_json::from_str::<SessionPutFileParams>(
        r#"{"lease":"l","name":"a.png","data":"aGVsbG8=","mode":"0600"}"#
    )
    .is_err());
    // Malformed base64 is a decode failure, so the engine never sees a
    // half-decoded payload.
    assert!(serde_json::from_str::<SessionPutFileParams>(
        r#"{"lease":"l","name":"a","data":"!!"}"#
    )
    .is_err());

    const RESULT: &str =
        r#"{"path":"/home/c/.cache/roost-session/files/4b9d1e7f0a3c5e21/shot.png","bytes":482113}"#;
    let result = SessionPutFileResult {
        path: "/home/c/.cache/roost-session/files/4b9d1e7f0a3c5e21/shot.png".into(),
        bytes: 482_113,
    };
    round_trip(&result);
    assert_eq!(serde_json::to_string(&result).unwrap(), RESULT);
    // `bytes` is a count, not an id: the string-int64 convention
    // deliberately does not apply, so it must stay a JSON number.
    assert!(serde_json::to_value(&result).unwrap()["bytes"].is_u64());
}

#[test]
fn tab_send_file_shapes_match_their_golden_json() {
    const PARAMS: &str = r#"{"tab":"h2.7","paths":["/Users/c/Desktop/shot.png"]}"#;
    let params = TabSendFileParams {
        tab: "h2.7".into(),
        paths: vec!["/Users/c/Desktop/shot.png".into()],
    };
    round_trip(&params);
    assert_eq!(serde_json::to_string(&params).unwrap(), PARAMS);
    // The tab ref rides as the same string `tab.focus` takes, so both
    // spellings have to survive the trip unchanged.
    for raw in ["5", "h2.7"] {
        let one: TabSendFileParams =
            serde_json::from_value(serde_json::json!({"tab": raw, "paths": ["/a"]})).unwrap();
        assert_eq!(one.tab, raw);
        assert!(WireTabRef::parse(&one.tab).is_some(), "{raw} parses");
    }
    assert!(serde_json::from_str::<TabSendFileParams>(
        r#"{"tab":"5","paths":["/a"],"host":"hs-2f1c"}"#
    )
    .is_err());

    const RESULT: &str = concat!(
        r#"{"pasted":"/home/c/files/shot.png","uploads":[{"source":"/Users/c/Desktop/shot.png","#,
        r#""name":"shot.png","path":"/home/c/files/shot.png","bytes":482113}],"#,
        r#""skipped":[{"path":"/Users/c/build","reason":"directory"}]}"#,
    );
    let result = TabSendFileResult {
        pasted: "/home/c/files/shot.png".into(),
        uploads: vec![SentFile {
            source: "/Users/c/Desktop/shot.png".into(),
            name: "shot.png".into(),
            path: "/home/c/files/shot.png".into(),
            bytes: 482_113,
        }],
        skipped: vec![SkippedFile {
            path: "/Users/c/build".into(),
            reason: "directory".into(),
        }],
    };
    round_trip(&result);
    assert_eq!(serde_json::to_string(&result).unwrap(), RESULT);

    // A local tab crosses no boundary: the pasted text is the escaped
    // local path and nothing was uploaded.
    let local = TabSendFileResult {
        pasted: "/Users/c/my\\ shot.png".into(),
        ..TabSendFileResult::default()
    };
    round_trip(&local);
    assert!(local.uploads.is_empty());
}

/// The five spellings the planner emits. A `String` on the wire, so a
/// reason a client has no name for still decodes — but these five are
/// the published set, and renaming one has to break here.
#[test]
fn every_published_skip_reason_round_trips() {
    for reason in [
        "directory",
        "missing",
        "unreadable",
        "not-regular",
        "over-cap",
    ] {
        let skipped = SkippedFile {
            path: "/a".into(),
            reason: reason.into(),
        };
        round_trip(&skipped);
        assert_eq!(
            serde_json::to_value(&skipped).unwrap()["reason"],
            serde_json::Value::String(reason.into())
        );
    }
    let future: SkippedFile =
        serde_json::from_str(r#"{"path":"/a","reason":"encrypted-volume"}"#).unwrap();
    assert_eq!(future.reason, "encrypted-volume");
}

/// `clipboard.write` takes exactly one of `text` / `image_png` (plan
/// 047 §3.5). `text` went optional here, so the two things this pins
/// are that a `text` request is byte-identical to what it always was,
/// and that the image form neither invents a `text` key nor accepts
/// both.
#[test]
fn clipboard_write_carries_text_or_a_png_and_never_invents_the_other() {
    const TEXT: &str = r#"{"target":"system","text":"hello"}"#;
    let text = ClipboardWriteParams {
        target: "system".into(),
        text: Some("hello".into()),
        image_png: None,
    };
    round_trip(&text);
    assert_eq!(serde_json::to_string(&text).unwrap(), TEXT);
    assert_eq!(
        serde_json::from_str::<ClipboardWriteParams>(TEXT).unwrap(),
        text
    );

    const IMAGE: &str = r#"{"target":"system","image_png":"aGVsbG8="}"#;
    let image = ClipboardWriteParams {
        target: "system".into(),
        text: None,
        image_png: Some(b"hello".to_vec()),
    };
    round_trip(&image);
    assert_eq!(serde_json::to_string(&image).unwrap(), IMAGE);
    assert_eq!(
        serde_json::from_str::<ClipboardWriteParams>(IMAGE).unwrap(),
        image
    );

    // Both absent decodes — the handler answers `missing-param`, which
    // is the layer that can say which op needed which field.
    let neither: ClipboardWriteParams = serde_json::from_str(r#"{"target":"system"}"#).unwrap();
    assert_eq!(neither.text, None);
    assert_eq!(neither.image_png, None);

    // An explicit null is absent, not empty bytes.
    let nulled: ClipboardWriteParams =
        serde_json::from_str(r#"{"target":"system","text":null,"image_png":null}"#).unwrap();
    assert_eq!(nulled.image_png, None);

    assert!(serde_json::from_str::<ClipboardWriteParams>(
        r#"{"target":"system","image_png":"!!"}"#
    )
    .is_err());
    assert!(serde_json::from_str::<ClipboardWriteParams>(
        r#"{"target":"system","text":"a","html":"<b>a</b>"}"#
    )
    .is_err());
}

// ============================================================================
// events.subscribe — the resume params (plan 052 §3.5)
// ============================================================================

fn decode_request_vector(name: &str) -> serde_json::Value {
    let raw = read_vector(name);
    let request: serde_json::Value = serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(request["op"], ops::EVENTS_SUBSCRIBE, "{name}: wrong op");
    request["params"].clone()
}

/// The four-direction rule for an additive request field
/// (`docs/reference/ipc-compatibility.md`): a client that does not
/// resume must put no new key on the wire at all, because an older
/// server's `deny_unknown_fields` would refuse the whole request.
#[test]
fn events_subscribe_omits_the_resume_keys_when_they_are_unset() {
    const GOLDEN: &str = r#"{"lease":"9f2c1d7a","tab_id_filter":"0"}"#;

    let plain = EventsSubscribeParams {
        lease: "9f2c1d7a".into(),
        tab_id_filter: 0,
        from_revision: None,
        session_id: None,
    };
    round_trip(&plain);
    assert_eq!(serde_json::to_string(&plain).unwrap(), GOLDEN);

    // A pre-052 request decodes into the new shape unchanged — the
    // other direction of the same rule.
    let decoded: EventsSubscribeParams = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(decoded, plain);
    assert_eq!(decoded.from_revision, None);
    assert_eq!(decoded.session_id, None);

    // Still strict: additive optional fields are not a licence for
    // arbitrary keys.
    let error = serde_json::from_str::<EventsSubscribeParams>(
        r#"{"lease":"a","tab_id_filter":"0","from_rev":9}"#,
    )
    .expect_err("an unknown key must be refused");
    assert!(
        error.to_string().contains("unknown field"),
        "unexpected error: {error}"
    );
}

/// A revision is a plain JSON number, not a string: it is an in-process
/// counter, not an id, so the string-int64 convention the ids use does
/// not apply to it (and the ack it is echoed in has always been one).
#[test]
fn events_subscribe_resume_matches_its_vector() {
    let resume = EventsSubscribeParams {
        lease: "9f2c1d7a4b6e08315c0d9a72e4f16b83".into(),
        tab_id_filter: 0,
        from_revision: Some(1180),
        session_id: Some("01K3S8TQ4F0Q9YB2K6WZ5D7XN".into()),
    };
    round_trip(&resume);

    let as_value = serde_json::to_value(&resume).unwrap();
    assert_eq!(
        as_value["from_revision"],
        serde_json::json!(1180),
        "a number, not the string-int64 an id would use: {as_value}"
    );

    let params = decode_request_vector("events.subscribe.resume.request.json");
    assert_eq!(params, as_value);
    let typed: EventsSubscribeParams = serde_json::from_value(params).expect("typed resume params");
    assert_eq!(typed, resume);

    let plain = decode_request_vector("events.subscribe.request.json");
    assert!(
        plain.get("from_revision").is_none() && plain.get("session_id").is_none(),
        "the plain exemplar must carry neither resume key: {plain}"
    );
    let typed: EventsSubscribeParams = serde_json::from_value(plain).expect("typed plain params");
    assert_eq!(typed.from_revision, None);
    assert_eq!(typed.session_id, None);
}

/// The refusal a client feature-detects on: it is answered on the ack,
/// so it arrives as an ordinary error envelope and the connection is
/// still a request/response connection afterwards.
#[test]
fn events_subscribe_error_vector_decodes_as_replay_expired() {
    let raw = read_vector("events.subscribe.error.json");
    let response: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(!response.ok);
    let error = response.error.expect("an error body");
    assert_eq!(error.code, "replay-expired");
    assert_eq!(
        roost_ipc::client::ServerCode::from_wire(&error.code),
        roost_ipc::client::ServerCode::ReplayExpired,
        "every refusal maps to a variant by name"
    );
    assert!(
        error.message.contains("oldest resumable"),
        "the message must say how far back a resume can reach: {}",
        error.message
    );
}
