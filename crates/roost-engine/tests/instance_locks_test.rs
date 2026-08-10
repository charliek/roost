//! The composed startup sequence: take the socket/bind lock, then let
//! `IpcServer::bind` probe → unlink → bind under it.
//!
//! The unit tests in `single_instance.rs` cover the locks and the ones
//! in `roost_ipc::socket_state` cover the errno rule. What only shows up
//! when the two are composed is the property the whole design exists
//! for: a stale socket plus two starting processes yields exactly one
//! server, and the loser never unlinks the winner's socket.

use std::sync::Arc;

use roost_engine::single_instance::{acquire_locks, LocksError};
use roost_engine::{ipc::IpcHandler, PtySupervisor, Workspace};
use roost_ipc::IpcServer;
use tempfile::tempdir;

fn handler(dir: &std::path::Path, socket_path: &std::path::Path) -> IpcHandler {
    IpcHandler::new(
        Arc::new(Workspace::open(dir.join("state.json"))),
        Arc::new(PtySupervisor::new()),
        socket_path.to_path_buf(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    )
}

/// Leave a socket file with no listener behind it — what a SIGKILLed
/// UI leaves on disk. Dropping a `UnixListener` does not unlink it.
fn stale_socket(path: &std::path::Path) {
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    drop(listener);
    assert!(path.exists(), "the stale socket file must survive the drop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exactly_one_of_two_starts_wins_a_stale_socket() {
    let runtime_dir = tempdir().unwrap();
    let state_a = tempdir().unwrap();
    let state_b = tempdir().unwrap();
    let socket_path = runtime_dir.path().join("roost.sock");
    let socket_lock = runtime_dir.path().join("roost.lock");
    stale_socket(&socket_path);

    // Both would-be instances race for the same socket with different
    // state dirs — the developer's and the harness's normal shape.
    let first = acquire_locks(&socket_lock, state_a.path().join("state.lock"));
    let second = acquire_locks(&socket_lock, state_b.path().join("state.lock"));

    let winner = first.expect("the first start must take the socket lock");
    match second {
        Err(LocksError::SocketHeld { .. }) => {}
        other => panic!("the second start must lose the socket lock, got {other:?}"),
    }

    let server = IpcServer::bind(&socket_path, handler(state_a.path(), &socket_path))
        .await
        .expect("the winner reclaims the stale socket");
    drop(server);
    drop(winner);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_socket_is_never_unlinked_by_a_second_bind() {
    let runtime_dir = tempdir().unwrap();
    let state_dir = tempdir().unwrap();
    let socket_path = runtime_dir.path().join("roost.sock");

    let server = IpcServer::bind(&socket_path, handler(state_dir.path(), &socket_path))
        .await
        .expect("first bind");
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    // A second bind is only reachable with the lock stolen or removed —
    // the probe is the backstop for exactly that. It must refuse rather
    // than unlink a socket a live server is serving.
    let error = match IpcServer::bind(&socket_path, handler(state_dir.path(), &socket_path)).await {
        Ok(_) => panic!("binding over a live socket must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("live listener"),
        "unexpected error: {error}"
    );

    // ...and the live socket is still there and still answering.
    tokio::net::UnixStream::connect(&socket_path)
        .await
        .expect("the live socket must survive the refused bind");
}
