//! Server-level framing robustness (#80): a malformed request
//! envelope still gets an error reply that carries the request id
//! (so the client's read loop unblocks at the right id), and a bind
//! failure is surfaced rather than swallowed.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::{Handler, HandlerError, IpcServer};
use tempfile::tempdir;
use tokio::net::UnixStream;

/// Trivial handler — every op succeeds with `{}`. The malformed-
/// envelope path never reaches it (the typed decode fails first).
struct OkHandler;

impl Handler for OkHandler {
    fn handle<'a>(
        &'a self,
        _op: &'a str,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, HandlerError>> + Send + 'a>> {
        Box::pin(async { Ok(serde_json::json!({})) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_envelope_reply_preserves_id() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(&socket, OkHandler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Connect raw so we can hand-craft a frame the typed `IpcClient`
    // could never send: a valid (string-encoded) id alongside an
    // unknown field, which trips `RawRequest`'s deny_unknown_fields.
    let stream = connect_with_retry(&server_socket).await;
    let (r, mut w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let bad = serde_json::to_vec(&serde_json::json!({
        "id": "7",
        "op": "identify",
        "params": {},
        "junk": 1
    }))
    .unwrap();
    write_frame(&mut w, &bad).await.expect("write frame");

    let line = reader
        .read_line()
        .await
        .expect("read")
        .expect("reply frame");
    let v: serde_json::Value = serde_json::from_slice(&line).unwrap();
    assert_eq!(
        v["ok"],
        serde_json::json!(false),
        "expected a failure envelope"
    );
    assert_eq!(v["error"]["code"], "parse-error");
    // The whole point of #80: the id must round-trip as "7", not "0".
    // (ids are string-encoded on the wire via string_int64.)
    assert_eq!(
        v["id"],
        serde_json::json!("7"),
        "parse-error reply lost the request id"
    );
}

#[tokio::test]
async fn bind_failure_is_surfaced() {
    // A regular file where the socket's parent directory should be →
    // `create_dir_all` fails → `bind` returns Err instead of leaving
    // the UI half-wired with no socket.
    let dir = tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let socket = blocker.join("roost.sock");
    let res = IpcServer::bind(&socket, OkHandler).await;
    assert!(
        res.is_err(),
        "bind under a non-directory parent must fail, not silently succeed"
    );
}

async fn connect_with_retry(socket: &std::path::Path) -> UnixStream {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
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
