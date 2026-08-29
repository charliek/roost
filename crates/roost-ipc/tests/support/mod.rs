//! A scriptable stand-in for a host session, for testing the client
//! transports in [`roost_ipc::client`].
//!
//! `crates/roost-session/tests/attach_stream_test.rs` proves the client
//! against a *real* `serve()` — that lane is the fidelity proof and
//! nothing here replaces it. What a real session cannot do is misbehave
//! on demand: a seq gap, a duplicate, an identity that does not match
//! what was asked for, an EOF in the middle of a snapshot. Those are
//! the client's discipline, and they only get tested against a server
//! that will produce them.
//!
//! So this is a [`Plan`] — a value describing exactly what one
//! connection will be answered with — plus a listener that carries it
//! out and records what arrived. Nothing adaptive, nothing timed: a
//! test writes the wire it wants to see and asserts on what the client
//! did with it.
//!
//! The first-line sniff mirrors the real server's
//! (`roost-ipc/src/server.rs`): an object carrying `attach` and no `op`
//! is a data handshake, anything else is a request connection.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use roost_ipc::dataframe::{
    write_data_frame, write_preamble, DataFrame, DataFrameReader, FRAME_ERROR, FRAME_EXIT,
    FRAME_PTY, FRAME_SNAP,
};
use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::messages::{
    ops, AttachAccepted, AttachHandshake, AttachHandshakeReply, AttachMode, AttachPayloadKind,
    EventBatch, EventEnvelope, EventsSubscribeResult, RawRequest, Response, ResponseError,
    SessionStoppingEvent, SESSION_STOPPING_EVENT,
};
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------
// The script
// ---------------------------------------------------------------------

/// A canned control-plane answer.
#[derive(Debug, Clone)]
pub enum Reply {
    Ok(serde_json::Value),
    Err(ResponseError),
}

/// How `events.subscribe` is answered.
#[derive(Debug, Clone)]
pub enum Subscribe {
    /// Ack at this fence revision, then push.
    Ack(u64),
    /// Refuse — `connect-required` and `taken-over` are the two the
    /// wire defines, and they instruct differently.
    Reject(ResponseError),
}

impl Default for Subscribe {
    fn default() -> Self {
        Subscribe::Ack(0)
    }
}

/// One frame pushed on a subscribed connection.
#[derive(Debug, Clone)]
pub enum Push {
    Batch(EventBatch),
    /// The terminal control envelope, with its reason.
    Stopping(String),
    /// Whatever JSON the test wants on the wire — an envelope from a
    /// newer server, or something malformed.
    Raw(serde_json::Value),
}

impl Push {
    /// A commit carrying no events, which is what an empty commit
    /// looks like and what makes a gap mean loss.
    pub fn empty(revision: u64) -> Push {
        Push::Batch(EventBatch {
            revision,
            events: Vec::new(),
        })
    }

    pub fn batch(revision: u64, events: Vec<EventEnvelope>) -> Push {
        Push::Batch(EventBatch { revision, events })
    }
}

/// How a data handshake is answered.
#[derive(Debug, Clone)]
pub enum Handshake {
    Accept(AttachAccepted),
    /// Written, then the connection closes with nothing binary after
    /// it.
    Reject(ResponseError),
}

impl Default for Handshake {
    fn default() -> Self {
        Handshake::Accept(accepted(AttachMode::Snapshot, 0))
    }
}

/// An accepted reply at `seq`, with the identity a resume is matched
/// against. Tests that need a *mismatched* identity build their own.
pub fn accepted(mode: AttachMode, seq: u64) -> AttachAccepted {
    AttachAccepted {
        kind: AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT),
        mode,
        seq,
        server_epoch: STUB_EPOCH,
        tab_generation: STUB_GENERATION,
    }
}

/// The identity [`accepted`] stamps. A test forcing an identity
/// mismatch hands the client one of these and the server the other.
pub const STUB_EPOCH: u64 = 6_032_428_321_756_423_947;
pub const STUB_GENERATION: u64 = 3;

/// One frame served on a data connection, after the preamble.
#[derive(Debug, Clone)]
pub enum Serve {
    Snap(Vec<u8>),
    Pty {
        seq: u64,
        bytes: Vec<u8>,
    },
    Exit {
        final_seq: u64,
        code: i32,
    },
    Error {
        code: String,
        message: String,
    },
    /// An arbitrary type byte and payload — a width the protocol
    /// forbids, or a type the server may not send.
    Raw {
        frame_type: u8,
        payload: Vec<u8>,
    },
    /// Raw bytes straight onto the socket, below the framer. The only
    /// way to produce a header with no payload behind it, which is what
    /// separates "the peer finished" from "the peer died mid-frame".
    Bytes(Vec<u8>),
}

impl Serve {
    pub fn pty(seq: u64, bytes: &[u8]) -> Serve {
        Serve::Pty {
            seq,
            bytes: bytes.to_vec(),
        }
    }

    pub fn error(code: &str, message: &str) -> Serve {
        Serve::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// What the stub does once it has run out of script.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum End {
    /// Drop the connection. On a data connection mid-snapshot this is
    /// the EOF-before-FINISH fault.
    #[default]
    Close,
    /// Keep it open and idle — for a test that has to observe what the
    /// client writes after it has read everything.
    Hold,
}

/// Everything one stub will do. Cheap to clone; every connection it
/// accepts is served from the same script.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Canned control-plane answers, by op name. An op with no entry
    /// gets `unknown-op`, exactly as a socket that does not serve it
    /// would.
    pub replies: HashMap<String, Reply>,
    pub subscribe: Subscribe,
    pub pushes: Vec<Push>,
    pub after_push: End,
    pub handshake: Handshake,
    pub serves: Vec<Serve>,
    pub after_serve: End,
}

impl Plan {
    pub fn new() -> Plan {
        Plan::default()
    }

    pub fn reply(mut self, op: &str, result: serde_json::Value) -> Plan {
        self.replies.insert(op.into(), Reply::Ok(result));
        self
    }

    pub fn refuse(mut self, op: &str, code: &str, message: &str) -> Plan {
        self.replies.insert(
            op.into(),
            Reply::Err(ResponseError {
                code: code.into(),
                message: message.into(),
            }),
        );
        self
    }

    pub fn subscribe(mut self, subscribe: Subscribe) -> Plan {
        self.subscribe = subscribe;
        self
    }

    pub fn push(mut self, push: Push) -> Plan {
        self.pushes.push(push);
        self
    }

    pub fn after_push(mut self, end: End) -> Plan {
        self.after_push = end;
        self
    }

    pub fn handshake(mut self, handshake: Handshake) -> Plan {
        self.handshake = handshake;
        self
    }

    pub fn serve(mut self, serve: Serve) -> Plan {
        self.serves.push(serve);
        self
    }

    pub fn after_serve(mut self, end: End) -> Plan {
        self.after_serve = end;
        self
    }
}

// ---------------------------------------------------------------------
// What arrived
// ---------------------------------------------------------------------

/// Everything the stub was sent, for a test that asserts on the
/// client's half of the exchange.
#[derive(Debug, Default)]
pub struct Recorded {
    requests: Mutex<Vec<RawRequest>>,
    handshakes: Mutex<Vec<AttachHandshake>>,
    frames: Mutex<Vec<DataFrame>>,
}

impl Recorded {
    pub fn requests(&self) -> Vec<RawRequest> {
        self.requests.lock().expect("requests").clone()
    }

    pub fn handshakes(&self) -> Vec<AttachHandshake> {
        self.handshakes.lock().expect("handshakes").clone()
    }

    /// Client → server data frames, in arrival order.
    pub fn frames(&self) -> Vec<DataFrame> {
        self.frames.lock().expect("frames").clone()
    }

    /// How many have arrived. Polling this instead of [`Self::frames`]
    /// keeps a wait from deep-cloning every recorded payload — the
    /// oversized-paste lane records a 1 MiB frame.
    pub fn frame_count(&self) -> usize {
        self.frames.lock().expect("frames").len()
    }
}

// ---------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------

/// A bound socket serving [`Plan`]. Dropping it stops the accept loop;
/// the temp dir goes with it.
pub struct Stub {
    _dir: TempDir,
    path: PathBuf,
    recorded: Arc<Recorded>,
    accept: JoinHandle<()>,
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl Stub {
    /// Bind a socket in a fresh temp dir and start serving `plan`.
    pub async fn start(plan: Plan) -> Stub {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("stub.sock");
        let listener = UnixListener::bind(&path).expect("bind the stub socket");
        let recorded = Arc::new(Recorded::default());
        let accept = tokio::spawn(accept_loop(listener, plan, Arc::clone(&recorded)));
        Stub {
            _dir: dir,
            path,
            recorded,
            accept,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn recorded(&self) -> &Recorded {
        &self.recorded
    }

    /// Wait until at least `n` client → server frames have arrived, then
    /// hand back everything recorded. Unbounded on purpose: the caller
    /// owns the deadline, so a stall is reported as that test's budget
    /// rather than as a wrong frame count.
    pub async fn frames_at_least(&self, n: usize) -> Vec<DataFrame> {
        while self.recorded.frame_count() < n {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        self.recorded.frames()
    }
}

async fn accept_loop(listener: UnixListener, plan: Plan, recorded: Arc<Recorded>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let plan = plan.clone();
        let recorded = Arc::clone(&recorded);
        tokio::spawn(async move {
            serve_conn(stream, plan, recorded).await;
        });
    }
}

async fn serve_conn(stream: UnixStream, plan: Plan, recorded: Arc<Recorded>) {
    let (read, mut write) = stream.into_split();
    let mut lines = FrameReader::new(read);
    let Ok(Some(line)) = lines.read_line().await else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
        return;
    };

    // The sniff: an object with `attach` and no `op`, first line only.
    let is_handshake =
        value.is_object() && value.get("attach").is_some() && value.get("op").is_none();
    if is_handshake {
        let Ok(handshake) = serde_json::from_value::<AttachHandshake>(value) else {
            return;
        };
        recorded
            .handshakes
            .lock()
            .expect("handshakes")
            .push(handshake);
        serve_data(lines, write, plan, recorded).await;
        return;
    }

    let mut pending = Some(value);
    loop {
        let value = match pending.take() {
            Some(value) => value,
            None => {
                let Ok(Some(line)) = lines.read_line().await else {
                    return;
                };
                let Ok(value) = serde_json::from_slice(&line) else {
                    return;
                };
                value
            }
        };
        let Ok(request) = serde_json::from_value::<RawRequest>(value) else {
            return;
        };
        recorded
            .requests
            .lock()
            .expect("requests")
            .push(request.clone());

        if request.op == ops::EVENTS_SUBSCRIBE {
            match plan.subscribe.clone() {
                Subscribe::Reject(error) => {
                    reply(
                        &mut write,
                        Response::err(request.id, error.code, error.message),
                    )
                    .await;
                    continue;
                }
                Subscribe::Ack(revision) => {
                    let ack = serde_json::to_value(EventsSubscribeResult { revision })
                        .expect("encode the subscribe ack");
                    reply(&mut write, Response::ok(request.id, ack)).await;
                    serve_push(write, plan).await;
                    return;
                }
            }
        }

        let answer = plan.replies.get(&request.op).cloned().unwrap_or_else(|| {
            Reply::Err(ResponseError {
                code: "unknown-op".into(),
                message: format!("unknown op: {}", request.op),
            })
        });
        let response = match answer {
            Reply::Ok(result) => Response::ok(request.id, result),
            Reply::Err(error) => Response::err(request.id, error.code, error.message),
        };
        reply(&mut write, response).await;
    }
}

async fn reply<W: tokio::io::AsyncWrite + Unpin>(write: &mut W, response: Response) {
    let body = serde_json::to_vec(&response).expect("encode a response");
    let _ = write_frame(write, &body).await;
}

async fn serve_push<W: tokio::io::AsyncWrite + Unpin>(mut write: W, plan: Plan) {
    for push in &plan.pushes {
        let body = match push {
            Push::Batch(batch) => serde_json::to_vec(batch).expect("encode a batch"),
            Push::Stopping(reason) => serde_json::to_vec(&EventEnvelope {
                event: SESSION_STOPPING_EVENT.to_string(),
                data: serde_json::to_value(SessionStoppingEvent {
                    reason: reason.clone(),
                })
                .expect("encode the stopping reason"),
            })
            .expect("encode the stopping envelope"),
            Push::Raw(value) => serde_json::to_vec(value).expect("encode a raw push"),
        };
        if write_frame(&mut write, &body).await.is_err() {
            return;
        }
    }
    if plan.after_push == End::Hold {
        std::future::pending::<()>().await;
    }
}

async fn serve_data(
    lines: FrameReader<tokio::net::unix::OwnedReadHalf>,
    mut write: tokio::net::unix::OwnedWriteHalf,
    plan: Plan,
    recorded: Arc<Recorded>,
) {
    let reply = match &plan.handshake {
        Handshake::Accept(accepted) => AttachHandshakeReply::Accepted(accepted.clone()),
        Handshake::Reject(error) => AttachHandshakeReply::Rejected(error.clone()),
    };
    let body = serde_json::to_vec(&reply).expect("encode the handshake reply");
    if write_frame(&mut write, &body).await.is_err() {
        return;
    }
    // A refusal is the connection's last word, and nothing binary ever
    // follows it.
    if matches!(plan.handshake, Handshake::Reject(_)) {
        return;
    }
    if write_preamble(&mut write).await.is_err() {
        return;
    }

    // The client's own frames land here while the script plays out.
    let (read, residue) = lines.into_parts();
    let reader_recorded = Arc::clone(&recorded);
    let inbound = tokio::spawn(async move {
        let mut reader = DataFrameReader::new(read, residue);
        while let Ok(Some(frame)) = reader.next_frame().await {
            reader_recorded.frames.lock().expect("frames").push(frame);
        }
    });

    for serve in &plan.serves {
        let (frame_type, payload) = match serve {
            // Straight onto the socket, below the framer — the one way
            // to write a header with no payload behind it.
            Serve::Bytes(bytes) => {
                use tokio::io::AsyncWriteExt;
                if write.write_all(bytes).await.is_err() || write.flush().await.is_err() {
                    break;
                }
                continue;
            }
            Serve::Snap(bytes) => (FRAME_SNAP, bytes.clone()),
            Serve::Pty { seq, bytes } => {
                let mut payload = seq.to_le_bytes().to_vec();
                payload.extend_from_slice(bytes);
                (FRAME_PTY, payload)
            }
            Serve::Exit { final_seq, code } => {
                let mut payload = final_seq.to_le_bytes().to_vec();
                payload.extend_from_slice(&code.to_le_bytes());
                (FRAME_EXIT, payload)
            }
            Serve::Error { code, message } => (
                FRAME_ERROR,
                serde_json::to_vec(&ResponseError {
                    code: code.clone(),
                    message: message.clone(),
                })
                .expect("encode an ERROR frame"),
            ),
            Serve::Raw {
                frame_type,
                payload,
            } => (*frame_type, payload.clone()),
        };
        if write_data_frame(&mut write, frame_type, &payload)
            .await
            .is_err()
        {
            break;
        }
    }

    if plan.after_serve == End::Hold {
        std::future::pending::<()>().await;
    }
    // Otherwise: drop the write half. Mid-script that is the
    // EOF-before-FINISH fault; after an EXIT it is the ordinary close.
    drop(write);
    inbound.abort();
}
