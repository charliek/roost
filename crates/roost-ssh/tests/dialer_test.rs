//! `TailscaleSshDialer` against an in-process SSH server: trust on first
//! use, one shared connection, the login, the far side's failures, and
//! a dropped connection. No network beyond loopback.

mod support;

use std::io;

use roost_client::dial::{DialError, Dialer, Duplex};
use roost_ipc::ssh::SshFailure;
use roost_ssh::HostKeyPolicy;
use support::{dial_error, Behavior, Server, NO_SESSION_STDERR};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn echo(duplex: &mut Duplex, bytes: &[u8]) {
    duplex.write.write_all(bytes).await.expect("write");
    let mut back = vec![0; bytes.len()];
    duplex
        .read
        .read_exact(&mut back)
        .await
        .expect("read the echo");
    assert_eq!(back, bytes);
}

#[tokio::test]
async fn an_unpinned_host_fails_with_its_fingerprint() {
    let server = Server::start(Behavior::Echo).await;
    let dialer = server.dialer(None);
    match dial_error(dialer.dial().await) {
        DialError::UntrustedHostKey { host, fingerprint } => {
            assert_eq!(host, format!("127.0.0.1:{}", server.addr.port()));
            assert_eq!(fingerprint, server.fingerprint);
            assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
            assert!(
                !fingerprint.ends_with('='),
                "unpadded, as OpenSSH prints it"
            );
        }
        other => panic!("expected an untrusted host key, got {other:?}"),
    }
    assert!(
        server.commands().is_empty(),
        "nothing ran on an untrusted host"
    );
}

#[tokio::test]
async fn a_pinned_host_runs_the_bridge_ladder() {
    let server = Server::start(Behavior::Echo).await;
    let dialer = server.pinned_dialer();
    let mut duplex = dialer.dial().await.expect("dial");
    echo(&mut duplex, b"{\"id\":\"1\"}\n").await;
    assert_eq!(server.commands(), vec![roost_ipc::ssh::remote_command()]);
    assert_eq!(
        dialer.describe(),
        format!("tester@127.0.0.1:{}", server.addr.port())
    );
}

#[tokio::test]
async fn a_changed_key_fails_hard() {
    let server = Server::start(Behavior::Echo).await;
    let pinned = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let error = dial_error(server.dialer(Some(pinned)).dial().await);
    assert!(!error.retryable());
    match error {
        DialError::HostKeyChanged { expected, got, .. } => {
            assert_eq!(expected, pinned);
            assert_eq!(got, server.fingerprint);
        }
        other => panic!("expected a changed host key, got {other:?}"),
    }
    assert!(server.commands().is_empty());
}

#[tokio::test]
async fn pinning_the_shown_fingerprint_lets_the_same_dialer_in() {
    let server = Server::start(Behavior::Echo).await;
    let dialer = server.dialer(None);
    let DialError::UntrustedHostKey { fingerprint, .. } = dial_error(dialer.dial().await) else {
        panic!("expected an untrusted host key");
    };
    dialer.set_host_key_policy(HostKeyPolicy {
        pinned: Some(fingerprint),
    });
    let mut duplex = dialer.dial().await.expect("dial once pinned");
    echo(&mut duplex, b"trusted").await;
}

#[tokio::test]
async fn concurrent_dials_share_one_connection() {
    let server = Server::start(Behavior::Echo).await;
    let dialer = server.pinned_dialer();
    let (first, second) = tokio::join!(dialer.dial(), dialer.dial());
    let (mut first, mut second) = (first.expect("first"), second.expect("second"));
    // Interleaved, so each echo can only come back on its own channel.
    first.write.write_all(b"first").await.unwrap();
    second.write.write_all(b"second").await.unwrap();
    let mut buf = [0; 6];
    second.read.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"second");
    first.read.read_exact(&mut buf[..5]).await.unwrap();
    assert_eq!(&buf[..5], b"first");

    let mut third = dialer.dial().await.expect("third");
    echo(&mut third, b"third").await;
    assert_eq!(server.connections(), 1);
    assert_eq!(server.commands().len(), 3);

    // Half-closing one channel ends only that one.
    first.write.shutdown().await.unwrap();
    let mut rest = Vec::new();
    first
        .read
        .read_to_end(&mut rest)
        .await
        .expect("a clean EOF");
    assert!(rest.is_empty());
    echo(&mut second, b"still here").await;
}

#[tokio::test]
async fn a_refused_login_is_an_auth_failure() {
    let server = Server::start_with(Behavior::Echo, false).await;
    let error = dial_error(server.pinned_dialer().dial().await);
    assert!(!error.retryable());
    match error {
        DialError::Remote { failure, message } => {
            assert_eq!(failure, Some(SshFailure::Auth));
            assert!(
                message.contains("did not accept a Tailscale SSH login"),
                "{message}"
            );
        }
        other => panic!("expected a refused login, got {other:?}"),
    }
}

#[tokio::test]
async fn a_bridge_with_no_session_is_classified() {
    let server = Server::start(Behavior::Fail {
        stderr: NO_SESSION_STDERR.into(),
        code: 1,
    })
    .await;
    let dialer = server.pinned_dialer();
    let mut duplex = dialer.dial().await.expect("the dial itself succeeds");
    let mut buf = Vec::new();
    let error = duplex
        .read
        .read_to_end(&mut buf)
        .await
        .expect_err("the read ends on the classified failure, not a bare EOF");
    let inner = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<DialError>())
        .expect("the io error carries the DialError");
    assert!(
        matches!(
            inner,
            DialError::Remote {
                failure: Some(SshFailure::NoSession),
                ..
            }
        ),
        "{inner:?}"
    );
    assert!(
        error.to_string().contains("has no roost session running"),
        "{error}"
    );
    match dialer.last_remote_error() {
        Some(DialError::Remote {
            failure: Some(SshFailure::NoSession),
            message,
        }) => assert_eq!(message, error.to_string()),
        other => panic!("expected the last remote error to be NoSession, got {other:?}"),
    }
}

#[tokio::test]
async fn a_working_channel_clears_the_last_failure() {
    let server = Server::start(Behavior::Fail {
        stderr: NO_SESSION_STDERR.into(),
        code: 1,
    })
    .await;
    let dialer = server.pinned_dialer();
    let mut failed = dialer.dial().await.expect("dial");
    failed
        .read
        .read_to_end(&mut Vec::new())
        .await
        .expect_err("no session");
    assert!(dialer.last_remote_error().is_some());

    server.set_behavior(Behavior::Echo);
    let mut duplex = dialer.dial().await.expect("dial");
    echo(&mut duplex, b"x").await;
    assert!(dialer.last_remote_error().is_none());
    duplex.write.shutdown().await.unwrap();
    let mut rest = Vec::new();
    duplex
        .read
        .read_to_end(&mut rest)
        .await
        .expect("an exit 0 is a clean EOF");
    assert!(rest.is_empty());
}

#[tokio::test]
async fn a_dropped_connection_is_reestablished() {
    let server = Server::start(Behavior::Echo).await;
    let dialer = server.pinned_dialer();
    let mut old = dialer.dial().await.expect("dial");
    echo(&mut old, b"before").await;

    server.drop_connections();
    let mut buf = [0; 1];
    let error = old
        .read
        .read(&mut buf)
        .await
        .expect_err("a lost connection is not a clean EOF");
    assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted, "{error}");

    let mut new = dialer.dial().await.expect("dial after the drop");
    echo(&mut new, b"after").await;
    assert_eq!(server.connections(), 2);

    // Dialing straight after the cut, before russh has seen it close.
    server.drop_connections();
    let mut newer = dialer.dial().await.expect("dial straight after the drop");
    echo(&mut newer, b"again").await;
    assert_eq!(server.connections(), 3);
}

#[tokio::test]
async fn an_unreachable_host_is_an_io_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let dialer = roost_ssh::TailscaleSshDialer::new(
        roost_ssh::HostTarget::parse(&format!("127.0.0.1:{port}"), "tester").unwrap(),
        HostKeyPolicy::default(),
    );
    match dial_error(dialer.dial().await) {
        DialError::Io(error) => assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused),
        other => panic!("expected an io error, got {other:?}"),
    }
}
