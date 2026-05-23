//! Integration round-trip tests for the IPC wire format. These run
//! against the library's public API the same way external callers
//! would.

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
