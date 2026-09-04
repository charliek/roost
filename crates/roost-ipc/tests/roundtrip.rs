//! Integration round-trip tests for the IPC wire format. These run
//! against the library's public API the same way external callers
//! would.

use roost_ipc::agent::AgentLifecycle;
use roost_ipc::messages::*;

fn round_trip_to_value<T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug>(
    v: &T,
) -> serde_json::Value {
    let json = serde_json::to_value(v).expect("serialize");
    let back: T = serde_json::from_value(json.clone()).expect("deserialize");
    let json2 = serde_json::to_value(&back).expect("re-serialize");
    assert_eq!(json, json2, "value drifted under round-trip");
    json
}

#[test]
fn identify_request_envelope() {
    let raw = RawRequest {
        id: 1,
        op: ops::IDENTIFY.into(),
        params: serde_json::to_value(IdentifyParams {
            client_name: "roostctl".into(),
            client_version: "0.6.0".into(),
        })
        .unwrap(),
    };
    let json = round_trip_to_value(&raw);
    assert_eq!(json["id"], "1");
    assert_eq!(json["op"], "identify");
}

#[test]
fn tab_open_request_envelope_uses_string_ids() {
    let params = TabOpenParams {
        project_id: 17,
        cwd: "/tmp".into(),
        argv: vec!["/bin/zsh".into()],
        cols: 120,
        rows: 30,
        title: "".into(),
    };
    let json = serde_json::to_value(&params).unwrap();
    assert_eq!(json["project_id"], "17");
    assert_eq!(json["cols"], 120);
}

#[test]
fn tab_write_data_round_trips_as_base64() {
    let p = TabWriteParams {
        tab_id: 5,
        data: b"ls -la\n".to_vec(),
    };
    let json = round_trip_to_value(&p);
    assert_eq!(
        json["data"],
        serde_json::Value::String("bHMgLWxhCg==".into())
    );
}

#[test]
fn response_ok_envelope_round_trip() {
    let r = Response::ok(42, serde_json::json!({"foo": "bar"}));
    let json = round_trip_to_value(&r);
    assert_eq!(json["id"], "42");
    assert_eq!(json["ok"], true);
    assert_eq!(json["result"]["foo"], "bar");
}

#[test]
fn response_err_envelope_round_trip() {
    let r = Response::err(42, "unknown-op", "no such op: foo");
    let json = round_trip_to_value(&r);
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "unknown-op");
}

#[test]
fn event_envelope_round_trip() {
    let ev = EventEnvelope {
        event: ops::EVENT_TAB_OPENED.into(),
        data: serde_json::to_value(TabOpenedEvent {
            tab: Tab {
                id: 1,
                project_id: 1,
                title: "shell".into(),
                cwd: "/".into(),
                state: TabState::None,
                has_notification: false,
                is_active: true,
                user_titled: false,
                position: 0,
                created_at: 1_700_000_000,
                last_active: 1_700_000_000,
                hook_active: false,
                shell_state: Default::default(),
                agent_lifecycle: Default::default(),
                ownership: None,
            },
        })
        .unwrap(),
    };
    let json = round_trip_to_value(&ev);
    assert_eq!(json["event"], "tab.opened");
}

#[test]
fn tab_state_enum_values() {
    for state in [
        TabState::None,
        TabState::Running,
        TabState::NeedsInput,
        TabState::Idle,
    ] {
        let json = serde_json::to_value(state).unwrap();
        let back: TabState = serde_json::from_value(json).unwrap();
        assert_eq!(back, state);
    }
}

#[test]
fn palette_state_agent_rows_round_trip_and_omit_absent_fields() {
    let with_agent = PaletteItemView {
        id: "agent:3".into(),
        title: "roost · slauth-refactor".into(),
        subtitle: None,
        agent: Some(PaletteAgentRow {
            effective_lifecycle: AgentLifecycle::Waiting,
            project: "roost".into(),
            name: "slauth-refactor".into(),
            status_text: "Waiting for input".into(),
            time_text: "2m".into(),
            metrics_text: Some("4f +86 -12".into()),
        }),
    };
    let pending_metrics = PaletteItemView {
        id: "agent:4".into(),
        title: "roost · pending-metrics".into(),
        subtitle: None,
        agent: Some(PaletteAgentRow {
            effective_lifecycle: AgentLifecycle::Working,
            project: "roost".into(),
            name: "pending-metrics".into(),
            status_text: "Working".into(),
            time_text: "41s".into(),
            metrics_text: None,
        }),
    };
    let no_agent = PaletteItemView {
        id: "new_tab".into(),
        title: "New Tab".into(),
        subtitle: None,
        agent: None,
    };

    let result = PaletteStateResult {
        open: true,
        frame: Some("agents".into()),
        query: "".into(),
        selection: 0,
        items: vec![
            with_agent.clone(),
            pending_metrics.clone(),
            no_agent.clone(),
        ],
        selected_in_view: Some(true),
    };
    let json = round_trip_to_value(&result);
    let back: PaletteStateResult = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(back, result);

    let no_agent_json = serde_json::to_string(&no_agent).unwrap();
    assert!(
        !no_agent_json.contains("agent"),
        "non-agent row must omit the agent key: {no_agent_json}"
    );

    let pending_json = serde_json::to_string(&pending_metrics).unwrap();
    assert!(
        !pending_json.contains("metrics_text"),
        "pending metrics must omit metrics_text: {pending_json}"
    );
    assert!(pending_json.contains("\"agent\""));
}

#[test]
fn agents_vector_file_decodes_as_typed_palette_state() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/ipc-vectors/palette.state.agents.response.json"
    ))
    .expect("vector file readable");
    let envelope: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let result: PaletteStateResult =
        serde_json::from_value(envelope["result"].clone()).expect("vector decodes typed");
    assert_eq!(result.frame.as_deref(), Some("agents"));
    let agent = result.items[0].agent.as_ref().expect("agent payload");
    assert_eq!(agent.effective_lifecycle, AgentLifecycle::Waiting);
    assert_eq!(agent.status_text, "Waiting for input");
    assert_eq!(agent.time_text, "2m");
    assert_eq!(agent.metrics_text.as_deref(), Some("4f +86 -12"));
    let reencoded = serde_json::to_value(&result).unwrap();
    assert_eq!(
        reencoded, envelope["result"],
        "typed re-encode must match the vector byte-for-byte"
    );
}

/// The host connection ops, decoded as the types the engine and
/// `roostctl` actually use. `deny_unknown_fields` on the params makes
/// this a real check on both directions: a vector that grew a field the
/// struct has no name for fails here rather than at a user's socket.
#[test]
fn host_connection_vectors_decode_as_typed_params_and_results() {
    fn vector(name: &str) -> serde_json::Value {
        let path = format!(
            "{}/../../tests/ipc-vectors/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    let request = vector("host.connect.request.json");
    assert_eq!(request["op"], ops::HOST_CONNECT);
    let params: HostConnectParams =
        serde_json::from_value(request["params"].clone()).expect("host.connect params decode");
    assert_eq!(params.id, "3f9a2b7c1d4e4f5a");
    assert_eq!(serde_json::to_value(&params).unwrap(), request["params"]);

    let response = vector("host.connect.response.json");
    let result: HostConnectionResult =
        serde_json::from_value(response["result"].clone()).expect("host.connect result decode");
    assert_eq!(result.state, host_state::CONNECTING);
    assert_eq!(result.host.label, "pop-os");
    assert_eq!(serde_json::to_value(&result).unwrap(), response["result"]);

    let request = vector("host.disconnect.request.json");
    assert_eq!(request["op"], ops::HOST_DISCONNECT);
    let params: HostDisconnectParams =
        serde_json::from_value(request["params"].clone()).expect("host.disconnect params decode");
    assert_eq!(params.id, result.host.id);

    let response = vector("host.disconnect.response.json");
    let result: HostConnectionResult =
        serde_json::from_value(response["result"].clone()).expect("host.disconnect result decode");
    assert_eq!(result.state, host_state::DISCONNECTED);
    assert_eq!(serde_json::to_value(&result).unwrap(), response["result"]);

    let request = vector("host.status.request.json");
    assert_eq!(request["op"], ops::HOST_STATUS);
    let params: HostStatusParams =
        serde_json::from_value(request["params"].clone()).expect("host.status params decode");
    assert!(params.id.is_none(), "the vector is the all-hosts form");
    assert_eq!(serde_json::to_value(&params).unwrap(), request["params"]);

    let response = vector("host.status.response.json");
    let result: HostStatusResult =
        serde_json::from_value(response["result"].clone()).expect("host.status result decode");
    let armed = &result.hosts[0];
    assert_eq!(armed.generation, 3);
    assert_eq!(
        armed.retry.as_ref().map(|retry| retry.attempt),
        Some(Some(3))
    );
    // The never-connected host is the shape a caller must be ready for:
    // every optional omitted, `generation` still present at `0`.
    let never = &result.hosts[1];
    assert_eq!(never.generation, 0);
    assert_eq!(never.retry, None);
    assert_eq!(never.rollup, None);
    assert_eq!(serde_json::to_value(&result).unwrap(), response["result"]);
}

#[test]
fn malformed_agent_payload_decodes_to_none_not_error() {
    for junk in [
        r#"{"id": "x", "title": "t", "agent": "garbage"}"#,
        r#"{"id": "x", "title": "t", "agent": {"effective_lifecycle": "no-such"}}"#,
        r#"{"id": "x", "title": "t", "agent": 7}"#,
    ] {
        let item: PaletteItemView =
            serde_json::from_str(junk).expect("malformed agent must not fail the item");
        assert_eq!(item.agent, None);
    }
    let ok: PaletteItemView = serde_json::from_str(
        r#"{"id": "x", "title": "t", "agent": {"effective_lifecycle": "waiting",
            "project": "p", "name": "n", "status_text": "s", "time_text": "1s"}}"#,
    )
    .unwrap();
    assert!(ok.agent.is_some(), "well-formed agent must still decode");
}

#[test]
fn reorder_tab_ids_serialize_as_string_array() {
    let p = TabReorderParams {
        project_id: 1,
        tab_ids: vec![5, 3, 1],
    };
    let json = serde_json::to_value(&p).unwrap();
    assert_eq!(
        json["tab_ids"],
        serde_json::Value::Array(vec![
            serde_json::Value::String("5".into()),
            serde_json::Value::String("3".into()),
            serde_json::Value::String("1".into()),
        ])
    );
}
