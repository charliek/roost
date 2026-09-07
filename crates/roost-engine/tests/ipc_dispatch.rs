//! End-to-end IPC smoke. Spins up an `IpcServer` against a temp
//! Unix socket backed by the real `IpcHandler` (in-process
//! `Workspace` + `PtySupervisor`), then dials it with the
//! `IpcClient` and exercises a short scripted scenario.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use roost_engine::ipc::{FileStore, IpcHandler, SessionInfo, StopHandle};
use roost_engine::{PtySupervisor, Workspace};
use roost_ipc::messages::{
    ops, IdentifyParams, IdentifyResult, ProjectCreateParams, ProjectCreateResult,
    SessionConnectParams, SessionConnectResult, SessionPutFileParams, SessionPutFileResult,
    TabListResult, TabOpenParams, TabOpenResult, MAX_PUT_FILE_BYTES,
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

/// `clipboard.write` carries `text` **or** `image_png`, never both.
/// Preferring one silently would drop the other, so the ambiguous
/// request is refused outright; and a request carrying neither is a
/// missing `text`, which is the field this arm serves.
///
/// The image form's two dispatcher-level judgements ride along: PRIMARY
/// is refused before any UI sees it (the paste path never probes it for
/// an image), and a well-formed system request takes the `ui_call`
/// route rather than falling through to the text arm's `missing-param`
/// — which headless, with no UI attached, is exactly `internal`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_write_refuses_both_text_and_an_image_and_demands_one() {
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

    for (params, want) in [
        (
            serde_json::json!({"target": "system", "text": "hi", "image_png": "aGVsbG8="}),
            "invalid-param",
        ),
        (serde_json::json!({"target": "system"}), "missing-param"),
        (
            serde_json::json!({"target": "selection", "image_png": "aGVsbG8="}),
            "invalid-param",
        ),
        (
            serde_json::json!({"target": "system", "image_png": "aGVsbG8="}),
            "internal",
        ),
    ] {
        let err = client
            .call_raw(ops::CLIPBOARD_WRITE, params.clone())
            .await
            .expect_err("expected error");
        match err {
            roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, want, "{params}"),
            other => panic!("expected Server error, got {other:?}"),
        }
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

// ============================================================================
// `session.put_file` — plan 047 §3.1 / W1
// ============================================================================

/// A live session socket, with or without a file store behind it.
///
/// Dialed over a real socket rather than driven through the `Handler`
/// trait, because half of what this op promises is about two
/// *connections*: two uploads racing for the same room, and a second
/// connection presenting a lease the first one minted.
struct SessionFixture {
    socket: PathBuf,
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl SessionFixture {
    /// `cap` bounds the store; `None` builds the session without one at
    /// all, which is every socket that is not a host session's.
    async fn new(cap: Option<u64>) -> Self {
        let dir = tempdir().unwrap();
        let socket = dir.path().join("roost.sock");
        let root = dir.path().join("files");
        std::fs::create_dir_all(&root).unwrap();

        let mut handler = IpcHandler::new(
            Arc::new(Workspace::new()),
            Arc::new(PtySupervisor::new()),
            socket.clone(),
            "Roost-test",
            "ai.stridelabs.Roost.test",
        )
        .with_session(
            SessionInfo {
                session_id: "01K3S8TQ4F0Q9YB2K6WZ5D7XN".into(),
                started_at: "2026-09-05T14:03:11Z".into(),
                app_version: "9.9.9".into(),
                payload_kinds: Vec::new(),
                libghostty_build: String::new(),
                default_tab_size: (80, 24),
                test_mode: false,
            },
            StopHandle::new(|| async {}),
        );
        if let Some(cap) = cap {
            handler = handler
                .with_file_store(FileStore::with_cap(root.clone(), cap).expect("open the store"));
        }

        let server = IpcServer::bind(&socket, handler).await.expect("bind");
        let bound = server.socket_path().to_path_buf();
        tokio::spawn(async move {
            let _ = server.run().await;
        });
        Self {
            socket: bound,
            root,
            _dir: dir,
        }
    }

    async fn client(&self) -> IpcClient {
        connect_with_retry(&self.socket).await
    }

    /// A connection holding the session's interactive lease.
    async fn leased(&self) -> (IpcClient, String) {
        let mut client = self.client().await;
        let lease: SessionConnectResult = client
            .call(
                ops::SESSION_CONNECT,
                SessionConnectParams { takeover: true },
            )
            .await
            .expect("session.connect");
        (client, lease.lease)
    }

    /// Every upload directory the store currently holds.
    fn uploads(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<_> = std::fs::read_dir(&self.root)
            .expect("read the store root")
            .map(|entry| entry.expect("entry").path())
            .collect();
        dirs.sort();
        dirs
    }
}

async fn put_file(
    client: &mut IpcClient,
    lease: &str,
    name: &str,
    data: Vec<u8>,
) -> Result<SessionPutFileResult, roost_ipc::ClientError> {
    client
        .call(
            ops::SESSION_PUT_FILE,
            SessionPutFileParams {
                lease: lease.to_string(),
                name: name.to_string(),
                data,
            },
        )
        .await
}

fn code(error: &roost_ipc::ClientError) -> &str {
    match error {
        roost_ipc::ClientError::Server { code, .. } => code,
        other => panic!("expected a server error, got {other:?}"),
    }
}

/// The name is the client's and is never repaired: the path this op
/// returns has to be pasteable bare, so a name that would need quoting
/// — or that could climb out of its upload directory — is refused
/// rather than rewritten into something the client did not ask for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_file_refuses_every_name_it_cannot_paste_bare() {
    let f = SessionFixture::new(Some(1 << 20)).await;
    let (mut client, lease) = f.leased().await;

    let long = "a".repeat(129);
    for name in [
        "",
        long.as_str(),
        ".",
        "..",
        "-rf.png",
        "dir/shot.png",
        "../escape.png",
        "two words.png",
        "caf\u{e9}.png",
        "shot.png; rm -rf /",
        "$HOME.png",
        "shot\npng",
    ] {
        let error = put_file(&mut client, &lease, name, b"x".to_vec())
            .await
            .expect_err("a name outside the grammar must be refused");
        assert_eq!(code(&error), "invalid-param", "{name:?}");
    }
    assert!(
        f.uploads().is_empty(),
        "a refused name must not have claimed a directory"
    );

    // The boundary on the good side: 128 bytes is a name.
    let name = format!("{}.png", "b".repeat(124));
    assert_eq!(name.len(), 128);
    let landed = put_file(&mut client, &lease, &name, b"x".to_vec())
        .await
        .expect("a 128-byte name is inside the rule");
    assert!(landed.path.ends_with(&name));
}

/// The cap is exact on both sides, and the *encoded* screen in front of
/// the decode does not move it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exactly_the_cap_lands_and_one_more_byte_does_not() {
    let f = SessionFixture::new(Some(64 * 1024 * 1024)).await;
    let (mut client, lease) = f.leased().await;

    let cap = usize::try_from(MAX_PUT_FILE_BYTES).unwrap();
    let landed = put_file(&mut client, &lease, "exact.bin", vec![0xAB; cap])
        .await
        .expect("exactly the cap must land");
    assert_eq!(landed.bytes, MAX_PUT_FILE_BYTES);
    assert_eq!(
        std::fs::metadata(&landed.path).expect("stat").len(),
        MAX_PUT_FILE_BYTES
    );

    let error = put_file(&mut client, &lease, "over.bin", vec![0xAB; cap + 1])
        .await
        .expect_err("one byte over the cap must be refused");
    assert_eq!(code(&error), "too-large");
    assert_eq!(f.uploads().len(), 1, "the refusal claimed no directory");
}

/// The screen in front of the decode, proven by the one payload that
/// can tell the two apart: base64 that is both over-long *and*
/// malformed. Decoded first, it would be `invalid-param`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_payload_is_refused_before_it_is_decoded() {
    let f = SessionFixture::new(Some(64 * 1024 * 1024)).await;
    let (mut client, lease) = f.leased().await;

    let encoded_cap = usize::try_from(MAX_PUT_FILE_BYTES).unwrap().div_ceil(3) * 4;
    let error = client
        .call_raw(
            ops::SESSION_PUT_FILE,
            serde_json::json!({
                "lease": lease,
                "name": "over.bin",
                "data": "!".repeat(encoded_cap + 1),
            }),
        )
        .await
        .expect_err("an over-long payload must be refused");
    assert_eq!(code(&error), "too-large");
    assert!(f.uploads().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_base64_is_invalid_param() {
    let f = SessionFixture::new(Some(1 << 20)).await;
    let (mut client, lease) = f.leased().await;

    let error = client
        .call_raw(
            ops::SESSION_PUT_FILE,
            serde_json::json!({"lease": lease, "name": "shot.png", "data": "not base64!!"}),
        )
        .await
        .expect_err("malformed base64 must be refused");
    assert_eq!(code(&error), "invalid-param");
    assert!(f.uploads().is_empty());
}

/// The op writes into the session user's home and its answer is about
/// to be typed into one of the session's tabs, so it is lease-gated
/// like every other op that carries authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_file_needs_a_lease_and_loses_it_to_a_takeover() {
    let f = SessionFixture::new(Some(1 << 20)).await;

    let mut stranger = f.client().await;
    let error = put_file(
        &mut stranger,
        "0".repeat(32).as_str(),
        "shot.png",
        b"x".to_vec(),
    )
    .await
    .expect_err("no lease, no upload");
    assert_eq!(code(&error), "connect-required");

    let (mut first, lease) = f.leased().await;
    put_file(&mut first, &lease, "before.png", b"x".to_vec())
        .await
        .expect("the lease holder may upload");

    // One client, several connections, is the shape a host client
    // actually has — §3.3 gives uploads a connection of their own — so a
    // second connection presenting the same lease is admitted under it.
    let mut sibling = f.client().await;
    put_file(&mut sibling, &lease, "sibling.png", b"x".to_vec())
        .await
        .expect("a second connection under one lease may upload too");

    // A connection that holds the lease token but has not yet presented
    // it: the takeover below closes the *registered* connections, so
    // this is the one that lives to hear the refusal.
    let mut displaced = f.client().await;

    let (_second, new_lease) = f.leased().await;
    assert_ne!(new_lease, lease);
    let error = put_file(&mut displaced, &lease, "after.png", b"x".to_vec())
        .await
        .expect_err("a displaced lease cannot upload");
    assert_eq!(code(&error), "taken-over");
    assert_eq!(
        f.uploads().len(),
        2,
        "and nothing landed after the takeover"
    );
}

/// Two connections under one lease, racing for the last room in the
/// store. Admission is one serialized decision, so exactly one of them
/// can win — and the loser hearing `store-full` rather than a lease
/// error is also what proves the second connection was admitted under
/// the first one's lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_connections_racing_for_the_last_room_cannot_over_admit() {
    const BYTES: usize = 4096;
    let f = SessionFixture::new(Some(2 * BYTES as u64 - 1)).await;
    let (mut first, lease) = f.leased().await;
    let mut second = f.client().await;

    let (a, b) = tokio::join!(
        put_file(&mut first, &lease, "a.bin", vec![0xA; BYTES]),
        put_file(&mut second, &lease, "b.bin", vec![0xB; BYTES]),
    );

    let winners = [&a, &b].into_iter().filter(|r| r.is_ok()).count();
    assert_eq!(winners, 1, "exactly one upload fits: a={a:?} b={b:?}");
    for outcome in [&a, &b] {
        if let Err(error) = outcome {
            assert_eq!(code(error), "store-full");
        }
    }
    assert_eq!(f.uploads().len(), 1, "and only one directory was claimed");
}

/// A full store refuses and **deletes nothing**: a path this op has
/// already handed back may sit unsubmitted in an agent's composer for
/// an hour, so eviction would break the one promise the op makes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_store_refuses_and_deletes_nothing() {
    const BYTES: usize = 4096;
    let f = SessionFixture::new(Some(BYTES as u64)).await;
    let (mut client, lease) = f.leased().await;

    let landed = put_file(&mut client, &lease, "kept.bin", vec![0xA; BYTES])
        .await
        .expect("the first file fills the store exactly");

    for (name, size) in [("second.bin", BYTES), ("tiny.bin", 1)] {
        let error = put_file(&mut client, &lease, name, vec![0xB; size])
            .await
            .expect_err("nothing else fits");
        assert_eq!(code(&error), "store-full", "{name}");
    }

    assert_eq!(
        std::fs::read(&landed.path).expect("the kept file is still there"),
        vec![0xA; BYTES]
    );
    assert_eq!(f.uploads().len(), 1);
}

/// Not every socket is a host session's, and a session built without a
/// store says so rather than writing somewhere nothing will sweep.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_without_a_store_answers_not_supported() {
    let f = SessionFixture::new(None).await;
    let (mut client, lease) = f.leased().await;

    let error = put_file(&mut client, &lease, "shot.png", b"x".to_vec())
        .await
        .expect_err("no store, no upload");
    assert_eq!(code(&error), "not-supported");
}

/// Plan 047 §3.4: the dispatcher decides the ref spelling and nothing
/// else — the paths reach the app untouched, because §3.4 ranks the tab
/// and the host ahead of them and only the app can answer those. The
/// recorder proves both halves: a misspelt ref never becomes a gesture,
/// and the paths the app hears are the ones the caller sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tab_send_file_refuses_a_misspelt_ref_before_the_ui_hears_it() {
    use roost_engine::ipc::UiRequest;
    use roost_ipc::messages::{TabSendFileParams, TabSendFileResult};

    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = IpcHandler::new(
        Arc::new(Workspace::new()),
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

    // Stand in for the app: record the request and answer with the
    // paths it was handed, so the reply carries what crossed the seam.
    let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, Vec<String>)>::new()));
    let recorder = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Some(request) = ui_rx.recv().await {
            if let UiRequest::TabSendFile { tab, paths, reply } = request {
                recorder
                    .lock()
                    .unwrap()
                    .push((tab.to_string(), paths.clone()));
                let _ = reply.send(Ok(TabSendFileResult {
                    pasted: paths.join("\n"),
                    ..TabSendFileResult::default()
                }));
            }
        }
    });

    let mut client = connect_with_retry(&server_socket).await;

    // A relative path rides along untouched: the app is the one that
    // judges it, after the tab and the host.
    let sent: TabSendFileResult = client
        .call(
            ops::TAB_SEND_FILE,
            TabSendFileParams {
                tab: "h2.7".into(),
                paths: vec!["/tmp/b.txt".into(), "tmp/relative.txt".into()],
            },
        )
        .await
        .expect("tab.send_file");
    assert_eq!(
        sent.pasted, "/tmp/b.txt\ntmp/relative.txt",
        "the paths cross the seam as the caller spelled them"
    );

    for (tab, paths, needle) in [
        // Non-canonical spellings are refused rather than normalized,
        // the same rule `tab.focus`'s ref parser states.
        ("h0.7", vec!["/tmp/a.txt"], "h0.7"),
        ("007", vec!["/tmp/a.txt"], "007"),
        ("", vec!["/tmp/a.txt"], "invalid tab reference"),
    ] {
        let error = client
            .call_raw(
                ops::TAB_SEND_FILE,
                TabSendFileParams {
                    tab: tab.into(),
                    paths: paths.iter().map(|p| p.to_string()).collect(),
                },
            )
            .await
            .expect_err("a malformed tab.send_file must be refused");
        assert_eq!(code(&error), "invalid-param", "{tab} {paths:?}");
        let roost_ipc::ClientError::Server { message, .. } = &error else {
            unreachable!()
        };
        assert!(message.contains(needle), "{tab} {paths:?}: {message}");
    }

    assert_eq!(
        *seen.lock().unwrap(),
        vec![(
            "h2.7".to_string(),
            vec!["/tmp/b.txt".to_string(), "tmp/relative.txt".to_string()]
        )],
        "only the well-spelled ref may reach the app"
    );
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
