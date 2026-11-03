//! A UI under `local-backend = session` that cannot resolve its session's
//! socket serves no event stream at all: its `identify` names no
//! `local_session_socket` and its `ops` lack `events.subscribe`, and its
//! own subscribe is `not-implemented`. That is the shape `roostctl wait`
//! and `tab prompt` refuse `unsupported` rather than poll — a degraded
//! state, never advertised as a stream.
//!
//! One test in its own binary, because it sets `ROOST_SESSION_SOCKET_DIR`
//! for the whole process: a relative value is refused by every session
//! path resolution, which is how the socket comes to be unresolvable.

use std::sync::Arc;
use std::time::Duration;

use roost_engine::ipc::IpcHandler;
use roost_engine::{PtySupervisor, Workspace};
use roost_ipc::messages::{ops, IdentifyParams, IdentifyResult};
use roost_ipc::{IpcClient, IpcServer, LocalBackendCell, LocalBackendMode, LocalRoute};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_backed_ui_with_no_session_socket_serves_no_stream() {
    std::env::set_var(roost_ipc::paths::SESSION_SOCKET_DIR_ENV, "not/absolute");
    assert_eq!(roost_ipc::session_socket_path(), None, "the precondition");

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");
    let handler = IpcHandler::new(
        Arc::new(Workspace::new()),
        Arc::new(PtySupervisor::new()),
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    )
    .with_local_route(Arc::new(LocalBackendCell::new(LocalRoute {
        mode: LocalBackendMode::Session,
        ..LocalRoute::default()
    })));
    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    let mut client = connect_with_retry(&server_socket).await;

    let identify: IdentifyResult = client
        .call(
            ops::IDENTIFY,
            IdentifyParams {
                client_name: "test".into(),
                client_version: "0".into(),
            },
        )
        .await
        .expect("identify");
    assert_eq!(identify.local_backend, LocalBackendMode::Session);
    assert_eq!(identify.local_session_socket, None);
    let served = identify.ops.expect("identify lists its ops");
    assert!(
        !served.iter().any(|op| op == ops::EVENTS_SUBSCRIBE),
        "{served:?}"
    );

    let refused = client
        .call_raw(ops::EVENTS_SUBSCRIBE, serde_json::json!({}))
        .await
        .expect_err("no stream to subscribe to");
    match refused {
        roost_ipc::ClientError::Server { code, .. } => assert_eq!(code, "not-implemented"),
        other => panic!("expected a server refusal, got {other:?}"),
    }
}

async fn connect_with_retry(socket_path: &std::path::Path) -> IpcClient {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match IpcClient::connect(socket_path).await {
            Ok(client) => return client,
            Err(error) if std::time::Instant::now() >= deadline => {
                panic!("could not connect to {}: {error:?}", socket_path.display())
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}
