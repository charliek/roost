//! The `HandlerOutcome` seam: `Reply` stays wire-identical to the
//! request/response-only server, `ReplyThen` puts the reply on the wire
//! *before* it acts, and `run_until` stops accepting and cancels live
//! connections.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::{
    ConnAction, ConnCtx, Handler, HandlerError, HandlerOutcome, IpcServer, PushSource,
    StopFinalizer,
};
use tempfile::tempdir;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

const TIMEOUT: Duration = Duration::from_secs(5);

/// What the test handler should do with the next request, installed
/// before the client dials so the handler itself stays trivial.
enum Script {
    Reply,
    /// Reply, then block the finalizer on `gate` and report through
    /// `ran` once it is released.
    StopThen {
        gate: Mutex<Option<oneshot::Receiver<()>>>,
        ran: Arc<AtomicBool>,
        ran_tx: Mutex<Option<oneshot::Sender<()>>>,
    },
    /// Wait for `peer_gone` before returning, so the server's reply
    /// write is guaranteed to hit a closed socket. Reports through
    /// `ran_tx` when the finalizer runs.
    StopAfterPeerGone {
        peer_gone: Mutex<Option<oneshot::Receiver<()>>>,
        ran_tx: Mutex<Option<oneshot::Sender<()>>>,
    },
    /// Reply, then push everything `source` produces.
    Push(Mutex<Option<PushSource>>),
}

struct ScriptedHandler(Script);

impl Handler for ScriptedHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a ConnCtx,
        _op: &'a str,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HandlerOutcome, HandlerError>> + Send + 'a>> {
        Box::pin(async move {
            match &self.0 {
                Script::Reply => Ok(HandlerOutcome::Reply(serde_json::json!({"served": true}))),
                Script::StopThen { gate, ran, ran_tx } => {
                    let gate = gate.lock().unwrap().take().expect("gate taken once");
                    let ran = ran.clone();
                    let ran_tx = ran_tx.lock().unwrap().take().expect("signal taken once");
                    Ok(HandlerOutcome::ReplyThen {
                        reply: serde_json::json!({"stopped": true}),
                        then: ConnAction::FinalizeStop(StopFinalizer::new(move || async move {
                            // Blocking here is the ordering assertion:
                            // if the server ran the finalizer before
                            // flushing the reply, the client below never
                            // gets its frame and the test times out.
                            let _ = gate.await;
                            ran.store(true, Ordering::SeqCst);
                            let _ = ran_tx.send(());
                        })),
                    })
                }
                Script::StopAfterPeerGone { peer_gone, ran_tx } => {
                    let peer_gone = peer_gone.lock().unwrap().take().expect("gate taken once");
                    let _ = peer_gone.await;
                    let ran_tx = ran_tx.lock().unwrap().take().expect("signal taken once");
                    Ok(HandlerOutcome::ReplyThen {
                        reply: serde_json::json!({"stopped": true}),
                        then: ConnAction::FinalizeStop(StopFinalizer::new(move || async move {
                            let _ = ran_tx.send(());
                        })),
                    })
                }
                Script::Push(source) => {
                    let source = source.lock().unwrap().take().expect("source taken once");
                    Ok(HandlerOutcome::ReplyThen {
                        reply: serde_json::json!({"subscribed": true}),
                        then: ConnAction::StartPush(source),
                    })
                }
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_reply_is_wire_identical() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(&socket, ScriptedHandler(Script::Reply))
        .await
        .expect("bind");
    let path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    let (mut reader, mut w) = dial(&path).await;
    request(&mut w, 7, "identify").await;
    let reply = read_frame(&mut reader).await;
    assert_eq!(
        reply,
        serde_json::json!({"id": "7", "ok": true, "result": {"served": true}})
    );

    // The read loop keeps serving after a `Reply`.
    request(&mut w, 8, "identify").await;
    let second = read_frame(&mut reader).await;
    assert_eq!(second["id"], serde_json::json!("8"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reply_then_writes_the_reply_before_it_acts() {
    let (gate_tx, gate_rx) = oneshot::channel();
    let (ran_tx, ran_rx) = oneshot::channel();
    let ran = Arc::new(AtomicBool::new(false));

    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(
        &socket,
        ScriptedHandler(Script::StopThen {
            gate: Mutex::new(Some(gate_rx)),
            ran: ran.clone(),
            ran_tx: Mutex::new(Some(ran_tx)),
        }),
    )
    .await
    .expect("bind");
    let path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    let (mut reader, mut w) = dial(&path).await;
    request(&mut w, 1, "session.stop").await;
    let reply = tokio::time::timeout(TIMEOUT, read_frame(&mut reader))
        .await
        .expect("reply must arrive before the finalizer completes");
    assert_eq!(reply["ok"], serde_json::json!(true));
    assert_eq!(reply["result"]["stopped"], serde_json::json!(true));
    assert!(
        !ran.load(Ordering::SeqCst),
        "finalizer ran before the reply was read"
    );

    gate_tx.send(()).unwrap();
    tokio::time::timeout(TIMEOUT, ran_rx)
        .await
        .expect("finalizer must run")
        .unwrap();
    assert!(ran.load(Ordering::SeqCst));

    // The connection closes once the finalizer is done.
    let tail = tokio::time::timeout(TIMEOUT, reader.read_line())
        .await
        .expect("eof")
        .expect("read");
    assert!(tail.is_none(), "connection should close after FinalizeStop");
}

/// A `session.stop` whose caller hangs up before the reply lands — a
/// Ctrl-C'd `roostctl session stop` during the reap window. The reply
/// write fails EPIPE, but the stop it describes has already happened, so
/// dropping the finalizer here would leave a latched daemon with its
/// socket still on disk and no way to ask it to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalize_stop_runs_even_when_the_client_hangs_up_first() {
    let (gone_tx, gone_rx) = oneshot::channel();
    let (ran_tx, ran_rx) = oneshot::channel();

    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(
        &socket,
        ScriptedHandler(Script::StopAfterPeerGone {
            peer_gone: Mutex::new(Some(gone_rx)),
            ran_tx: Mutex::new(Some(ran_tx)),
        }),
    )
    .await
    .expect("bind");
    let path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    let (reader, mut w) = dial(&path).await;
    request(&mut w, 1, "session.stop").await;
    // Close both halves, then release the handler: the server is now
    // guaranteed to be writing into a socket whose peer is gone.
    drop(reader);
    drop(w);
    gone_tx.send(()).unwrap();

    tokio::time::timeout(TIMEOUT, ran_rx)
        .await
        .expect("finalizer must run even though the reply could not be delivered")
        .expect("finalizer signal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_push_streams_after_the_reply_and_tears_down_on_peer_eof() {
    let (tx, rx) = mpsc::channel(8);
    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(
        &socket,
        ScriptedHandler(Script::Push(Mutex::new(Some(PushSource::new(rx))))),
    )
    .await
    .expect("bind");
    let path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    let (mut reader, mut w) = dial(&path).await;
    request(&mut w, 3, "events.subscribe").await;
    let reply = read_frame(&mut reader).await;
    assert_eq!(reply["result"]["subscribed"], serde_json::json!(true));

    tx.send(serde_json::json!({"revision": 1})).await.unwrap();
    tx.send(serde_json::json!({"revision": 2})).await.unwrap();
    assert_eq!(read_frame(&mut reader).await["revision"], 1);
    assert_eq!(read_frame(&mut reader).await["revision"], 2);

    // A request written in push mode is read and discarded — the point
    // of keeping the read half alive is EOF detection, not dispatch.
    request(&mut w, 4, "identify").await;
    tx.send(serde_json::json!({"revision": 3})).await.unwrap();
    assert_eq!(read_frame(&mut reader).await["revision"], 3);

    // Peer EOF cancels the writer: the source's receiver is dropped, so
    // sending eventually fails.
    drop(reader);
    drop(w);
    let closed = tokio::time::timeout(TIMEOUT, async {
        loop {
            if tx.send(serde_json::json!({"revision": 99})).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "push loop should end when the peer goes away"
    );
}

/// A peer that stops reading blocks the push write as soon as the
/// socket buffer fills. Without a bound on that write the connection
/// task parks there for as long as the peer feels like it, holding the
/// socket and every queued frame behind it; with one, the stall ends the
/// connection like any other write failure.
///
/// Deterministic by size, not by timing: a megabyte cannot fit in an
/// unread socket buffer on any platform this ships on, so the very first
/// frame is the one that stalls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_write_that_stalls_past_its_deadline_closes_the_connection() {
    const DEADLINE: Duration = Duration::from_millis(200);
    let (tx, rx) = mpsc::channel(4);
    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(
        &socket,
        ScriptedHandler(Script::Push(Mutex::new(Some(
            PushSource::new(rx).with_write_deadline(DEADLINE),
        )))),
    )
    .await
    .expect("bind");
    let path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });

    // `w` stays alive for the whole test: a half-closed peer is
    // indistinguishable from one that hung up, and would end the stream
    // for the wrong reason.
    let (mut reader, mut w) = dial(&path).await;
    request(&mut w, 3, "events.subscribe").await;
    let reply = read_frame(&mut reader).await;
    assert_eq!(reply["result"]["subscribed"], serde_json::json!(true));

    // From here the client reads nothing. Each message is far larger
    // than any socket buffer, so the writer blocks on the first one.
    let fat = serde_json::json!({ "revision": 1, "pad": "x".repeat(1024 * 1024) });
    let ended = tokio::time::timeout(TIMEOUT, async {
        loop {
            if tx.send(fat.clone()).await.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "a stalled write must end the push loop, not park on it forever"
    );

    // And the client sees it: whatever made it into the buffer, then EOF.
    let closed = tokio::time::timeout(TIMEOUT, async {
        loop {
            match reader.read_line().await {
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the peer must see the connection close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_until_stops_accepting_and_cancels_live_connections() {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let dir = tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let server = IpcServer::bind(&socket, ScriptedHandler(Script::Reply))
        .await
        .expect("bind");
    let path = server.socket_path().to_path_buf();
    let running = tokio::spawn(async move {
        server
            .run_until(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    // A live, idle connection: served, then parked on its read loop.
    let (mut reader, mut w) = dial(&path).await;
    request(&mut w, 1, "identify").await;
    assert_eq!(read_frame(&mut reader).await["result"]["served"], true);

    shutdown_tx.send(()).unwrap();
    running
        .await
        .expect("accept task")
        .expect("run_until returns cleanly");

    // The live connection was cancelled: its read half sees EOF.
    let tail = tokio::time::timeout(TIMEOUT, reader.read_line())
        .await
        .expect("eof")
        .expect("read");
    assert!(tail.is_none(), "live connection should be cancelled");

    // Nothing is accepting any more: a fresh dial either fails outright
    // (the listener is closed) or gets an immediate EOF.
    if let Ok(stream) = UnixStream::connect(&path).await {
        let (r, mut w) = stream.into_split();
        let mut reader = FrameReader::new(r);
        let bad =
            serde_json::to_vec(&serde_json::json!({"id":"1","op":"identify","params":{}})).unwrap();
        let _ = write_frame(&mut w, &bad).await;
        let tail = tokio::time::timeout(TIMEOUT, reader.read_line())
            .await
            .expect("eof");
        assert!(
            matches!(tail, Ok(None) | Err(_)),
            "a post-shutdown dial must not be served"
        );
    }
}

async fn dial(
    path: &std::path::Path,
) -> (
    FrameReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
) {
    for _ in 0..200 {
        if let Ok(stream) = UnixStream::connect(path).await {
            let (r, w) = stream.into_split();
            return (FrameReader::new(r), w);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server never came up at {}", path.display());
}

async fn request(w: &mut tokio::net::unix::OwnedWriteHalf, id: i64, op: &str) {
    let body = serde_json::to_vec(&serde_json::json!({
        "id": id.to_string(),
        "op": op,
        "params": {},
    }))
    .unwrap();
    write_frame(w, &body).await.expect("write request");
}

/// The next frame on the connection, whether it is a response envelope
/// or a push message.
async fn read_frame(
    reader: &mut FrameReader<tokio::net::unix::OwnedReadHalf>,
) -> serde_json::Value {
    let line = reader
        .read_line()
        .await
        .expect("read")
        .expect("expected a frame");
    serde_json::from_slice(&line).expect("valid JSON frame")
}
