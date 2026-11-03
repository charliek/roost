//! End to end: the dialer's exec channels piped into a real session
//! socket, with `session.identify` answered by a real `roost-session`
//! serving in-process — the far side `client-bridge` would reach. Once
//! through the test fixture's server, and once through the development
//! host's (`devserver`, what `examples/dev_sshd.rs` runs).

mod support;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use roost_client::dial::{DialError, Dialer};
use roost_engine::single_instance;
use roost_ipc::messages::SESSION_PROTOCOL_VERSION;
use roost_ipc::session_launch::FirstProject;
use roost_ipc::ssh::SshFailure;
use roost_session::{Readiness, SessionConfig};
use roost_ssh::{devserver, HostKeyPolicy, HostTarget, TailscaleSshDialer};
use support::{identify, Behavior, Server};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

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

/// A real session serving in-process under a fresh short root (under
/// `/tmp`, not `$TMPDIR`, so the socket path fits `sun_path`), once it
/// listens.
async fn start_session() -> (tempfile::TempDir, PathBuf, JoinHandle<()>) {
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
        let _ = roost_session::serve(config, locks, &mut readiness).await;
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        assert!(Instant::now() < deadline, "the session never listened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (root, socket, session)
}

/// `devserver::serve` on a loopback port, piping to `socket`, and a
/// dialer pinned to its key — the key `load_or_create_key` made in `dir`.
async fn start_dev_server(dir: &Path, socket: PathBuf) -> (TailscaleSshDialer, JoinHandle<()>) {
    let key = devserver::load_or_create_key(&dir.join("host_ed25519")).expect("host key");
    let pinned = devserver::fingerprint(&key);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let server = tokio::spawn(async move {
        let _ = devserver::serve(listener, key, socket).await;
    });
    let target = HostTarget::parse(&format!("dev@127.0.0.1:{port}"), "").unwrap();
    let dialer = TailscaleSshDialer::new(
        target,
        HostKeyPolicy {
            pinned: Some(pinned),
        },
    );
    (dialer, server)
}

#[tokio::test]
async fn identify_reaches_a_real_session_over_ssh() {
    let (_root, socket, session) = start_session().await;

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

#[tokio::test]
async fn the_dev_server_reaches_a_real_session() {
    let (root, socket, session) = start_session().await;
    let (dialer, server) = start_dev_server(root.path(), socket).await;

    let (control, events) = tokio::join!(dialer.dial(), dialer.dial());
    let (first, second) = tokio::join!(
        identify(control.expect("dial control")),
        identify(events.expect("dial events"))
    );
    let first = first.expect("identify over the first channel");
    let second = second.expect("identify over the second channel");
    assert_eq!(first.session_protocol, SESSION_PROTOCOL_VERSION);
    assert_eq!(
        first.session_id, second.session_id,
        "one session behind both"
    );

    server.abort();
    session.abort();
}

#[tokio::test]
async fn the_dev_server_with_no_session_is_classified() {
    let root = tempfile::Builder::new()
        .prefix("rs-")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let (dialer, server) = start_dev_server(root.path(), root.path().join("roost.sock")).await;

    let mut duplex = dialer.dial().await.expect("the dial itself succeeds");
    let mut buf = Vec::new();
    let error: io::Error = duplex
        .read
        .read_to_end(&mut buf)
        .await
        .expect_err("the read ends on the classified failure, not a bare EOF");
    let inner = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<DialError>());
    assert!(
        matches!(
            inner,
            Some(DialError::Remote {
                failure: Some(SshFailure::NoSession),
                ..
            })
        ),
        "{error:?}"
    );

    server.abort();
}
