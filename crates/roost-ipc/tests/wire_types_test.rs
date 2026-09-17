//! Host-session wire types: `SessionIdentify`, `EventBatch`,
//! `AttachPayloadKind`, the attach handshake
//! and its reply, and `SESSION_PROTOCOL_VERSION`.
//!
//! What pins their shape is this file plus the golden vectors under
//! `tests/ipc-vectors/` (the identify vector per generation, see
//! `identify_vector_name`), which the Swift mirror in
//! `mac/Sources/Roost/IPCMessages.swift` consumes too. The
//! assertions are deliberately byte-exact against literal JSON:
//! a field rename or a reordering that a `round_trip` would happily
//! accept is a cross-language break. These fixtures are a
//! compatibility contract — an existing vector is edited only by a
//! breaking `SESSION_PROTOCOL_VERSION` bump, in the same commit;
//! see `docs/reference/ipc-compatibility.md`.

use std::fs;
use std::path::PathBuf;

use roost_ipc::messages::{
    ops, AgentHooksOutcome, AgentSetHooksAgents, AgentSetHooksParams, AgentSetHooksResult,
    AttachAccepted, AttachHandshake, AttachHandshakeReply, AttachHandshakeTerms, AttachMode,
    AttachPayloadKind, ClipboardEffectTarget, ClipboardWriteParams, DurabilityChangedEvent,
    EventBatch, EventEnvelope, EventsSubscribeParams, EventsSubscribeResult, IdentifyResult,
    NotificationFiredEvent, ProjectReorderParams, ResponseError, RetrySchedule, SentFile,
    SessionBinaryIdentity, SessionIdentify, SessionIdentifyParams, SessionPutFileParams,
    SessionPutFileResult, SessionSetAgentHooksParams, SessionSetThemeParams, SessionSetThemeResult,
    SessionStopParams, SessionStopResult, SessionStoppingEvent, SkippedFile,
    TabClearNotificationParams, TabClearNotificationResult, TabDumpCursor, TabDumpParams,
    TabDumpResult, TabEffect, TabEffectEvent, TabReorderParams, TabSendFileParams,
    TabSendFileResult, TabWriteParams, WireProjectRef, WireTabRef, MAX_PUT_FILE_BYTES,
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
        libghostty_build: "ghostty-3f6b1c9a4d2e5f80+snapshot.v1".into(),
        session_id: "01K3S8TQ4F0Q9YB2K6WZ5D7XN".into(),
        started_at: "2026-08-27T14:03:11Z".into(),
        persist_error: None,
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
         contract; see docs/reference/ipc-compatibility.md"
    );
}

/// `6` reshapes `session.set_agent_hooks` into a pure raise: `mode` +
/// `skip` are gone, replaced by a single `agents` allow-list, and the
/// op can no longer spell `off` or a narrowing — breaking in both
/// directions (plan 064 §3.3). The request/response wire version did
/// not move with it — the two version different things.
#[test]
fn session_protocol_version_is_six() {
    assert_eq!(SESSION_PROTOCOL_VERSION, 6);
    assert_eq!(roost_ipc::PROTOCOL_VERSION, 1);
}

/// The break, from the far side: every op a `4` client put a `lease` on
/// refuses the key outright, because each of those params is
/// `deny_unknown_fields` — which is what the engine answers
/// `invalid-param` to. Together with `ops`' having no `session.connect`
/// (a compile-time fact, and `unknown-op` on the wire) this is the whole
/// of what a pre-bump client hits.
#[test]
fn a_request_carrying_a_lease_is_refused_by_every_op_that_took_one() {
    let refused = |what: &str, result: Result<(), serde_json::Error>| {
        let error = result.expect_err(&format!("{what} must refuse a `lease` key"));
        assert!(
            error.to_string().contains("unknown field `lease`"),
            "{what}: {error}"
        );
    };

    refused(
        ops::TAB_WRITE,
        serde_json::from_str::<TabWriteParams>(r#"{"tab_id":"5","data":"bHMK","lease":"l"}"#)
            .map(drop),
    );
    refused(
        ops::EVENTS_SUBSCRIBE,
        serde_json::from_str::<EventsSubscribeParams>(r#"{"tab_id_filter":"0","lease":"l"}"#)
            .map(drop),
    );
    refused(
        ops::SESSION_SET_THEME,
        serde_json::from_value::<SessionSetThemeParams>(serde_json::json!({
            "lease": "l",
            "osc_colors": {
                "foreground": "#ffffff",
                "background": "#000000",
                "cursor": "#ffffff",
                "palette": vec!["#000000"; 256],
            },
        }))
        .map(drop),
    );
    refused(
        ops::SESSION_SET_AGENT_HOOKS,
        serde_json::from_value::<SessionSetAgentHooksParams>(
            serde_json::json!({"lease": "l", "agents": ["claude"], "client": "c"}),
        )
        .map(drop),
    );
    refused(
        ops::SESSION_PUT_FILE,
        serde_json::from_str::<SessionPutFileParams>(
            r#"{"lease":"l","name":"a.png","data":"aGVsbG8="}"#,
        )
        .map(drop),
    );
}

/// The ack names the incarnation, and `session_id` is **required**: a
/// subscribe's two legs can be two dials, so a client that cannot read
/// which session answered cannot refuse a pair that disagree.
#[test]
fn the_subscribe_ack_names_its_incarnation_and_cannot_omit_it() {
    const GOLDEN: &str = r#"{"revision":42,"session_id":"01K3S8TQ4F0Q9YB2K6WZ5D7XN"}"#;

    let ack = EventsSubscribeResult {
        revision: 42,
        session_id: "01K3S8TQ4F0Q9YB2K6WZ5D7XN".into(),
    };
    round_trip(&ack);
    assert_eq!(serde_json::to_string(&ack).unwrap(), GOLDEN);
    assert_eq!(
        serde_json::from_str::<EventsSubscribeResult>(GOLDEN).unwrap(),
        ack
    );

    let missing = serde_json::from_str::<EventsSubscribeResult>(r#"{"revision":42}"#)
        .expect_err("an ack without a session_id must not decode");
    assert!(
        missing.to_string().contains("missing field `session_id`"),
        "{missing}"
    );

    // A counter, not an id: the string-int64 convention does not apply.
    assert!(serde_json::to_value(&ack).unwrap()["revision"].is_u64());
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
        r#"{"app_version":"0.0.18","session_protocol":6,"#,
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

/// `roost-session identify`'s wire shape (plan 039 §3.1) — a binary's
/// offline identity, not a running session's. Distinct from
/// `SessionIdentify` above: three required fields, no `payload_kinds` /
/// `session_id` / `started_at`.
#[test]
fn session_binary_identity_matches_its_golden_json() {
    const GOLDEN: &str = concat!(
        r#"{"app_version":"0.0.19","session_protocol":6,"#,
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

/// The current generation's vector is this build's identity, field for
/// field: the integer is the whole negotiation at `5`, so there is
/// nothing a session may differ on and still be served.
#[test]
fn session_identify_vector_decodes_into_its_typed_result() {
    let result = decode_identify_vector(&identify_vector_name(SESSION_PROTOCOL_VERSION));
    assert_eq!(result.session_protocol, SESSION_PROTOCOL_VERSION);
    assert_eq!(result, sample_identify());
}

/// Every prior generation's vector stays on disk and keeps decoding —
/// the compatibility policy's "an old client's view still parses", as a
/// test rather than a claim, and what the compat gate relies on to
/// refuse an old session *by name* rather than on a decode failure. The
/// frozen files carry keys this generation no longer declares
/// (`features`), which `SessionIdentify` tolerates by design. `2` is the
/// first versioned generation (the corpus was backfilled at the `2` →
/// `3` bump).
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
// Attach
// ============================================================================

const EPOCH: u64 = 6_032_428_321_756_423_947;

/// The whole negotiation on one line: `attach` names the tab as a
/// `string_int64` and the terms ride beside it.
#[test]
fn an_attach_handshake_matches_its_golden_json() {
    const SNAPSHOT: &str = concat!(
        r#"{"attach":"7","protocol_version":6,"#,
        r#""session_id":"01K3S8TQ4F0Q9YB2K6WZ5D7XN","kinds":["ghostty-snapshot","vt"],"#,
        r#""cols":100,"rows":30,"cell_w_px":8,"cell_h_px":16,"#,
        r#""libghostty_build":"ghostty-1a2b3c4d5e6f7a8b+snapshot.v1","focus":true}"#,
    );
    const RESUME: &str = concat!(
        r#"{"attach":"7","protocol_version":6,"#,
        r#""session_id":"01K3S8TQ4F0Q9YB2K6WZ5D7XN","kinds":["ghostty-snapshot","vt"],"#,
        r#""cols":100,"rows":30,"cell_w_px":8,"cell_h_px":16,"#,
        r#""libghostty_build":"ghostty-1a2b3c4d5e6f7a8b+snapshot.v1","focus":true,"#,
        r#""resume_from_seq":901,"server_epoch":6032428321756423947,"tab_generation":3}"#,
    );

    let fresh = AttachHandshake::snapshot(7, sample_handshake_terms());
    round_trip(&fresh);
    assert_eq!(serde_json::to_string(&fresh).unwrap(), SNAPSHOT);
    assert_eq!(
        serde_json::from_str::<AttachHandshake>(SNAPSHOT).unwrap(),
        fresh
    );

    let resuming = AttachHandshake::resume(7, sample_handshake_terms(), 901, EPOCH, 3);
    round_trip(&resuming);
    assert_eq!(serde_json::to_string(&resuming).unwrap(), RESUME);
    assert_eq!(
        serde_json::from_str::<AttachHandshake>(RESUME).unwrap(),
        resuming
    );
}

fn sample_handshake_terms() -> AttachHandshakeTerms {
    AttachHandshakeTerms {
        session_id: "01K3S8TQ4F0Q9YB2K6WZ5D7XN".into(),
        kinds: vec![
            AttachPayloadKind::GHOSTTY_SNAPSHOT.into(),
            AttachPayloadKind::VT.into(),
        ],
        cols: 100,
        rows: 30,
        cell_w_px: 8,
        cell_h_px: 16,
        libghostty_build: "ghostty-1a2b3c4d5e6f7a8b+snapshot.v1".into(),
        focus: true,
    }
}

/// A handshake is all-or-nothing, and the decode error names the term
/// that is missing — a client that forgot one must not be left reading
/// "invalid JSON".
///
/// `kinds` is in the loop like any other term now. It used to be the
/// discriminator between the inline form and a ticket, so a line
/// without it was a *different shape* rather than a malformed one;
/// there is one shape left and a missing `kinds` is simply missing.
#[test]
fn a_handshake_missing_a_required_term_names_it() {
    for missing in [
        "session_id",
        "kinds",
        "cols",
        "rows",
        "libghostty_build",
        "focus",
    ] {
        let mut line = serde_json::json!({
            "attach": "7",
            "protocol_version": 6,
            "session_id": "s",
            "kinds": ["vt"],
            "cols": 80,
            "rows": 24,
            "libghostty_build": "b",
            "focus": false,
        });
        line.as_object_mut().unwrap().remove(missing);
        let error = serde_json::from_value::<AttachHandshake>(line)
            .expect_err("every term but the cell metrics is required");
        assert!(
            error.to_string().contains(missing),
            "the error names {missing}: {error}"
        );
    }
}

/// The two terms a headless client genuinely has nothing to say about.
#[test]
fn a_handshake_may_omit_its_cell_metrics() {
    let decoded: AttachHandshake = serde_json::from_str(concat!(
        r#"{"attach":"7","protocol_version":6,"session_id":"s","kinds":["vt"],"#,
        r#""cols":80,"rows":24,"libghostty_build":"b","focus":true}"#,
    ))
    .expect("a headless client reports no cell metrics");
    assert_eq!((decoded.terms.cell_w_px, decoded.terms.cell_h_px), (0, 0));
}

/// The handshake is the one line a client of a *newer* build might send
/// with fields this build has never heard of; refusing it over one
/// would turn an additive change into a hard incompatibility.
#[test]
fn attach_handshake_tolerates_unknown_fields() {
    let decoded: AttachHandshake = serde_json::from_str(
        r#"{"attach":"7","protocol_version":2,"session_id":"s","kinds":["vt"],
            "cols":80,"rows":24,"libghostty_build":"b","focus":true,
            "resume_from_seq":5,"viewport_hint":{"top":0},"future_field":true}"#,
    )
    .unwrap();
    assert_eq!(decoded.attach, "7");
    assert_eq!(decoded.resume_from_seq, Some(5));
    assert_eq!(decoded.server_epoch, None);
}

#[test]
fn attach_handshake_reply_matches_its_golden_json_on_both_arms() {
    const ACCEPTED: &str = concat!(
        r#"{"ok":true,"kind":"ghostty-snapshot","mode":"snapshot","seq":900,"#,
        r#""server_epoch":6032428321756423947,"tab_generation":3,"#,
        r#""snapshot_cols":100,"snapshot_rows":30}"#,
    );
    const REJECTED: &str = concat!(
        r#"{"ok":false,"error":{"code":"session-mismatch","#,
        r#""message":"this session is \"a\"; the client attached to \"b\""}}"#,
    );

    let accepted = AttachHandshakeReply::Accepted(AttachAccepted {
        kind: AttachPayloadKind::GHOSTTY_SNAPSHOT.into(),
        mode: AttachMode::Snapshot,
        seq: 900,
        server_epoch: EPOCH,
        tab_generation: 3,
        snapshot_cols: 100,
        snapshot_rows: 30,
    });
    round_trip(&accepted);
    assert_eq!(serde_json::to_string(&accepted).unwrap(), ACCEPTED);
    assert_eq!(
        serde_json::from_str::<AttachHandshakeReply>(ACCEPTED).unwrap(),
        accepted
    );

    let rejected = AttachHandshakeReply::rejected(
        "session-mismatch",
        r#"this session is "a"; the client attached to "b""#,
    );
    round_trip(&rejected);
    assert_eq!(serde_json::to_string(&rejected).unwrap(), REJECTED);
    assert_eq!(
        serde_json::from_str::<AttachHandshakeReply>(REJECTED).unwrap(),
        rejected
    );
    assert_eq!(
        rejected,
        AttachHandshakeReply::Rejected(ResponseError {
            code: "session-mismatch".into(),
            message: r#"this session is "a"; the client attached to "b""#.into(),
        })
    );
}

/// The snapshot geometry rides **every** accepted arm: under protocol
/// equality there is no session that leaves it out, so a reply without
/// it is a truncated reply and not an older peer.
#[test]
fn an_accepted_handshake_reply_without_the_snapshot_geometry_is_refused() {
    const WITH: &str = concat!(
        r#"{"ok":true,"kind":"vt","mode":"snapshot","seq":900,"#,
        r#""server_epoch":6032428321756423947,"tab_generation":3,"#,
        r#""snapshot_cols":100,"snapshot_rows":30}"#,
    );

    let sized = AttachHandshakeReply::Accepted(AttachAccepted {
        kind: AttachPayloadKind::VT.into(),
        mode: AttachMode::Snapshot,
        seq: 900,
        server_epoch: EPOCH,
        tab_generation: 3,
        snapshot_cols: 100,
        snapshot_rows: 30,
    });
    round_trip(&sized);
    assert_eq!(serde_json::to_string(&sized).unwrap(), WITH);
    assert_eq!(
        serde_json::from_str::<AttachHandshakeReply>(WITH).unwrap(),
        sized
    );

    let error = serde_json::from_str::<AttachHandshakeReply>(concat!(
        r#"{"ok":true,"kind":"vt","mode":"snapshot","seq":900,"#,
        r#""server_epoch":6032428321756423947,"tab_generation":3}"#,
    ))
    .expect_err("an accepted reply that states no geometry is truncated");
    assert!(
        error.to_string().contains("snapshot_cols"),
        "the error names the missing field: {error}"
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
fn tab_dump_vectors_decode_into_their_typed_shapes() {
    for (name, scrollback) in [
        ("tab.dump.request.json", 0),
        ("tab.dump.scrollback.request.json", 50),
    ] {
        let raw = read_vector(name);
        let request: roost_ipc::messages::RawRequest =
            serde_json::from_str(&raw).expect("decode request envelope");
        assert_eq!(request.op, roost_ipc::messages::ops::TAB_DUMP, "{name}");
        let params: TabDumpParams =
            serde_json::from_value(request.params).expect("decode dump params");
        assert_eq!(
            params,
            TabDumpParams {
                tab_id: WireTabRef::Local(5),
                scrollback,
            },
            "{name}"
        );
    }

    let raw = read_vector("tab.dump.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: TabDumpResult =
        serde_json::from_value(resp.result.expect("result body")).expect("decode dump result");
    assert_eq!(
        result,
        TabDumpResult {
            cols: 80,
            rows: 24,
            cursor: Some(TabDumpCursor {
                row: 2,
                col: 0,
                visible: true,
            }),
            rows_text: vec!["/tmp $ echo hi".into(), "hi".into()],
            scrollback_rows: 3,
            scrollback_text: vec![
                "/tmp $ ls".into(),
                "README.md".into(),
                "/tmp $ clear".into(),
            ],
        }
    );
    round_trip(&result);
}

/// The compatibility matrix's "old server → new client" row: a
/// response minted before `tab.dump` grew history carries neither new
/// field, and a client built against the new shape must still read it.
#[test]
fn a_pre_scrollback_tab_dump_result_still_decodes() {
    let result: TabDumpResult = serde_json::from_str(
        r#"{"cols":80,"rows":24,"cursor":{"row":2,"col":0,"visible":true},
            "rows_text":["/tmp $ echo hi","hi"]}"#,
    )
    .expect("a pre-scrollback response must decode");
    assert_eq!(result.scrollback_rows, 0);
    assert!(result.scrollback_text.is_empty());
}

/// The strict-struct half of the same matrix, and the sharper one:
/// `TabDumpParams` is `deny_unknown_fields`, so a viewport-only dump
/// must not put the key on the wire at all or an older server rejects
/// the whole request.
#[test]
fn tab_dump_params_omit_an_unset_scrollback() {
    let viewport_only = TabDumpParams {
        tab_id: WireTabRef::Local(5),
        scrollback: 0,
    };
    assert_eq!(
        serde_json::to_string(&viewport_only).unwrap(),
        r#"{"tab_id":"5"}"#
    );
    round_trip(&viewport_only);

    let with_history = TabDumpParams {
        tab_id: WireTabRef::Local(5),
        scrollback: 50,
    };
    assert_eq!(
        serde_json::to_string(&with_history).unwrap(),
        r#"{"tab_id":"5","scrollback":50}"#
    );
    round_trip(&with_history);

    let decoded: TabDumpParams = serde_json::from_str(r#"{"tab_id":"5"}"#).unwrap();
    assert_eq!(decoded.scrollback, 0);
}

#[test]
fn session_stopping_vector_decodes_into_its_typed_shape() {
    let raw = read_vector("session.stopping.event.json");
    let envelope: EventEnvelope = serde_json::from_str(&raw).expect("decode event envelope");
    assert_eq!(envelope.event, SESSION_STOPPING_EVENT);
    let data: SessionStoppingEvent =
        serde_json::from_value(envelope.data).expect("decode stopping data");
    assert_eq!(data.reason, "stop");
    round_trip(&data);
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

/// The two effect spellings a client switches on. The constants are the
/// wire strings verbatim (`TabEffect` is a transparent newtype), so
/// renaming a constant must break this file, not a client.
#[test]
fn tab_effect_names_are_their_wire_strings() {
    assert_eq!(
        serde_json::to_string(&TabEffect::from(TabEffect::BELL)).unwrap(),
        r#""bell""#
    );
    assert_eq!(
        serde_json::to_string(&TabEffect::from(TabEffect::CLIPBOARD_WRITE)).unwrap(),
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
}

/// `TabEffect` is an open list (#188, #364), not a closed enum: a
/// session ahead of this build can name an effect this build has never
/// heard of, and the client's job is to ignore it, not refuse the whole
/// envelope. Decoding an unknown value must succeed and preserve it —
/// the opposite of the old closed-enum contract, which failed the
/// decode outright.
#[test]
fn an_unknown_effect_decodes_as_an_opaque_value_instead_of_failing() {
    let decoded: TabEffect =
        serde_json::from_str(r#""pointer-shape""#).expect("unknown effect must still decode");
    assert_eq!(decoded, TabEffect::from("pointer-shape"));
    assert_eq!(decoded.as_str(), "pointer-shape");
    // Still routes: a known effect is unaffected by the type opening up.
    let bell: TabEffect = serde_json::from_str(r#""bell""#).expect("decode bell");
    assert_eq!(bell, TabEffect::from(TabEffect::BELL));
}

/// A bell carries no payload at all — the optional fields are absent
/// from the wire rather than present and null, so a client reading
/// `data` unconditionally fails loudly on a bell instead of pasting
/// "null" into a clipboard.
#[test]
fn a_bell_effect_omits_its_payload_fields() {
    let bell = TabEffectEvent {
        tab_id: 5,
        effect: TabEffect::BELL.into(),
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
            effect: TabEffect::CLIPBOARD_WRITE.into(),
            // base64 of "hello": the payload rides encoded like every
            // other bytes field on this wire.
            data: Some("aGVsbG8=".into()),
            target: Some(ClipboardEffectTarget::System),
        }
    );
    round_trip(&data);
}

/// `error` is deliberately **not** `skip_serializing_if` — see
/// [`DurabilityChangedEvent`].
#[test]
fn a_durability_recovery_says_so_with_an_explicit_null() {
    let recovered = DurabilityChangedEvent { error: None };
    assert_eq!(
        serde_json::to_string(&recovered).unwrap(),
        r#"{"error":null}"#
    );
    round_trip(&recovered);
}

#[test]
fn durability_vectors_decode_into_their_typed_shape() {
    for (name, expected) in [
        (
            "workspace.durability_changed.event.json",
            Some("Read-only file system (os error 30)".to_string()),
        ),
        ("workspace.durability_changed.recovered.event.json", None),
    ] {
        let raw = read_vector(name);
        let envelope: EventEnvelope = serde_json::from_str(&raw).expect("decode event envelope");
        assert_eq!(
            envelope.event,
            roost_ipc::messages::ops::EVENT_WORKSPACE_DURABILITY_CHANGED
        );
        let data: DurabilityChangedEvent =
            serde_json::from_value(envelope.data).expect("decode durability data");
        assert_eq!(data.error, expected, "{name}");
        round_trip(&data);
    }
}

/// The field is additive, which is exactly what the pair of vectors
/// pins: the plain `v6` file has no `persist_error` and still decodes to
/// this build's identity, and the variant carries one.
#[test]
fn session_identify_carries_a_persist_error_only_when_there_is_one() {
    let plain = decode_identify_vector(&identify_vector_name(SESSION_PROTOCOL_VERSION));
    assert_eq!(plain.persist_error, None);
    assert!(
        !serde_json::to_string(&plain)
            .unwrap()
            .contains("persist_error"),
        "an absent durability failure is omitted, not null"
    );

    let failing = decode_identify_vector(&format!(
        "session.identify.persist_error.response.v{SESSION_PROTOCOL_VERSION}.json"
    ));
    assert_eq!(
        failing.persist_error.as_deref(),
        Some("No space left on device (os error 28)")
    );
    assert_eq!(
        SessionIdentify {
            persist_error: None,
            ..failing
        },
        sample_identify()
    );
}

#[test]
fn identify_carries_a_persist_error_only_when_there_is_one() {
    fn result(name: &str) -> IdentifyResult {
        let raw = read_vector(name);
        let resp: roost_ipc::messages::Response =
            serde_json::from_str(&raw).expect("decode response envelope");
        serde_json::from_value(resp.result.expect("result body")).expect("decode identify result")
    }

    assert_eq!(result("identify.response.json").persist_error, None);
    assert_eq!(
        result("identify.persist_error.response.json")
            .persist_error
            .as_deref(),
        Some("Read-only file system (os error 30)")
    );
}

#[test]
fn session_set_theme_vectors_decode_into_their_typed_shapes() {
    let raw = read_vector("session.set_theme.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(request.op, roost_ipc::messages::ops::SESSION_SET_THEME);
    let params: SessionSetThemeParams =
        serde_json::from_value(request.params).expect("decode set_theme params");
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
            "osc_colors": colors.clone(),
        }))
        .is_ok()
    );
    assert!(
        serde_json::from_value::<SessionSetThemeParams>(serde_json::json!({
            "osc_colors": colors,
            "tab_id": "5",
        }))
        .is_err()
    );
}

// ---------------------------------------------------------------------------
// tab.clear_notification + notification.fired (#474)
// ---------------------------------------------------------------------------

#[test]
fn clear_notification_vectors_decode_into_their_typed_shapes() {
    let raw = read_vector("tab.clear_notification.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(request.op, ops::TAB_CLEAR_NOTIFICATION);
    let params: TabClearNotificationParams =
        serde_json::from_value(request.params).expect("decode clear params");
    assert_eq!(params.tab_id, 3);
    assert_eq!(
        params.generation, None,
        "the plain form is a person answering the tab"
    );
    round_trip(&params);

    let raw = read_vector("tab.clear_notification.generation.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    let params: TabClearNotificationParams =
        serde_json::from_value(request.params).expect("decode an acknowledgement");
    assert_eq!(params.tab_id, 3);
    assert_eq!(params.generation, Some(7));
    round_trip(&params);

    let raw = read_vector("tab.clear_notification.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: TabClearNotificationResult =
        serde_json::from_value(resp.result.expect("a result")).expect("decode the result");
    assert!(result.cleared);
}

/// `generation` is omit-when-unset, which is what makes it additive: a
/// clear from a peer that predates it is the unconditional form, and
/// nothing in the encoder can emit a `null` a strict decoder would have
/// to have a rule for.
#[test]
fn a_clear_without_a_generation_omits_the_field_entirely() {
    let bare = TabClearNotificationParams {
        tab_id: 3,
        generation: None,
    };
    assert_eq!(
        serde_json::to_value(&bare).expect("serialize"),
        serde_json::json!({"tab_id": "3"}),
    );
    let named = TabClearNotificationParams {
        tab_id: 3,
        generation: Some(7),
    };
    assert_eq!(
        serde_json::to_value(&named).expect("serialize"),
        serde_json::json!({"tab_id": "3", "generation": 7}),
    );

    // And the omission decodes back to the unconditional form rather
    // than being refused, which is the other half of "additive".
    let decoded: TabClearNotificationParams =
        serde_json::from_value(serde_json::json!({"tab_id": "3"})).expect("decode");
    assert_eq!(decoded.generation, None);
}

#[test]
fn notification_fired_vector_decodes_with_its_generation() {
    let raw = read_vector("notification.fired.event.json");
    let envelope: roost_ipc::messages::EventEnvelope =
        serde_json::from_str(&raw).expect("decode event envelope");
    assert_eq!(envelope.event, ops::EVENT_NOTIFICATION_FIRED);
    let fired: NotificationFiredEvent =
        serde_json::from_value(envelope.data).expect("decode notification.fired");
    assert_eq!(fired.tab_id, 5);
    assert_eq!(fired.generation, 7);
    round_trip(&fired);
}

/// A peer that predates the field decodes to generation `0` — a value
/// no raise ever mints, so it reads as "this fire named none" and is
/// acknowledged unconditionally rather than with a number the engine
/// could never match.
#[test]
fn a_fired_notification_without_a_generation_decodes_to_zero() {
    let fired: NotificationFiredEvent = serde_json::from_value(serde_json::json!({
        "tab_id": "5",
        "title": "Claude Code",
        "body": "Turn complete",
    }))
    .expect("decode a pre-generation event");
    assert_eq!(fired.generation, 0);
}

// ---------------------------------------------------------------------------
// session.set_agent_hooks (plan 046 C8, reshaped into a raise by plan 064
// C3 — see docs/reference/ipc-compatibility.md's generation-6 entry)
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
    assert_eq!(
        params.agents,
        vec!["claude".to_string(), "codex".to_string()]
    );
    assert_eq!(params.client, "charlie-mbp");
    round_trip(&params);

    let raw = read_vector("session.set_agent_hooks.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: AgentHooksOutcome = serde_json::from_value(resp.result.expect("result"))
        .expect("decode set_agent_hooks result");
    assert_eq!(
        result.wired,
        vec!["claude".to_string(), "codex".to_string()]
    );
    assert!(result.refreshed.is_empty());
    // Always empty from this op now: a raise only ever widens, so there
    // is nothing for it to report as taken out (plan 064 §3.3).
    assert!(result.removed.is_empty());
    assert!(result.errors.is_empty());
    let reasons: Vec<(&str, &str)> = result
        .skipped
        .iter()
        .map(|s| (s.agent.as_str(), s.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        vec![("cursor", "not allowed"), ("grok", "not installed")]
    );
    round_trip(&result);
}

/// `agents` is the whole decision now: a raise-only allow-list, with no
/// wire spelling left for `off` or a narrowing (plan 064 §3.3). Strict
/// like every other request type — an unknown field, or a missing
/// required one, is refused rather than silently accepted.
#[test]
fn session_set_agent_hooks_params_are_strict_about_shape() {
    let ok = serde_json::json!({
        "agents": ["claude"],
        "client": "charlie-mbp",
    });
    assert!(serde_json::from_value::<SessionSetAgentHooksParams>(ok).is_ok());

    // `agents` is required — there is no default allow-list, and an
    // absent field is a client that forgot to say, not `off` or `ask`.
    let missing = serde_json::from_value::<SessionSetAgentHooksParams>(serde_json::json!({
        "client": "charlie-mbp",
    }))
    .expect_err("an omitted agents list must not decode");
    assert!(missing.to_string().contains("missing field"), "{missing}");

    // `client` is required too — the host's record has to name who asked.
    let missing_client = serde_json::from_value::<SessionSetAgentHooksParams>(serde_json::json!({
        "agents": ["claude"],
    }))
    .expect_err("an omitted client must not decode");
    assert!(
        missing_client.to_string().contains("missing field"),
        "{missing_client}"
    );

    assert!(
        serde_json::from_value::<SessionSetAgentHooksParams>(serde_json::json!({
            "agents": ["claude"],
            "client": "charlie-mbp",
            "tab_id": "5",
        }))
        .is_err(),
        "strict like every other request type"
    );
}

/// The retired `mode` + `skip` shape fails to decode rather than
/// silently reinterpreting `mode` as an agent name or dropping `skip` on
/// the floor. Protocol equality (plan 061) means a v5-shaped frame can
/// never actually arrive from a real peer — a session too old to send
/// this shape is also too old to pass the `session-mismatch` check at
/// attach — but the type itself must not paper over it either.
#[test]
fn session_set_agent_hooks_rejects_the_pre_reshape_v5_wire_shape() {
    let v5_shaped = serde_json::json!({
        "mode": "auto",
        "skip": ["cursor"],
        "client": "charlie-mbp",
    });
    assert!(serde_json::from_value::<SessionSetAgentHooksParams>(v5_shaped).is_err());
}

// ---------------------------------------------------------------------------
// agent.set_hooks (plan 064 §3.4) — UI-socket types only; no server
// implementation ships until C6 (iced) / C8 (Mac).
// ---------------------------------------------------------------------------

#[test]
fn agent_set_hooks_vectors_decode_into_their_typed_shapes() {
    let raw = read_vector("agent.set_hooks.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    assert_eq!(request.op, roost_ipc::messages::ops::AGENT_SET_HOOKS);
    let params: AgentSetHooksParams =
        serde_json::from_value(request.params).expect("decode agent.set_hooks params");
    assert_eq!(
        params.agents,
        AgentSetHooksAgents::List(vec!["claude".to_string(), "codex".to_string()])
    );
    round_trip(&params);

    let raw = read_vector("agent.set_hooks.off.request.json");
    let request: roost_ipc::messages::RawRequest =
        serde_json::from_str(&raw).expect("decode request envelope");
    let off: AgentSetHooksParams =
        serde_json::from_value(request.params).expect("decode an off agent.set_hooks request");
    assert_eq!(off.agents, AgentSetHooksAgents::Off);
    round_trip(&off);

    let raw = read_vector("agent.set_hooks.response.json");
    let resp: roost_ipc::messages::Response =
        serde_json::from_str(&raw).expect("decode response envelope");
    assert!(resp.ok);
    let result: AgentSetHooksResult = serde_json::from_value(resp.result.expect("result"))
        .expect("decode agent.set_hooks result");
    assert_eq!(
        result.config_path,
        "/home/charlie/.config/roost/config.conf"
    );
    assert_eq!(
        result.local.wired,
        vec!["claude".to_string(), "codex".to_string()]
    );
    assert_eq!(result.hosts.len(), 2);
    round_trip(&result);
}

/// `agents` accepts a list of any length, including empty — whether an
/// empty list is a *valid* request is the server's call (`invalid-param`,
/// plan 064 C6), not this type's. It also accepts the one word `off`,
/// and rejects everything else: a two-variant `#[serde(untagged)]` enum
/// would have let any string through as `Off`, silently. `[]` and other
/// non-`"off"` strings are exercised together with the shape checks
/// below rather than split out, since both are about what the wire
/// format itself does or does not carry.
#[test]
fn agent_set_hooks_agents_accepts_lists_and_off_only() {
    assert_eq!(
        serde_json::from_value::<AgentSetHooksAgents>(serde_json::json!([])).unwrap(),
        AgentSetHooksAgents::List(vec![])
    );
    assert_eq!(
        serde_json::from_value::<AgentSetHooksAgents>(serde_json::json!(["claude"])).unwrap(),
        AgentSetHooksAgents::List(vec!["claude".to_string()])
    );
    assert_eq!(
        serde_json::from_value::<AgentSetHooksAgents>(serde_json::json!("off")).unwrap(),
        AgentSetHooksAgents::Off
    );

    for bad in [
        serde_json::json!("Off"),
        serde_json::json!("none"),
        serde_json::json!("ask"),
        serde_json::json!(true),
        serde_json::json!(5),
        serde_json::json!({"agents": ["claude"]}),
    ] {
        assert!(
            serde_json::from_value::<AgentSetHooksAgents>(bad.clone()).is_err(),
            "{bad} must not decode"
        );
    }
}

/// `off` round-trips back to the exact string `"off"`, not to `null` or
/// an empty array — the failure mode a naively-derived untagged enum
/// would have produced.
#[test]
fn agent_set_hooks_agents_off_serializes_to_the_literal_string() {
    assert_eq!(
        serde_json::to_value(AgentSetHooksAgents::Off).unwrap(),
        serde_json::json!("off")
    );
}

#[test]
fn agent_set_hooks_params_are_strict_about_shape() {
    assert!(
        serde_json::from_value::<AgentSetHooksParams>(serde_json::json!({
            "agents": ["claude"],
        }))
        .is_ok()
    );

    assert!(
        serde_json::from_value::<AgentSetHooksParams>(serde_json::json!({
            "agents": ["claude"],
            "client": "charlie-mbp",
        }))
        .is_err(),
        "unlike session.set_agent_hooks this op has no client field —          the UI socket already knows who is asking"
    );

    let missing = serde_json::from_value::<AgentSetHooksParams>(serde_json::json!({}))
        .expect_err("an omitted agents field must not decode");
    assert!(missing.to_string().contains("missing field"), "{missing}");
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
    const PARAMS: &str = r#"{"name":"shot.png","data":"aGVsbG8="}"#;
    let params = SessionPutFileParams {
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
        r#"{"name":"a.png","data":"aGVsbG8=","mode":"0600"}"#
    )
    .is_err());
    // Malformed base64 is a decode failure, so the engine never sees a
    // half-decoded payload.
    assert!(serde_json::from_str::<SessionPutFileParams>(r#"{"name":"a","data":"!!"}"#).is_err());

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
    const GOLDEN: &str = r#"{"tab_id_filter":"0"}"#;

    let plain = EventsSubscribeParams {
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
    let error =
        serde_json::from_str::<EventsSubscribeParams>(r#"{"tab_id_filter":"0","from_rev":9}"#)
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
