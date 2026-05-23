//! End-to-end IPC smoke. Spins up an `IpcServer` against a temp
//! Unix socket backed by the real `IpcHandler` (in-process
//! `Workspace` + `PtySupervisor`), then dials it with the
//! `IpcClient` and exercises a short scripted scenario.

use std::sync::Arc;
use std::time::Duration;

use roost_ipc::messages::{
    ops, IdentifyParams, IdentifyResult, ProjectCreateParams, ProjectCreateResult, TabListResult,
    TabOpenParams, TabOpenResult,
};
use roost_ipc::IpcClient;
use roost_ipc::IpcServer;
use roost_linux::daemon::{PtySupervisor, Workspace};
use roost_linux::ipc::IpcHandler;
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
