//! The first-line sniff and the connection-close seam (plan 036 D4/D6).
//!
//! Two contracts live here. First, exactly one first-line shape diverts
//! a connection to `Handler::handle_data` — a JSON object with `attach`
//! and no `op` — and every other shape behaves precisely as it did
//! before the data plane existed, because every UI socket in the
//! product runs through this same loop. Second, a `ConnCloser` ends its
//! connection from the outside: a push connection with a final labeled
//! envelope, a request connection with a plain close, a data connection
//! whatever its handler does plus a hard abort.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use roost_ipc::dataframe::{write_data_frame, DataFrameReader, FRAME_INPUT, FRAME_PTY, PREAMBLE};
use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::messages::{AttachHandshake, SESSION_STOPPING_EVENT};
use roost_ipc::{
    CloseReason, ConnAction, ConnCloser, ConnCtx, DataConn, Handler, HandlerError, HandlerOutcome,
    IpcServer, PushSource,
};
use tempfile::tempdir;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(5);

/// What the test handler does with a data connection.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum DataMode {
    /// Don't override `handle_data` at all — the socket has no data
    /// plane, like every UI socket.
    #[default]
    Unsupported,
    /// Accept, then echo each `INPUT` frame back as a `PTY` frame.
    Echo,
    /// Accept and then never finish, ignoring the close watch: the
    /// server's own abort is the only thing that can end it.
    Park,
}

/// Records what reached the handler, and serves the data half per
/// [`DataMode`].
#[derive(Default)]
struct Recorder {
    data_mode: DataMode,
    handshakes: Mutex<Vec<AttachHandshake>>,
    ops: Mutex<Vec<String>>,
    conn_ids: Mutex<Vec<u64>>,
    closers: Mutex<Vec<ConnCloser>>,
    /// When set, the first request reply flips the connection into push
    /// mode fed by this source.
    push: Mutex<Option<PushSource>>,
}

impl Handler for Recorder {
    fn handle<'a>(
        &'a self,
        ctx: &'a ConnCtx,
        op: &'a str,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>> {
        self.ops.lock().unwrap().push(op.to_string());
        self.conn_ids.lock().unwrap().push(ctx.conn_id);
        self.closers.lock().unwrap().push(ctx.closer.clone());
        let push = self.push.lock().unwrap().take();
        Box::pin(async move {
            match push {
                Some(source) => Ok(HandlerOutcome::ReplyThen {
                    reply: serde_json::json!({"subscribed": true}),
                    then: ConnAction::StartPush(source),
                }),
                None => Ok(HandlerOutcome::Reply(serde_json::json!({"served": true}))),
            }
        })
    }

    fn handle_data<'a>(
        &'a self,
        ctx: &'a ConnCtx,
        handshake: AttachHandshake,
        conn: DataConn,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        if self.data_mode == DataMode::Unsupported {
            // Exercise the trait's own default — what a socket with no
            // data plane answers.
            static PLAIN: PlainHandler = PlainHandler;
            return Handler::handle_data(&PLAIN, ctx, handshake, conn);
        }
        self.handshakes.lock().unwrap().push(handshake);
        self.conn_ids.lock().unwrap().push(ctx.conn_id);
        self.closers.lock().unwrap().push(ctx.closer.clone());
        let mode = self.data_mode;
        Box::pin(async move {
            let (mut reader, mut writer, mut close) = conn.into_parts();
            let reply = serde_json::to_vec(&serde_json::json!({
                "ok": true, "kind": "ghostty-snapshot", "mode": "snapshot",
                "seq": 7, "server_epoch": 1, "tab_generation": 1,
            }))
            .unwrap();
            write_frame(&mut writer, &reply).await.unwrap();
            if mode == DataMode::Park {
                // No preamble: the test that ends this connection from
                // the outside asserts a clean EOF, and a dangling
                // 8-byte magic would read as a truncated line instead.
                std::future::pending::<()>().await;
            }
            writer.write_all(&PREAMBLE).await.unwrap();
            writer.flush().await.unwrap();
            loop {
                tokio::select! {
                    frame = reader.next_frame() => match frame {
                        Ok(Some(frame)) => {
                            assert_eq!(frame.frame_type, FRAME_INPUT);
                            write_data_frame(&mut writer, FRAME_PTY, &frame.payload)
                                .await
                                .unwrap();
                        }
                        _ => return,
                    },
                    _ = close.closed() => return,
                }
            }
        })
    }
}

/// A handler that overrides nothing — the shape the default
/// `handle_data` is written for.
struct PlainHandler;

impl Handler for PlainHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a ConnCtx,
        _op: &'a str,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>> {
        Box::pin(async { Ok(HandlerOutcome::Reply(serde_json::json!({}))) })
    }
}

/// The server takes ownership of its handler; the tests want a handle
/// on the same one to read back what it recorded.
struct Shared(Arc<Recorder>);

impl Handler for Shared {
    fn handle<'a>(
        &'a self,
        ctx: &'a ConnCtx,
        op: &'a str,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>> {
        self.0.handle(ctx, op, params)
    }

    fn handle_data<'a>(
        &'a self,
        ctx: &'a ConnCtx,
        handshake: AttachHandshake,
        conn: DataConn,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.0.handle_data(ctx, handshake, conn)
    }
}

struct Served {
    handler: Arc<Recorder>,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Served {
    /// The closer for the first connection the handler saw — every test
    /// that closes one from the outside drives a single connection.
    /// Polls because the handler records it on the server's task.
    async fn closer(&self) -> ConnCloser {
        for _ in 0..500 {
            if let Some(c) = self.handler.closers.lock().unwrap().first() {
                return c.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the handler never saw a connection");
    }
}

async fn serve(handler: Recorder) -> Served {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let handler = Arc::new(handler);
    let server = IpcServer::bind(&socket, Shared(handler.clone()))
        .await
        .expect("bind");
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    Served {
        handler,
        socket,
        _dir: dir,
    }
}

async fn dial(socket: &Path) -> UnixStream {
    for _ in 0..200 {
        if let Ok(s) = UnixStream::connect(socket).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server never came up at {}", socket.display());
}

/// Write `bytes` plus a newline verbatim. The malformed-input cases
/// must reach the wire exactly as written, which `write_frame`
/// (rightly) refuses for anything carrying a newline.
async fn write_raw_line(w: &mut OwnedWriteHalf, bytes: &[u8]) {
    w.write_all(bytes).await.expect("write");
    w.write_all(b"\n").await.expect("write newline");
    w.flush().await.expect("flush");
}

/// Send `line` as a connection's first frame and read the one line that
/// comes back, or `None` if the server closed instead.
async fn first_reply(socket: &Path, line: &[u8]) -> Option<serde_json::Value> {
    let (r, mut w) = dial(socket).await.into_split();
    let mut reader = FrameReader::new(r);
    write_raw_line(&mut w, line).await;
    let frame = tokio::time::timeout(TIMEOUT, reader.read_line())
        .await
        .expect("a reply within the deadline")
        .expect("read");
    frame.map(|f| serde_json::from_slice(&f).expect("the reply is JSON"))
}

async fn read_json_line(
    reader: &mut FrameReader<tokio::net::unix::OwnedReadHalf>,
) -> serde_json::Value {
    let line = tokio::time::timeout(TIMEOUT, reader.read_line())
        .await
        .expect("a frame within the deadline")
        .expect("read")
        .expect("a frame, not EOF");
    serde_json::from_slice(&line).expect("the frame is JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_first_line_reaches_handle_data_with_its_residue() {
    let served = serve(Recorder {
        data_mode: DataMode::Echo,
        ..Recorder::default()
    })
    .await;

    let (r, mut w) = dial(&served.socket).await.into_split();
    let mut reader = FrameReader::new(r);

    // The handshake line and the first binary frame in ONE write: the
    // shape that loses the head of the stream if the line reader's
    // residue is dropped on handover.
    let payload = b"echo me";
    let mut wire = serde_json::to_vec(&serde_json::json!({
        "attach": "tok", "protocol_version": 2, "resume_from_seq": 12,
    }))
    .unwrap();
    wire.push(b'\n');
    wire.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    wire.push(FRAME_INPUT);
    wire.extend_from_slice(payload);
    w.write_all(&wire).await.unwrap();
    w.flush().await.unwrap();

    let reply = read_json_line(&mut reader).await;
    assert_eq!(reply["ok"], serde_json::json!(true));
    assert_eq!(reply["seq"], serde_json::json!(7));

    let (read_half, residue) = reader.into_parts();
    let mut frames = DataFrameReader::new(read_half, residue);
    tokio::time::timeout(TIMEOUT, frames.read_preamble())
        .await
        .expect("the preamble within the deadline")
        .expect("a valid preamble");
    let echoed = tokio::time::timeout(TIMEOUT, frames.next_frame())
        .await
        .expect("a frame within the deadline")
        .expect("read")
        .expect("a frame, not EOF");
    assert_eq!(echoed.frame_type, FRAME_PTY);
    assert_eq!(echoed.payload, payload);

    let handshakes = served.handler.handshakes.lock().unwrap();
    assert_eq!(handshakes.len(), 1);
    assert_eq!(handshakes[0].attach, "tok");
    assert_eq!(handshakes[0].protocol_version, 2);
    assert_eq!(handshakes[0].resume_from_seq, Some(12));
    assert!(
        served.handler.ops.lock().unwrap().is_empty(),
        "a data connection must never reach the request dispatcher"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_socket_without_a_data_plane_answers_not_supported() {
    let served = serve(Recorder::default()).await;
    let reply = first_reply(&served.socket, br#"{"attach":"tok","protocol_version":2}"#)
        .await
        .expect("a reply line");
    assert_eq!(reply["ok"], serde_json::json!(false));
    assert_eq!(reply["error"]["code"], serde_json::json!("not-supported"));
}

/// The one decode failure the handler never sees: a first line that
/// sniffs as a handshake but cannot be decoded as one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_undecodable_handshake_gets_a_clean_error_line() {
    let served = serve(Recorder {
        data_mode: DataMode::Echo,
        ..Recorder::default()
    })
    .await;
    let reply = first_reply(&served.socket, br#"{"attach":5,"protocol_version":2}"#)
        .await
        .expect("a reply line");
    assert_eq!(reply["ok"], serde_json::json!(false));
    assert_eq!(reply["error"]["code"], serde_json::json!("parse-error"));
    assert!(
        served.handler.handshakes.lock().unwrap().is_empty(),
        "an undecodable handshake must not reach the handler"
    );
}

/// Everything that is not the handshake shape keeps its pre-data-plane
/// behavior. These are the lines a UI socket actually sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_other_first_line_shape_behaves_exactly_as_before() {
    let served = serve(Recorder {
        data_mode: DataMode::Echo,
        ..Recorder::default()
    })
    .await;

    // An op-carrying envelope stays a request — even one that also
    // carries `attach`, because `op` is what says "this is a request".
    // (`RawRequest` then rejects the unknown field, as it always has.)
    let reply = first_reply(
        &served.socket,
        br#"{"id":"1","op":"identify","params":{},"attach":"tok"}"#,
    )
    .await
    .expect("a reply line");
    assert_eq!(reply["ok"], serde_json::json!(false));
    assert_eq!(reply["error"]["code"], serde_json::json!("parse-error"));
    assert_eq!(reply["id"], serde_json::json!("1"));

    let reply = first_reply(&served.socket, br#"{"id":"2","op":"identify","params":{}}"#)
        .await
        .expect("a reply line");
    assert_eq!(reply["ok"], serde_json::json!(true));
    assert_eq!(reply["result"], serde_json::json!({"served": true}));

    // An object with neither key, a bare array, a truncated object, and
    // bytes that are not JSON at all: all parse-error at id 0.
    for line in [
        br#"{"hello":1}"#.as_slice(),
        br#"[1,2]"#.as_slice(),
        br#"{"attach":"#.as_slice(),
        b"not json at all".as_slice(),
    ] {
        let reply = first_reply(&served.socket, line)
            .await
            .unwrap_or_else(|| panic!("a reply for {}", String::from_utf8_lossy(line)));
        assert_eq!(reply["ok"], serde_json::json!(false), "line {line:?}");
        assert_eq!(reply["error"]["code"], serde_json::json!("parse-error"));
        assert_eq!(reply["id"], serde_json::json!("0"));
    }

    assert!(
        served.handler.handshakes.lock().unwrap().is_empty(),
        "nothing here is a handshake"
    );
}

/// The sniff is first-line-only: a request connection cannot be
/// converted mid-stream by a later line that looks like a handshake.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handshake_after_the_first_line_is_a_parse_error() {
    let served = serve(Recorder {
        data_mode: DataMode::Echo,
        ..Recorder::default()
    })
    .await;
    let (r, mut w) = dial(&served.socket).await.into_split();
    let mut reader = FrameReader::new(r);

    write_frame(&mut w, br#"{"id":"1","op":"identify","params":{}}"#)
        .await
        .unwrap();
    assert_eq!(
        read_json_line(&mut reader).await["ok"],
        serde_json::json!(true)
    );

    write_frame(&mut w, br#"{"attach":"tok","protocol_version":2}"#)
        .await
        .unwrap();
    assert_eq!(
        read_json_line(&mut reader).await["error"]["code"],
        serde_json::json!("parse-error")
    );
    assert!(served.handler.handshakes.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_connection_gets_its_own_id() {
    let served = serve(Recorder::default()).await;
    for _ in 0..3 {
        first_reply(&served.socket, br#"{"id":"1","op":"identify","params":{}}"#)
            .await
            .expect("a reply");
    }
    let ids = served.handler.conn_ids.lock().unwrap().clone();
    assert_eq!(ids.len(), 3);
    let unique: std::collections::HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(unique.len(), 3, "conn ids must be unique: {ids:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_a_push_connection_writes_one_labeled_envelope() {
    let (tx, rx) = mpsc::channel(4);
    let served = serve(Recorder {
        push: Mutex::new(Some(PushSource::new(rx))),
        ..Recorder::default()
    })
    .await;

    let (r, mut w) = dial(&served.socket).await.into_split();
    let mut reader = FrameReader::new(r);
    write_frame(&mut w, br#"{"id":"1","op":"events.subscribe","params":{}}"#)
        .await
        .unwrap();
    assert_eq!(
        read_json_line(&mut reader).await["result"],
        serde_json::json!({"subscribed": true})
    );

    tx.send(serde_json::json!({"revision": 1, "events": []}))
        .await
        .unwrap();
    assert_eq!(
        read_json_line(&mut reader).await["revision"],
        serde_json::json!(1)
    );

    let closer = served.closer().await;
    assert!(closer.close(CloseReason::TakenOver));
    // One-shot: the first reason wins and a later one is a no-op.
    assert!(!closer.close(CloseReason::ShuttingDown));
    assert_eq!(closer.reason(), Some(CloseReason::TakenOver));

    let stopping = read_json_line(&mut reader).await;
    assert_eq!(
        stopping["event"],
        serde_json::json!(SESSION_STOPPING_EVENT),
        "the last frame names why the stream ended"
    );
    assert_eq!(stopping["data"]["reason"], serde_json::json!("taken-over"));
    assert!(
        stopping.get("revision").is_none(),
        "the control envelope carries no revision — it is exempt from the gap check"
    );

    let eof = tokio::time::timeout(TIMEOUT, reader.read_line())
        .await
        .expect("eof within the deadline")
        .unwrap();
    assert!(eof.is_none(), "nothing follows the stopping envelope");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_close_is_labeled_stop() {
    let (_tx, rx) = mpsc::channel(4);
    let served = serve(Recorder {
        push: Mutex::new(Some(PushSource::new(rx))),
        ..Recorder::default()
    })
    .await;
    let (r, mut w) = dial(&served.socket).await.into_split();
    let mut reader = FrameReader::new(r);
    write_frame(&mut w, br#"{"id":"1","op":"events.subscribe","params":{}}"#)
        .await
        .unwrap();
    read_json_line(&mut reader).await;

    served.closer().await.close(CloseReason::ShuttingDown);
    assert_eq!(
        read_json_line(&mut reader).await["data"]["reason"],
        serde_json::json!("stop")
    );
}

/// A request connection has nothing pending on it to label, so the
/// closer just ends it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_a_request_connection_just_closes_it() {
    let served = serve(Recorder::default()).await;
    let (r, mut w) = dial(&served.socket).await.into_split();
    let mut reader = FrameReader::new(r);
    write_frame(&mut w, br#"{"id":"1","op":"identify","params":{}}"#)
        .await
        .unwrap();
    read_json_line(&mut reader).await;

    served.closer().await.close(CloseReason::ShuttingDown);
    let eof = tokio::time::timeout(TIMEOUT, reader.read_line())
        .await
        .expect("eof within the deadline")
        .unwrap();
    assert!(eof.is_none());
}

/// The abort guarantee: a data connection whose handler never finishes
/// and never looks at its close watch still ends when the closer fires.
/// Labeling the close is the handler's job (the engine's, at HS-1b C5);
/// ending it is this loop's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_a_data_connection_ends_it_even_when_the_handler_ignores_the_watch() {
    let served = serve(Recorder {
        data_mode: DataMode::Park,
        ..Recorder::default()
    })
    .await;
    let (r, mut w) = dial(&served.socket).await.into_split();
    let mut reader = FrameReader::new(r);
    write_frame(&mut w, br#"{"attach":"tok","protocol_version":2}"#)
        .await
        .unwrap();
    assert_eq!(
        read_json_line(&mut reader).await["ok"],
        serde_json::json!(true)
    );

    served.closer().await.close(CloseReason::Superseded);
    let eof = tokio::time::timeout(TIMEOUT, reader.read_line())
        .await
        .expect("the server ends the connection within the close deadline")
        .unwrap();
    assert!(eof.is_none());
}
