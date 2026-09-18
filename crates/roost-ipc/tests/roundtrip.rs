//! Integration round-trip tests for the IPC wire format. These run
//! against the library's public API the same way external callers
//! would.

use roost_ipc::agent::AgentLifecycle;
use roost_ipc::messages::*;
use roost_ipc::LocalBackendMode;

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
        activate: None,
    };
    let json = serde_json::to_value(&params).unwrap();
    assert_eq!(json["project_id"], "17");
    assert_eq!(json["cols"], 120);
}

/// Both `tab.open` request vectors decode as the typed params and
/// re-encode to their own params: the recorded one has no `activate` and
/// keeps not having one (#503).
#[test]
fn tab_open_vectors_decode_with_and_without_activate() {
    for (name, activate) in [
        ("tab.open.request.json", None),
        ("tab.open.activate-false.request.json", Some(false)),
    ] {
        let path = format!(
            "{}/../../tests/ipc-vectors/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let request: RawRequest = serde_json::from_str(&raw).expect(name);
        assert_eq!(request.op, ops::TAB_OPEN, "{name}");
        let params: TabOpenParams = serde_json::from_value(request.params.clone()).expect(name);
        assert_eq!(params.activate, activate, "{name}");
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            request.params,
            "{name}"
        );
    }
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
        title: "Claude Code · roost · slauth-refactor".into(),
        subtitle: None,
        agent: Some(PaletteAgentRow {
            effective_lifecycle: AgentLifecycle::Waiting,
            agent: "Claude Code".into(),
            project: "roost".into(),
            name: "slauth-refactor".into(),
            status_text: "Waiting for input".into(),
            time_text: "2m".into(),
            metrics_text: Some("4f +86 -12".into()),
        }),
    };
    let pending_metrics = PaletteItemView {
        id: "agent:4".into(),
        title: "Claude Code · roost · pending-metrics".into(),
        subtitle: None,
        agent: Some(PaletteAgentRow {
            effective_lifecycle: AgentLifecycle::Working,
            agent: "Claude Code".into(),
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
    assert_eq!(agent.agent, "Claude Code");
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

    /// A recorded `host.status` result, plus the always-serialized keys
    /// [`HostStatus`] has grown since it was recorded.
    ///
    /// An existing vector is never edited to bless a wire change
    /// (`ipc-compatibility.md`), so the expectation moves instead — and
    /// pinning each grown key to the value a vector that omits it must
    /// decode to is a stronger check than dropping it from both sides.
    fn grown(recorded: &serde_json::Value) -> serde_json::Value {
        let mut expected = recorded.clone();
        for host in expected["hosts"].as_array_mut().expect("a hosts array") {
            host["tabs"] = serde_json::json!(0);
        }
        expected
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
    // #399: the band's own `reason` is the armed-rung line; the rung's
    // is why it was armed. Two different sentences in one payload.
    assert_eq!(armed.reason.as_deref(), Some("reconnecting in 8s (3/10)"));
    assert!(armed
        .retry
        .as_ref()
        .and_then(|retry| retry.reason.as_deref())
        .is_some_and(|why| why.contains("Connection refused")));
    // The never-connected host is the shape a caller must be ready for:
    // every optional omitted, `generation` still present at `0`.
    let never = &result.hosts[1];
    assert_eq!(never.generation, 0);
    assert_eq!(never.retry, None);
    assert_eq!(never.rollup, None);
    // Nothing has attached on either host, so neither names a payload
    // kind — the variant vector below is where one does.
    assert_eq!(armed.payload_kind, None);
    assert_eq!(never.payload_kind, None);
    // Neither host is live, so neither carries a `connect` object.
    assert_eq!(armed.connect, None);
    assert_eq!(never.connect, None);
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        grown(&response["result"])
    );

    // The fallback shape: a connected host whose attach was served as a
    // `vt` byte stream because the two libghostty builds disagree.
    let response = vector("host.status.vt.response.json");
    let result: HostStatusResult =
        serde_json::from_value(response["result"].clone()).expect("host.status vt result decode");
    assert_eq!(
        result.hosts[0].payload_kind.as_deref(),
        Some(AttachPayloadKind::VT)
    );
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        grown(&response["result"])
    );

    // The live shape: what the prologue established, plus the row count
    // the section is drawing. `connect` is present exactly while the
    // connection is, which is why the two vectors above omit it.
    let response = vector("host.status.connect.response.json");
    let result: HostStatusResult = serde_json::from_value(response["result"].clone())
        .expect("host.status connect result decode");
    let live = result.hosts[0].connect.as_ref().expect("a live connection");
    assert_eq!(live.session_id, "5c1f0e2d3a4b5c6d");
    assert!(live.reduced_fidelity);
    assert!(live.resumed);
    assert_eq!(live.from_revision, Some(4_312));
    assert_eq!(result.hosts[0].tabs, 5);
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
            "agent": "Claude Code", "project": "p", "name": "n", "status_text": "s",
            "time_text": "1s"}}"#,
    )
    .unwrap();
    assert!(ok.agent.is_some(), "well-formed agent must still decode");
}

#[test]
fn reorder_tab_ids_serialize_as_string_array() {
    let p = TabReorderParams {
        project_id: WireProjectRef::Local(1),
        tab_ids: vec![
            WireTabRef::Local(5),
            WireTabRef::Local(3),
            WireTabRef::Local(1),
        ],
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

/// The plan-047 file-transfer ops. Both vectors are decoded into the
/// typed shapes *and* re-encoded against the fixture, so a field the
/// struct spells differently — or a `bytes` written as a string —
/// fails here rather than at a session's socket.
#[test]
fn file_transfer_vectors_decode_as_typed_params_and_results() {
    fn vector(name: &str) -> serde_json::Value {
        let path = format!(
            "{}/../../tests/ipc-vectors/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    let request = vector("session.put_file.request.json");
    assert_eq!(request["op"], ops::SESSION_PUT_FILE);
    let params: SessionPutFileParams =
        serde_json::from_value(request["params"].clone()).expect("session.put_file params decode");
    assert_eq!(params.name, "roost-image-1757083567-8f3a1d0e5b7c42c2.png");
    assert!(params.data.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(serde_json::to_value(&params).unwrap(), request["params"]);

    let response = vector("session.put_file.response.json");
    let result: SessionPutFileResult =
        serde_json::from_value(response["result"].clone()).expect("session.put_file result decode");
    // The three properties the client re-checks before it pastes:
    // absolute, in the paste-safe grammar, and named what was sent.
    assert!(result.path.starts_with('/'));
    assert!(result
        .path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c)));
    assert!(result.path.ends_with(&format!("/{}", params.name)));
    assert_eq!(result.bytes as usize, params.data.len());
    assert_eq!(serde_json::to_value(&result).unwrap(), response["result"]);

    let request = vector("tab.send_file.request.json");
    assert_eq!(request["op"], ops::TAB_SEND_FILE);
    let params: TabSendFileParams =
        serde_json::from_value(request["params"].clone()).expect("tab.send_file params decode");
    assert_eq!(params.tab, "h2.7");
    assert!(params.paths.iter().all(|p| p.starts_with('/')));
    assert_eq!(serde_json::to_value(&params).unwrap(), request["params"]);

    let response = vector("tab.send_file.response.json");
    let result: TabSendFileResult =
        serde_json::from_value(response["result"].clone()).expect("tab.send_file result decode");
    assert_eq!(result.uploads.len(), 1);
    assert_eq!(result.uploads[0].source, params.paths[0]);
    assert_eq!(result.uploads[0].name, "shot.png");
    assert_eq!(result.pasted, result.uploads[0].path);
    assert_eq!(result.skipped.len(), 1);
    assert_eq!(result.skipped[0].path, params.paths[1]);
    assert_eq!(result.skipped[0].reason, "directory");
    assert_eq!(serde_json::to_value(&result).unwrap(), response["result"]);
}

/// The two `identify` vectors are the wire's record of plan 063's
/// additive response fields, and the pair is the point: the pre-063 file
/// is what a Swift UI still answers, the session one is what an iced UI
/// answers under `local-backend = session`. Decoding both against the
/// *current* struct is the old-server → new-client direction of the
/// compatibility matrix, which is the direction `#[serde(default)]` on
/// those two fields exists to satisfy.
#[test]
fn both_identify_vectors_decode_as_the_current_typed_result() {
    fn vector(name: &str) -> serde_json::Value {
        let path = format!(
            "{}/../../tests/ipc-vectors/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    let pre_063 = vector("identify.response.json");
    let result: IdentifyResult =
        serde_json::from_value(pre_063["result"].clone()).expect("pre-063 identify decodes");
    assert_eq!(result.local_backend, LocalBackendMode::InProcess);
    assert_eq!(result.local_session_socket, None);
    // Re-encoding is deliberately NOT byte-compared: an in-process reply
    // now carries `local_backend`, which is exactly the additive
    // new-server → old-client change the vector predates. The vector
    // stays as recorded (`docs/reference/ipc-compatibility.md`).
    assert_eq!(result.active_tab_id, 3);

    let session = vector("identify.session.response.json");
    let result: IdentifyResult =
        serde_json::from_value(session["result"].clone()).expect("session identify decodes");
    assert_eq!(result.local_backend, LocalBackendMode::Session);
    assert_eq!(
        result.local_session_socket.as_deref(),
        Some("/run/user/1000/roost-session/roost.sock")
    );
    // Under `session` the ids on the wire are the slot's selection, not
    // this socket's own workspace — the reason the field pair exists.
    assert_eq!((result.active_project_id, result.active_tab_id), (4, 9));
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        session["result"],
        "typed re-encode must match the vector"
    );
}

/// The same pair for `app.sidebar_dump` and plan 063 §D2's band strip:
/// the pre-063 file is a dump with `hosts` and no `sections` (still what
/// a Swift UI and an `in-process` UI with no saved hosts answer), the
/// session one is what an iced UI answers under `local-backend =
/// session` — a leading band that is itself a saved host.
#[test]
fn both_sidebar_dump_vectors_decode_as_the_current_typed_result() {
    fn vector(name: &str) -> serde_json::Value {
        let path = format!(
            "{}/../../tests/ipc-vectors/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    let pre_063 = vector("app.sidebar_dump.response.json");
    let result: SidebarDumpResult =
        serde_json::from_value(pre_063["result"].clone()).expect("pre-063 sidebar dump decodes");
    assert!(
        result.sections.is_empty(),
        "the recorded vector predates the strip and stays as recorded"
    );
    assert_eq!(result.hosts.len(), 2);
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        pre_063["result"],
        "an empty strip is omitted, so the pre-063 shape re-encodes exactly"
    );

    let session = vector("app.sidebar_dump.session.response.json");
    let result: SidebarDumpResult =
        serde_json::from_value(session["result"].clone()).expect("session sidebar dump decodes");
    let bands = &result.sections;
    assert_eq!(bands.len(), 2);
    // The session-only band: `PROJECTS`, but a saved host's own state
    // and dot — which is the whole of §D2 row 3 on the wire.
    assert_eq!(bands[0].role, "session");
    assert_eq!(bands[0].label, "PROJECTS");
    assert_eq!(bands[0].dot, "connected");
    assert_eq!(bands[0].saved_id.as_deref(), Some("hs-2f1c"));
    assert!(!bands[0].reconnect_row);
    // The slot leads the strip, so its band is *not* at its own
    // registry index + 1 — the pairing is `saved_id`, not position.
    assert_eq!(bands[1].role, "host");
    assert_eq!(bands[1].saved_id.as_deref(), Some("hs-9d40"));
    assert!(bands[1].reconnect_row);
    assert_eq!(
        result.hosts.first().map(|host| host.id.as_str()),
        Some("hs-2f1c"),
        "the slot is still listed among the saved hosts it belongs to"
    );
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        session["result"],
        "typed re-encode must match the vector"
    );
}
