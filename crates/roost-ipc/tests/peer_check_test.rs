//! Peer-credential enforcement, exercised over a real Unix socket.
//!
//! The reject branch is only reachable with an injected uid — a test
//! cannot become another user — so the server takes the expected uid as
//! a parameter and these tests pass `euid + 1`.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::{current_euid, peer_uid, Handler, HandlerError, HandlerOutcome, IpcServer};
use tempfile::tempdir;
use tokio::net::{UnixListener, UnixStream};

struct OkHandler;

impl Handler for OkHandler {
    fn handle<'a>(
        &'a self,
        _op: &'a str,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>> {
        Box::pin(async { Ok(HandlerOutcome::Reply(serde_json::json!({"served": true}))) })
    }
}

/// Both ends of a locally accepted connection report this process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_uid_reports_the_local_process_on_both_ends() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("peer.sock");
    let listener = UnixListener::bind(&socket).expect("bind");

    let client = UnixStream::connect(&socket).await.expect("connect");
    let (accepted, _) = listener.accept().await.expect("accept");

    let me = current_euid();
    assert_eq!(peer_uid(&accepted).expect("peer uid of the client"), me);
    assert_eq!(peer_uid(&client).expect("peer uid of the server"), me);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_matching_peer_uid_is_served() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(&socket, OkHandler)
        .await
        .expect("bind")
        .require_uid(current_euid());
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    let reply = request(&socket, 3).await.expect("a reply frame");
    assert_eq!(reply["ok"], serde_json::json!(true));
    assert_eq!(reply["id"], serde_json::json!("3"));
    assert_eq!(reply["result"]["served"], serde_json::json!(true));
}

/// `require_same_uid()` is the production spelling of the accept path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn require_same_uid_serves_this_process() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(&socket, OkHandler)
        .await
        .expect("bind")
        .require_same_uid();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    let reply = request(&socket, 1).await.expect("a reply frame");
    assert_eq!(reply["ok"], serde_json::json!(true));
}

/// A foreign uid gets dropped at accept — and the loop keeps accepting,
/// so one rejection does not take the server down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_peer_uid_is_rejected_and_the_accept_loop_survives() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(&socket, OkHandler)
        .await
        .expect("bind")
        .require_uid(current_euid().wrapping_add(1));
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    for attempt in 0..2 {
        assert!(
            request(&socket, 7).await.is_none(),
            "attempt {attempt}: server answered a connection it must have dropped"
        );
    }

    // The rejecting server is a dead end by design; a second server
    // under a matching uid still serves normally.
    let other = dir.path().join("other.sock");
    let ok_server = IpcServer::bind(&other, OkHandler)
        .await
        .expect("bind")
        .require_same_uid();
    tokio::spawn(async move {
        let _ = ok_server.run().await;
    });
    let reply = request(&other, 9).await.expect("a reply frame");
    assert_eq!(reply["ok"], serde_json::json!(true));
}

/// Send one request; `None` means the server closed without answering.
async fn request(socket: &std::path::Path, id: i64) -> Option<serde_json::Value> {
    let stream = connect_with_retry(socket).await;
    let (r, mut w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let body = serde_json::to_vec(&serde_json::json!({
        "id": id.to_string(),
        "op": "identify",
        "params": {},
    }))
    .unwrap();
    // A dropped connection can surface as a write error instead of a
    // read EOF, depending on how far the close raced the write.
    if write_frame(&mut w, &body).await.is_err() {
        return None;
    }
    let line = tokio::time::timeout(Duration::from_secs(5), reader.read_line())
        .await
        .expect("server neither answered nor closed within the timeout")
        .ok()??;
    Some(serde_json::from_slice(&line).expect("reply is json"))
}

async fn connect_with_retry(socket: &std::path::Path) -> UnixStream {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut backoff = Duration::from_millis(5);
    loop {
        match UnixStream::connect(socket).await {
            Ok(s) => return s,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_millis(100));
            }
            Err(e) => panic!("connect {}: {e}", socket.display()),
        }
    }
}
