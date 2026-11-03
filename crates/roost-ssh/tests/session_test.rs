//! End to end: the dialer's exec channels piped into a real session
//! socket, with `session.identify` answered by a real `roost-session`
//! serving in-process — the far side `client-bridge` would reach.

mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use roost_client::dial::Dialer;
use roost_engine::single_instance;
use roost_ipc::messages::SESSION_PROTOCOL_VERSION;
use roost_ipc::session_launch::FirstProject;
use roost_session::{Readiness, SessionConfig};
use support::{identify, Behavior, Server};

fn config(root: &Path) -> SessionConfig {
    SessionConfig {
        socket_path: root.join("roost.sock"),
        state_path: root.join("state.json"),
        app_label: "RoostSshTest".into(),
        app_id: "ai.stridelabs.Roost.ssh.test".into(),
        files_dir: Some(root.join("files")),
        files_fallback: root.join("files-fallback"),
        test_mode: true,
        fake_libghostty_build: None,
        legacy_payload_kinds: false,
        // Nothing here needs a tab, so no shell is spawned.
        first_project: FirstProject::Withheld,
        starter: roost_engine::TabOpener::None,
        replay_window: None,
        ownership: Default::default(),
        exe_path: None,
        identity_override: None,
    }
}

#[tokio::test]
async fn identify_reaches_a_real_session_over_ssh() {
    // Under /tmp, not $TMPDIR, so the socket path fits `sun_path`.
    let root = tempfile::Builder::new()
        .prefix("rs-")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let config = config(root.path());
    let socket = config.socket_path.clone();
    let locks = single_instance::acquire_locks(
        root.path().join("roost.lock"),
        root.path().join("state.lock"),
    )
    .expect("instance locks");
    let session = tokio::spawn(async move {
        let mut readiness = Readiness::Discard;
        roost_session::serve(config, locks, &mut readiness).await
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        assert!(Instant::now() < deadline, "the session never listened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let server = Server::start(Behavior::Pipe(socket)).await;
    let dialer = server.pinned_dialer();
    let (control, events) = tokio::join!(dialer.dial(), dialer.dial());
    let (first, second) = tokio::join!(
        identify(control.expect("dial control")),
        identify(events.expect("dial events"))
    );
    let first = first.expect("identify over the first channel");
    let second = second.expect("identify over the second channel");
    assert_eq!(first.session_protocol, SESSION_PROTOCOL_VERSION);
    assert!(!first.libghostty_build.is_empty());
    assert_eq!(
        first.session_id, second.session_id,
        "one session behind both"
    );
    assert_eq!(server.connections(), 1);

    session.abort();
}
