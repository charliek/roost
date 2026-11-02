//! Alone in its own binary on purpose (#583): a sibling test that forks
//! while the listener is open hands the child a copy of its descriptor
//! (on macOS until the child exits, since std sets `FD_CLOEXEC` in a
//! second call after `socket()`), and that copy keeps a dropped listener
//! accepting, so the corpse probes `Live`.

use roost_ipc::socket_state::{probe, SocketState, PROBE_TIMEOUT};

#[tokio::test]
async fn a_bound_listener_is_live_and_its_corpse_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roost.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    assert_eq!(probe(&path, PROBE_TIMEOUT).await, SocketState::Live);
    // Dropping a listener does NOT unlink its path — which is
    // exactly how a stale socket comes to exist after a SIGKILL.
    drop(listener);
    assert_eq!(probe(&path, PROBE_TIMEOUT).await, SocketState::Stale);
}
