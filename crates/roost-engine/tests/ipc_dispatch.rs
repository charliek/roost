//! End-to-end IPC smoke. Spins up an `IpcServer` against a temp
//! Unix socket backed by the real `IpcHandler` (in-process
//! `Workspace` + `PtySupervisor`), then dials it with the
//! `IpcClient` and exercises a short scripted scenario.

use std::sync::Arc;
use std::time::Duration;

use roost_engine::ipc::IpcHandler;
use roost_engine::{PtySupervisor, Workspace};
use roost_ipc::messages::{
    ops, IdentifyParams, IdentifyResult, ProjectCreateParams, ProjectCreateResult, TabListResult,
    TabOpenParams, TabOpenResult,
};
use roost_ipc::IpcClient;
use roost_ipc::IpcServer;
use tempfile::tempdir;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identify_create_project_open_tab_list() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");
    let state_path = dir.path().join("state.json");

    let workspace = Arc::new(Workspace::open(state_path.clone()));
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace.clone(),
        supervisor.clone(),
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut client = connect_with_retry(&server_socket).await;

    // identify
    let id: IdentifyResult = client
        .call(
            ops::IDENTIFY,
            IdentifyParams {
                client_name: "test".into(),
                client_version: "0".into(),
            },
        )
        .await
        .expect("identify");
    assert_eq!(id.app_label, "Roost-test");
    assert!(id.pid > 0);
    assert_eq!(id.protocol_version, roost_ipc::PROTOCOL_VERSION);

    // project.create
    let proj: ProjectCreateResult = client
        .call(
            ops::PROJECT_CREATE,
            ProjectCreateParams {
                name: "Hello".into(),
                cwd: "/tmp".into(),
            },
        )
        .await
        .expect("project.create");
    assert_eq!(proj.project.name, "Hello");

    // tab.open — spawn a short-lived shell so the test doesn't leak.
    let tab: TabOpenResult = client
        .call(
            ops::TAB_OPEN,
            TabOpenParams {
                project_id: proj.project.id,
                cwd: "/tmp".into(),
                argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
                cols: 80,
                rows: 24,
                title: "".into(),
            },
        )
        .await
        .expect("tab.open");
    assert_eq!(tab.tab.project_id, proj.project.id);
    assert!(tab.tab.is_active);

    // tab.list
    let list: TabListResult = client
        .call(ops::TAB_LIST, serde_json::json!({}))
        .await
        .expect("tab.list");
    assert_eq!(list.projects.len(), 1);
    assert_eq!(list.projects[0].tabs.len(), 1);

    // Let the shell exit + supervisor reap it. Not asserting on
    // it (timing-sensitive) — the spawn+exit smoke is already
    // covered in pty_smoke.rs.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_op_returns_unknown_op_error() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");

    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace,
        supervisor,
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;
    let err = client
        .call_raw("not.a.real.op", serde_json::json!({}))
        .await
        .expect_err("expected error");
    match err {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "unknown-op"),
        other => panic!("expected Server error, got {other:?}"),
    }
}

/// #80/#9: `events.subscribe` returns `not-implemented` rather than a
/// false `{}` ACK — the server never pushes events yet, so a client
/// must learn it can't subscribe and fall back (e.g. poll tab.list).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_subscribe_returns_not_implemented() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");

    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace,
        supervisor,
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;
    let err = client
        .call_raw(ops::EVENTS_SUBSCRIBE, serde_json::json!({}))
        .await
        .expect_err("expected error");
    match err {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "not-implemented"),
        other => panic!("expected Server error, got {other:?}"),
    }
}

/// `tab.feed_ime`'s cursor range is validated at the dispatcher, ahead
/// of any UI round trip — an inverted range must fail `invalid-param`
/// even with no UI attached (this test's handler has no `ui_tx`),
/// proving the check happens before `ui_call` rather than surfacing
/// as a confusing "no UI attached" internal error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tab_feed_ime_rejects_inverted_cursor_range() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");

    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace,
        supervisor,
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;
    let err = client
        .call_raw(
            ops::TAB_FEED_IME,
            serde_json::json!({
                "tab_id": "1",
                "action": "preedit",
                "text": "hi",
                "cursor_start": 5,
                "cursor_end": 2,
            }),
        )
        .await
        .expect_err("expected error");
    match err {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "invalid-param"),
        other => panic!("expected Server error, got {other:?}"),
    }
}

/// Same dispatcher-level guard for an unrecognized `action`: rejected
/// before `ui_call`, so a typo doesn't reach the UI as an ambiguous
/// no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tab_feed_ime_rejects_unknown_action() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");

    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace,
        supervisor,
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;
    let err = client
        .call_raw(
            ops::TAB_FEED_IME,
            serde_json::json!({
                "tab_id": "1",
                "action": "bogus",
                "text": "",
            }),
        )
        .await
        .expect_err("expected error");
    match err {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "invalid-param"),
        other => panic!("expected Server error, got {other:?}"),
    }
}

/// `app.dock_badge` takes no params, and the empty param struct denies
/// unknown fields. Asserting `unknown-field` (rather than the
/// `internal` / "no UI attached" this handler would give — it has no
/// `ui_tx`) proves the decode happens at the dispatcher, ahead of the
/// UI round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_dock_badge_rejects_unknown_params() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");

    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace,
        supervisor,
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;
    let err = client
        .call_raw(ops::APP_DOCK_BADGE, serde_json::json!({"label": "3"}))
        .await
        .expect_err("expected error");
    match err {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "unknown-field"),
        other => panic!("expected Server error, got {other:?}"),
    }
}

/// `host.add` / `host.list` / `host.remove` over the wire: the full
/// round trip a real `roostctl host` or the Hosts sidebar drives,
/// proving the dispatch arms (not just the `Workspace` accessors
/// `state_persist.rs` exercises directly).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_add_list_remove_round_trip_over_the_wire() {
    use roost_ipc::messages::{HostAddParams, HostAddResult, HostListResult, HostRemoveParams};

    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");

    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace,
        supervisor,
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;

    let added: HostAddResult = client
        .call(
            ops::HOST_ADD,
            HostAddParams {
                label: "pop-os".into(),
                target: "test1@localhost".into(),
            },
        )
        .await
        .expect("host.add");
    assert_eq!(added.host.label, "pop-os");
    assert_eq!(added.host.target, "test1@localhost");
    assert_eq!(added.host.last_connected, None);
    assert!(!added.host.id.is_empty());

    let listed: HostListResult = client
        .call(ops::HOST_LIST, serde_json::json!({}))
        .await
        .expect("host.list");
    assert_eq!(listed.hosts.len(), 1);
    assert_eq!(listed.hosts[0].id, added.host.id);

    client
        .call::<_, serde_json::Value>(
            ops::HOST_REMOVE,
            HostRemoveParams {
                id: added.host.id.clone(),
            },
        )
        .await
        .expect("host.remove");

    let after: HostListResult = client
        .call(ops::HOST_LIST, serde_json::json!({}))
        .await
        .expect("host.list after remove");
    assert!(after.hosts.is_empty());
}

/// Label validation surfaces as `invalid-param` at the wire, and a
/// removal of an id that was never added surfaces as `not-found` —
/// both mapped by `ws_err` in `ipc.rs`, not left as `internal`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_add_rejects_reserved_label_and_remove_reports_not_found() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");

    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace,
        supervisor,
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;

    let err = client
        .call_raw(
            ops::HOST_ADD,
            serde_json::json!({"label": "local", "target": "localhost"}),
        )
        .await
        .expect_err("expected error");
    match err {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "invalid-param"),
        other => panic!("expected Server error, got {other:?}"),
    }

    let err = client
        .call_raw(ops::HOST_REMOVE, serde_json::json!({"id": "never-added"}))
        .await
        .expect_err("expected error");
    match err {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "not-found"),
        other => panic!("expected Server error, got {other:?}"),
    }
}

/// With a UI attached, the registry mutations and both connection ops
/// route to it (plan 037 §3.5).
///
/// The reason is not symmetry: the app owns the connections and the
/// sidebar, so a `roostctl host add` that mutated the workspace behind
/// its back would be invisible until something else forced a reconcile.
/// The headless fallback above stays for embedders with no UI, and
/// `host.connect` has no fallback at all — there is no connection to
/// report without an app.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_ops_route_to_an_attached_ui() {
    use roost_engine::ipc::UiRequest;
    use roost_ipc::messages::{
        host_state, Host, HostAddResult, HostConnectionResult, HostStatus, HostStatusResult,
    };

    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");
    let workspace = Arc::new(Workspace::new());
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = IpcHandler::new(
        workspace.clone(),
        Arc::new(PtySupervisor::new()),
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    )
    .with_ui(ui_tx);

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Stand in for the app's main thread: answer whatever arrives, and
    // record what it was.
    let seen = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let recorder = Arc::clone(&seen);
    tokio::spawn(async move {
        let host = |id: &str| Host {
            id: id.to_string(),
            label: "pop-os".into(),
            target: "/tmp/s.sock".into(),
            last_connected: None,
        };
        while let Some(request) = ui_rx.recv().await {
            match request {
                UiRequest::HostAdd { reply, .. } => {
                    recorder.lock().unwrap().push("add");
                    let _ = reply.send(Ok(host("h1")));
                }
                UiRequest::HostRemove { reply, .. } => {
                    recorder.lock().unwrap().push("remove");
                    let _ = reply.send(Ok(()));
                }
                UiRequest::HostConnect { reply, .. } => {
                    recorder.lock().unwrap().push("connect");
                    let _ = reply.send(Ok(HostConnectionResult {
                        host: host("h1"),
                        state: host_state::CONNECTING.to_string(),
                    }));
                }
                UiRequest::HostDisconnect { id, reply } => {
                    recorder.lock().unwrap().push("disconnect");
                    let _ = reply.send(Err(roost_engine::WorkspaceError::HostNotFound(id)));
                }
                UiRequest::HostStatus { id, reply } => {
                    recorder.lock().unwrap().push("status");
                    let _ = reply.send(Ok(HostStatusResult {
                        hosts: vec![HostStatus {
                            id: id.unwrap_or_else(|| "h1".into()),
                            label: "pop-os".into(),
                            target: "/tmp/s.sock".into(),
                            generation: 2,
                            state: host_state::DISCONNECTED.to_string(),
                            rollup: Some("disconnected".into()),
                            ..HostStatus::default()
                        }],
                    }));
                }
                _ => {}
            }
        }
    });

    let mut client = connect_with_retry(&server_socket).await;

    let added: HostAddResult = client
        .call(
            ops::HOST_ADD,
            serde_json::json!({"label": "pop-os", "target": "/tmp/s.sock"}),
        )
        .await
        .expect("host.add");
    assert_eq!(added.host.id, "h1");
    assert!(
        workspace.hosts().is_empty(),
        "the engine must not also write the registry it delegated"
    );

    let connected: HostConnectionResult = client
        .call(ops::HOST_CONNECT, serde_json::json!({"id": "h1"}))
        .await
        .expect("host.connect");
    assert_eq!(connected.state, host_state::CONNECTING);

    // The UI's own refusal keeps its wire code: a `WorkspaceError`
    // crosses the seam, so `not-found` survives rather than flattening
    // into `internal`.
    let err = client
        .call_raw(ops::HOST_DISCONNECT, serde_json::json!({"id": "h1"}))
        .await
        .expect_err("expected the UI's refusal");
    match err {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "not-found"),
        other => panic!("expected Server error, got {other:?}"),
    }

    // The optional `id` narrows the read; the bare form asks for every
    // saved host.
    let status: HostStatusResult = client
        .call(ops::HOST_STATUS, serde_json::json!({"id": "h1"}))
        .await
        .expect("host.status");
    assert_eq!(status.hosts[0].id, "h1");
    assert_eq!(status.hosts[0].generation, 2);
    let status: HostStatusResult = client
        .call(ops::HOST_STATUS, serde_json::json!({}))
        .await
        .expect("host.status");
    assert_eq!(status.hosts.len(), 1);

    client
        .call::<_, serde_json::Value>(ops::HOST_REMOVE, serde_json::json!({"id": "h1"}))
        .await
        .expect("host.remove");

    assert_eq!(
        *seen.lock().unwrap(),
        vec!["add", "connect", "disconnect", "status", "status", "remove"]
    );
}

/// `host.connect` / `host.disconnect` / `host.status` have no headless
/// implementation: connection state belongs to the app, and inventing
/// one would answer with a state nothing is in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_connection_ops_have_no_headless_answer() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");
    let handler = IpcHandler::new(
        Arc::new(Workspace::new()),
        Arc::new(PtySupervisor::new()),
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );
    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;

    for op in [ops::HOST_CONNECT, ops::HOST_DISCONNECT, ops::HOST_STATUS] {
        let err = client
            .call_raw(op, serde_json::json!({"id": "h1"}))
            .await
            .expect_err("expected error");
        match err {
            roost_ipc::ClientError::Server { code, message } => {
                assert_eq!(code, "internal", "{op}");
                assert_eq!(message, "no UI attached", "{op}");
            }
            other => panic!("expected Server error, got {other:?}"),
        }
    }
}

/// `app.keybind_dispatch` accepts only `"paste"` (plan 039 §3.5's
/// consent-card test seam is not a general keybind dispatcher). A bad
/// action name must fail `invalid-param` — not `internal` — with no UI
/// attached (this handler has no `ui_tx`), proving the check happens at
/// the dispatcher, ahead of `ui_call`, the same way
/// `tab_feed_ime_rejects_unknown_action` proves it for `tab.feed_ime`
/// and the doc comment on `app.dialog_answer` promises for its own
/// `action`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_keybind_dispatch_rejects_non_paste_action() {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");

    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace,
        supervisor,
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;
    let err = client
        .call_raw(
            ops::APP_KEYBIND_DISPATCH,
            serde_json::json!({"action": "close_tab"}),
        )
        .await
        .expect_err("expected error");
    match err {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "invalid-param"),
        other => panic!("expected Server error, got {other:?}"),
    }
}

/// Connect to a freshly-bound server with bounded retries instead of
/// a flat sleep. CI runners under load can take more than 50ms to
/// schedule the accept loop; a bounded retry is robust without
/// slowing the happy path.
async fn connect_with_retry(socket_path: &std::path::Path) -> IpcClient {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut backoff = Duration::from_millis(5);
    let mut last_err: Option<roost_ipc::Error> = None;
    while std::time::Instant::now() < deadline {
        match IpcClient::connect(socket_path).await {
            Ok(c) => return c,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_millis(100));
            }
        }
    }
    panic!(
        "could not connect to {} within 2s: {:?}",
        socket_path.display(),
        last_err
    );
}
