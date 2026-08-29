//! Client transports for a Roost socket: the sequential request/
//! response client, the events push reader, and the attach data plane.
//!
//! Three shapes, because the wire has three:
//!
//! * [`IpcClient`] — one request in flight at a time, responses matched
//!   by id. The CLI (`roostctl`) is the primary consumer; a host
//!   client's control connection is the other. Unsolicited event
//!   envelopes mid-stream are silently dropped here: it never sends
//!   `events.subscribe`, so none arrive in practice.
//! * [`EventStream`] — `events.subscribe` flips a connection into a
//!   one-way push stream, which is the shape [`IpcClient`] cannot have.
//!   Subscribe-then-stream lives here so an event-shaped frame never
//!   sits in the way of an ordinary `call`. Mirrors
//!   `tools/roosttest/eventstream.py`.
//! * [`DataConnection`] — a data connection opens as newline-JSON (the
//!   attach handshake and its reply) and then turns binary. The line
//!   reader routinely buffers the head of the binary stream along with
//!   the handshake line, so the hand-off carries that residue into
//!   [`crate::dataframe::DataFrameReader`] — dropping it would silently
//!   behead the stream.
//!
//! Every refusal any of the three can hand back is a stable kebab-case
//! code; [`ServerCode`] is the typed mapping a client state machine
//! matches on instead of comparing strings.
//!
//! The wire contract is `docs/reference/ipc.md`
//! (`#session-sockets`, `#data-plane`).

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use crate::dataframe::{
    write_data_frame, DataFrame, DataFrameReader, FRAME_ERROR, FRAME_EXIT, FRAME_INPUT, FRAME_PTY,
    FRAME_RESIZE, FRAME_SNAP, MAX_DATA_FRAME_BYTES,
};
use crate::framing::{write_frame, FrameReader};
use crate::messages::{
    ops, AttachAccepted, AttachHandshake, AttachHandshakeReply, EventBatch, EventsSubscribeParams,
    EventsSubscribeResult, IdentifyParams, IdentifyResult, RawRequest, Response, ResponseError,
    SessionStoppingEvent, SESSION_STOPPING_EVENT,
};
use crate::Error;

// ============================================================================
// Sequential request/response client
// ============================================================================

/// Single-connection sequential client.
pub struct IpcClient {
    reader: FrameReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: AtomicI64,
}

impl IpcClient {
    /// Dial the socket at `path`.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, Error> {
        let stream = UnixStream::connect(path.as_ref()).await?;
        Ok(Self::over(stream))
    }

    /// Wrap an already-dialed stream.
    pub fn over(stream: UnixStream) -> Self {
        let (r, w) = stream.into_split();
        Self {
            reader: FrameReader::new(r),
            writer: w,
            next_id: AtomicI64::new(1),
        }
    }

    fn alloc_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a request and wait for the matching response.
    ///
    /// `op` is the dotted-lowercase op name (e.g. `"tab.open"`).
    /// `params` serializes to the JSON params object; pass
    /// `serde_json::json!({})` for empty.
    ///
    /// Returns the raw `result` value on success. Maps the
    /// server-side error envelope into [`ClientError::Server`].
    pub async fn call_raw<P: Serialize>(
        &mut self,
        op: &str,
        params: P,
    ) -> Result<serde_json::Value, ClientError> {
        let id = self.alloc_id();
        let request = RawRequest {
            id,
            op: op.into(),
            params: serde_json::to_value(params).map_err(Error::from)?,
        };
        let line = serde_json::to_vec(&request).map_err(Error::from)?;
        write_frame(&mut self.writer, &line).await?;

        // Read frames until we see one with our id. The M2 client
        // ignores unsolicited event frames; future event-aware
        // clients should consume them.
        loop {
            let frame = match self.reader.read_line().await? {
                Some(f) => f,
                None => return Err(ClientError::Disconnected),
            };

            // Try to decode as a response. If decoding fails, surface
            // the parse error: the server should never send us
            // anything that isn't a response or an event envelope.
            let v: serde_json::Value = serde_json::from_slice(&frame).map_err(Error::from)?;
            if v.get("event").is_some() {
                // Skip event envelopes — M2 client doesn't subscribe.
                continue;
            }
            let resp: Response = serde_json::from_value(v).map_err(Error::from)?;
            if resp.id != id {
                return Err(ClientError::IdMismatch {
                    expected: id,
                    got: resp.id,
                });
            }
            if !resp.ok {
                let err = resp.error.unwrap_or(ResponseError {
                    code: "internal".into(),
                    message: "server returned ok=false without error body".into(),
                });
                return Err(ClientError::Server {
                    code: err.code,
                    message: err.message,
                });
            }
            return Ok(resp.result.unwrap_or(serde_json::Value::Null));
        }
    }

    /// Typed convenience over [`Self::call_raw`].
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        op: &str,
        params: P,
    ) -> Result<R, ClientError> {
        let raw = self.call_raw(op, params).await?;
        // Decoding the result is schema/protocol drift — not a
        // transport failure. Surface it as `Protocol` so callers
        // can distinguish "the wire died" from "the server sent
        // something we couldn't parse." CR-flagged on PR #78.
        serde_json::from_value(raw).map_err(|e| ClientError::Protocol(Error::from(e)))
    }

    /// Convenience: send an `identify` request and decode the result.
    pub async fn identify(
        &mut self,
        params: IdentifyParams,
    ) -> Result<IdentifyResult, ClientError> {
        self.call(ops::IDENTIFY, params).await
    }

    /// Send `events.subscribe` and hand the connection over to the push
    /// reader.
    ///
    /// Consuming `self` is the contract, not a convenience: the ack is
    /// the last request/response frame this connection will ever carry,
    /// and everything after it is an [`EventBatch`]. A client that kept
    /// a callable handle would be holding one that can only ever return
    /// an event-shaped frame to the wrong caller.
    ///
    /// The lease is the one [`ops::SESSION_CONNECT`] handed out; without
    /// a live one the server answers `connect-required`, and with one
    /// another client has since taken, `taken-over` — see
    /// [`ServerCode`].
    pub async fn subscribe_events(mut self, lease: &str) -> Result<EventStream, ClientError> {
        let ack: EventsSubscribeResult = self
            .call(
                ops::EVENTS_SUBSCRIBE,
                EventsSubscribeParams {
                    lease: lease.to_string(),
                    // HS-2 scope: subscribe unfiltered and filter
                    // client-side. A non-zero value is refused.
                    tab_id_filter: 0,
                },
            )
            .await?;
        Ok(EventStream {
            reader: self.reader,
            writer: self.writer,
            revision: ack.revision,
            next_revision: ack.revision.saturating_add(1),
            stopping: None,
        })
    }
}

// ============================================================================
// Events push reader
// ============================================================================

/// One frame off a subscribed connection.
///
/// Every frame is a batch except the single terminal control envelope
/// that ends the stream. Envelopes that carry neither a `revision` nor
/// the [`SESSION_STOPPING_EVENT`] name are skipped rather than
/// surfaced — additive server-side control frames must not break a
/// client that predates them (`ipc.md` #versioning).
#[derive(Debug, Clone, PartialEq)]
pub enum EventFrame {
    /// One workspace commit. Empty commits are pushed too, which is
    /// what makes a revision gap mean loss and nothing else.
    Batch(EventBatch),
    /// The stream is over and says why. Always the last frame before
    /// the close.
    Stopping(SessionStoppingEvent),
}

/// A subscribed connection, reading the server's push stream.
///
/// Built by [`IpcClient::subscribe_events`] or [`EventStream::connect`].
/// Loss detection is built in: batches must arrive `revision + 1`,
/// `revision + 2`, … and a hole is [`ClientError::RevisionGap`], which
/// is the client's cue to resync (fresh `tab.list`, re-subscribe, fence
/// against the new ack).
pub struct EventStream {
    reader: FrameReader<OwnedReadHalf>,
    /// Held open on purpose. The server keeps reading this connection
    /// so it notices a peer that goes away; half-closing the write half
    /// is how a peer says it is gone, and the server would end the
    /// stream.
    #[allow(dead_code)]
    writer: OwnedWriteHalf,
    revision: u64,
    next_revision: u64,
    stopping: Option<SessionStoppingEvent>,
}

impl EventStream {
    /// Dial `path` and subscribe on the connection in one step.
    pub async fn connect(path: impl AsRef<Path>, lease: &str) -> Result<Self, ClientError> {
        IpcClient::connect(path)
            .await?
            .subscribe_events(lease)
            .await
    }

    /// The ack's fence: the commit this subscription starts from.
    ///
    /// The client already has everything at or below it, so a
    /// `tab.list` taken alongside is fenced by discarding every batch
    /// `<=` this and applying the rest (`ipc.md` #eventssubscribe).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Why the stream ended, once the terminal envelope has arrived.
    /// `"stop"` — the session is shutting down; `"taken-over"` —
    /// another client took the lease.
    pub fn stopping_reason(&self) -> Option<&str> {
        self.stopping.as_ref().map(|s| s.reason.as_str())
    }

    /// The next pushed frame, or `Ok(None)` when the server closed the
    /// stream.
    ///
    /// A close is itself a documented signal — it is how the session
    /// says "resync" — so it is not an error. Check
    /// [`Self::stopping_reason`] to tell a labeled goodbye from a bare
    /// EOF; both mean reconnect, the label says whether it is worth
    /// trying.
    pub async fn next(&mut self) -> Result<Option<EventFrame>, ClientError> {
        // The terminal envelope latches: it is defined as the last frame,
        // so a peer that keeps the socket open (or writes more) after it
        // must not have this reader block on — or worse, yield —
        // post-terminal frames.
        if self.stopping.is_some() {
            return Ok(None);
        }
        loop {
            let line = match self.reader.read_line().await? {
                Some(line) => line,
                None => return Ok(None),
            };
            let value: serde_json::Value = serde_json::from_slice(&line).map_err(Error::from)?;
            // A commit batch is `revision` + `events`, both present. A
            // frame with only one of them is not a commit — decoding it
            // as an empty batch would advance the fence off something
            // that never was one.
            if value.get("revision").is_none() || value.get("events").is_none() {
                let name = value.get("event").and_then(|e| e.as_str()).unwrap_or("");
                if name == SESSION_STOPPING_EVENT {
                    let stopping: SessionStoppingEvent = value
                        .get("data")
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose()
                        .map_err(Error::from)?
                        .unwrap_or_default();
                    // A labeled goodbye with no label is not one: fall
                    // through to the bare-EOF path the contract already
                    // prescribes for an unlabeled close.
                    if stopping.reason.is_empty() {
                        tracing::debug!("session.stopping without a reason; treating as EOF");
                        continue;
                    }
                    self.stopping = Some(stopping.clone());
                    return Ok(Some(EventFrame::Stopping(stopping)));
                }
                tracing::debug!(event = %name, "ignoring an unrecognized push envelope");
                continue;
            }
            let batch: EventBatch = serde_json::from_value(value).map_err(Error::from)?;
            if batch.revision != self.next_revision {
                return Err(ClientError::RevisionGap {
                    expected: self.next_revision,
                    got: batch.revision,
                });
            }
            // Saturating on purpose: a peer claiming u64::MAX must not
            // panic the fence arithmetic; the pinned fence then makes the
            // next batch a RevisionGap, which is the resync path anyway.
            self.next_revision = batch.revision.saturating_add(1);
            return Ok(Some(EventFrame::Batch(batch)));
        }
    }
}

// ============================================================================
// Attach data plane
// ============================================================================

/// One server→client frame, with the per-type widths `ipc.md` makes
/// fatal validated.
///
/// [`crate::dataframe`] deliberately stops at the length cap — the one
/// rule that has to precede the allocation. Everything below is the
/// endpoint's half of that split, and it lives here because the client
/// is the endpoint that knows a `PTY` frame carries a seq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFrame {
    /// The next bytes of the encoded snapshot stream. The one type for
    /// which a zero-length payload is meaningful.
    Snap(Vec<u8>),
    /// Live terminal output. `seq` must be exactly the previous one
    /// plus one; a gap or a duplicate is fatal and re-attach is the
    /// recovery.
    Pty { seq: u64, bytes: Vec<u8> },
    /// Always the last frame on a connection that sees one.
    /// `final_seq` is one past the last `PTY` seq — the exit consumes
    /// an ordinal of its own.
    Exit { final_seq: u64, code: i32 },
    /// A stable diagnostic code; the connection closes after it. Map it
    /// with [`ServerCode`].
    Error(ResponseError),
}

impl ServerFrame {
    /// Decode one framed payload. Rejects a width the protocol forbids
    /// and an unknown type byte, both naming what was seen.
    pub fn decode(frame: DataFrame) -> Result<ServerFrame, Error> {
        match frame.frame_type {
            FRAME_SNAP => Ok(ServerFrame::Snap(frame.payload)),
            FRAME_PTY => {
                if frame.payload.len() < 9 {
                    return Err(Error::DataProtocol(format!(
                        "a PTY frame carries a u64 seq and at least one byte, got {}",
                        frame.payload.len()
                    )));
                }
                let mut bytes = frame.payload;
                let seq = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
                // The framer already handed up an owned buffer; strip
                // the seq in place rather than reallocating the whole
                // payload on every frame of terminal output.
                bytes.drain(..8);
                Ok(ServerFrame::Pty { seq, bytes })
            }
            FRAME_EXIT => {
                if frame.payload.len() != 12 {
                    return Err(Error::DataProtocol(format!(
                        "an EXIT frame is u64 final_seq + i32 code = 12 bytes, got {}",
                        frame.payload.len()
                    )));
                }
                let final_seq = u64::from_le_bytes(frame.payload[..8].try_into().expect("8 bytes"));
                let code = i32::from_le_bytes(frame.payload[8..].try_into().expect("4 bytes"));
                Ok(ServerFrame::Exit { final_seq, code })
            }
            FRAME_ERROR => {
                let error: ResponseError =
                    serde_json::from_slice(&frame.payload).map_err(Error::from)?;
                Ok(ServerFrame::Error(error))
            }
            other => Err(Error::DataProtocol(format!(
                "unexpected data frame type {other:#04x} from the server"
            ))),
        }
    }
}

/// The client half of an attach data connection: framed reads one way,
/// input and resizes the other.
///
/// Generic over the read half so a caller can interpose on the socket
/// — the fragmentation tests wrap it in a one-byte-at-a-time reader,
/// which is what proves nothing in the decode depends on a frame
/// arriving whole.
pub struct DataConnection<R = OwnedReadHalf, W = OwnedWriteHalf> {
    reader: DataFrameReader<R>,
    writer: DataWriter<W>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> DataConnection<R, W> {
    /// Run the handshake on an already-split connection: one JSON line
    /// out, one JSON line back, then the preamble and binary for the
    /// rest of its life.
    ///
    /// The residue hand-off is the load-bearing step. A refusal is
    /// [`ClientError::Server`] and nothing binary follows it, so a
    /// client that got one never has to guess whether the bytes after
    /// it are frames.
    pub async fn handshake(
        read: R,
        mut write: W,
        request: &AttachHandshake,
    ) -> Result<(AttachAccepted, Self), ClientError> {
        let body = serde_json::to_vec(request).map_err(Error::from)?;
        write_frame(&mut write, &body).await?;

        let mut lines = FrameReader::new(read);
        let line = match lines.read_line().await? {
            Some(line) => line,
            None => return Err(ClientError::Disconnected),
        };
        let accepted = match serde_json::from_slice(&line).map_err(Error::from)? {
            AttachHandshakeReply::Accepted(accepted) => accepted,
            AttachHandshakeReply::Rejected(error) => {
                return Err(ClientError::Server {
                    code: error.code,
                    message: error.message,
                })
            }
        };

        // The handshake line and the first binary bytes routinely
        // arrive in one read, so what the line reader already buffered
        // has to be consumed before the socket is touched again.
        let (read, residue) = lines.into_parts();
        let mut reader = DataFrameReader::new(read, residue);
        reader.read_preamble().await?;
        Ok((
            accepted,
            DataConnection {
                reader,
                writer: DataWriter { writer: write },
            },
        ))
    }

    /// The next frame, or `Ok(None)` on a clean EOF at a frame
    /// boundary. An EOF part-way through a header or payload is
    /// [`Error::UnexpectedEof`].
    pub async fn next_frame(&mut self) -> Result<Option<DataFrame>, Error> {
        self.reader.next_frame().await
    }

    /// [`Self::next_frame`], with the payload decoded and its width
    /// validated.
    pub async fn next_server_frame(&mut self) -> Result<Option<ServerFrame>, Error> {
        match self.reader.next_frame().await? {
            Some(frame) => ServerFrame::decode(frame).map(Some),
            None => Ok(None),
        }
    }

    /// See [`DataWriter::send_frame`].
    pub async fn send_frame(&mut self, frame_type: u8, payload: &[u8]) -> Result<(), Error> {
        self.writer.send_frame(frame_type, payload).await
    }

    /// See [`DataWriter::send_input`].
    pub async fn send_input(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.writer.send_input(bytes).await
    }

    /// See [`DataWriter::send_resize`].
    pub async fn send_resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_w_px: u16,
        cell_h_px: u16,
    ) -> Result<(), Error> {
        self.writer
            .send_resize(cols, rows, cell_w_px, cell_h_px)
            .await
    }

    /// Split the halves apart: the reader drives a decode loop on its
    /// own task while input keeps flowing from wherever the UI runs.
    pub fn into_split(self) -> (DataFrameReader<R>, DataWriter<W>) {
        (self.reader, self.writer)
    }
}

impl DataConnection<OwnedReadHalf, OwnedWriteHalf> {
    /// Dial `path` and run the handshake.
    pub async fn dial(
        path: impl AsRef<Path>,
        request: &AttachHandshake,
    ) -> Result<(AttachAccepted, Self), ClientError> {
        let stream = UnixStream::connect(path.as_ref())
            .await
            .map_err(Error::from)?;
        let (read, write) = stream.into_split();
        Self::handshake(read, write, request).await
    }
}

/// The write half of a split [`DataConnection`].
pub struct DataWriter<W = OwnedWriteHalf> {
    writer: W,
}

impl<W: AsyncWrite + Unpin> DataWriter<W> {
    /// Client → server keystrokes and pastes, **split at the 1 MiB
    /// frame cap** — that split is the client's job, not something the
    /// server does for it. An empty slice writes nothing: an empty
    /// `INPUT` frame is a protocol error.
    pub async fn send_input(&mut self, bytes: &[u8]) -> Result<(), Error> {
        for chunk in bytes.chunks(MAX_DATA_FRAME_BYTES) {
            write_data_frame(&mut self.writer, FRAME_INPUT, chunk).await?;
        }
        Ok(())
    }

    /// Client → server geometry. The pixel dimensions are load-bearing,
    /// not decoration: the server terminal's resize and its mode-2048
    /// size reports both need them.
    pub async fn send_resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_w_px: u16,
        cell_h_px: u16,
    ) -> Result<(), Error> {
        let mut payload = [0u8; 8];
        for (slot, value) in payload
            .chunks_exact_mut(2)
            .zip([cols, rows, cell_w_px, cell_h_px])
        {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        write_data_frame(&mut self.writer, FRAME_RESIZE, &payload).await
    }

    /// Write one raw frame. Prefer [`Self::send_input`] /
    /// [`Self::send_resize`]; this is the escape hatch for a test that
    /// needs to put a specific byte on the wire.
    pub async fn send_frame(&mut self, frame_type: u8, payload: &[u8]) -> Result<(), Error> {
        write_data_frame(&mut self.writer, frame_type, payload).await
    }
}

// ============================================================================
// Typed refusals
// ============================================================================

/// A server refusal, as something a state machine can match on.
///
/// The wire carries stable kebab-case strings; comparing them at every
/// decision point is how a client ends up reacting to a typo. The
/// variants below cover the three places a refusal can come from — a
/// response envelope's `error.code`, a rejected attach handshake, and
/// an `ERROR` data frame — because a client that has to reconnect does
/// not care which of the three said so.
///
/// [`ServerCode::Other`] keeps the string: an unrecognized code is a
/// newer server's, and losing it would make the log useless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerCode {
    // -- lease / session lifecycle ---------------------------------------
    /// No live lease. Run `session.connect` and retry.
    ConnectRequired,
    /// Another client took the lease. Stop driving this session; a
    /// deliberate reconnect is a takeover back.
    TakenOver,
    /// A lease is live and `takeover` was not set.
    AlreadyConnected,
    /// `session.stop` has latched.
    ShuttingDown,
    // -- attach negotiation ----------------------------------------------
    /// The two ends' libghostty builds disagree — the upgrade flow.
    BuildMismatch,
    /// No payload kind in common.
    UnsupportedKind,
    /// 16 unconsumed attach tokens already exist.
    TooManyTokens,
    /// No such tab, or it was respawned between the ticket and the dial.
    NotFound,
    /// Malformed parameter.
    InvalidParam,
    /// The handshake's `protocol_version` is not this session's.
    ProtocolMismatch,
    /// Unknown, expired, already-used, or takeover-purged attach token.
    InvalidToken,
    /// The terminal could not be encoded right now. Re-attach.
    SnapshotFailed,
    /// This socket serves no data connections (a UI socket).
    NotSupported,
    // -- data-plane stream faults ----------------------------------------
    /// A gap or duplicate `seq`, a lagged tee, a blown attach budget.
    /// Re-attach.
    Desync,
    /// The client is not reading fast enough.
    Overflow,
    /// A newer data connection took this tab.
    Superseded,
    /// The client sent something the framing forbids.
    ProtocolError,
    // -- generic ----------------------------------------------------------
    UnknownOp,
    NotImplemented,
    ParseError,
    FrameTooLarge,
    Internal,
    /// A code this build does not know, kept verbatim.
    Other(String),
}

impl ServerCode {
    /// Map a wire code onto a variant.
    pub fn from_wire(code: &str) -> ServerCode {
        match code {
            "connect-required" => ServerCode::ConnectRequired,
            "taken-over" => ServerCode::TakenOver,
            "already-connected" => ServerCode::AlreadyConnected,
            "shutting-down" => ServerCode::ShuttingDown,
            "build-mismatch" => ServerCode::BuildMismatch,
            "unsupported-kind" => ServerCode::UnsupportedKind,
            "too-many-tokens" => ServerCode::TooManyTokens,
            "not-found" => ServerCode::NotFound,
            "invalid-param" => ServerCode::InvalidParam,
            "protocol-mismatch" => ServerCode::ProtocolMismatch,
            "invalid-token" => ServerCode::InvalidToken,
            "snapshot-failed" => ServerCode::SnapshotFailed,
            "not-supported" => ServerCode::NotSupported,
            "desync" => ServerCode::Desync,
            "overflow" => ServerCode::Overflow,
            "superseded" => ServerCode::Superseded,
            "protocol-error" => ServerCode::ProtocolError,
            "unknown-op" => ServerCode::UnknownOp,
            "not-implemented" => ServerCode::NotImplemented,
            "parse-error" => ServerCode::ParseError,
            "frame-too-large" => ServerCode::FrameTooLarge,
            "internal" => ServerCode::Internal,
            other => ServerCode::Other(other.to_string()),
        }
    }

    /// The wire spelling, round-tripping [`Self::from_wire`].
    pub fn as_str(&self) -> &str {
        match self {
            ServerCode::ConnectRequired => "connect-required",
            ServerCode::TakenOver => "taken-over",
            ServerCode::AlreadyConnected => "already-connected",
            ServerCode::ShuttingDown => "shutting-down",
            ServerCode::BuildMismatch => "build-mismatch",
            ServerCode::UnsupportedKind => "unsupported-kind",
            ServerCode::TooManyTokens => "too-many-tokens",
            ServerCode::NotFound => "not-found",
            ServerCode::InvalidParam => "invalid-param",
            ServerCode::ProtocolMismatch => "protocol-mismatch",
            ServerCode::InvalidToken => "invalid-token",
            ServerCode::SnapshotFailed => "snapshot-failed",
            ServerCode::NotSupported => "not-supported",
            ServerCode::Desync => "desync",
            ServerCode::Overflow => "overflow",
            ServerCode::Superseded => "superseded",
            ServerCode::ProtocolError => "protocol-error",
            ServerCode::UnknownOp => "unknown-op",
            ServerCode::NotImplemented => "not-implemented",
            ServerCode::ParseError => "parse-error",
            ServerCode::FrameTooLarge => "frame-too-large",
            ServerCode::Internal => "internal",
            ServerCode::Other(code) => code,
        }
    }
}

impl From<&ResponseError> for ServerCode {
    fn from(error: &ResponseError) -> ServerCode {
        ServerCode::from_wire(&error.code)
    }
}

// ============================================================================
// Errors
// ============================================================================

/// Client-side errors. Distinct from [`crate::Error`] because the
/// server-error case is meaningful here.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Io(#[from] Error),
    /// Schema / decode failure. Distinct from `Io` so callers can
    /// distinguish "the wire died" from "the server sent
    /// something I couldn't parse." Typically indicates client +
    /// server schema drift.
    #[error("protocol error: {0}")]
    Protocol(Error),
    #[error("server returned error: {code} — {message}")]
    Server { code: String, message: String },
    #[error("response id mismatch: expected {expected}, got {got}")]
    IdMismatch { expected: i64, got: i64 },
    #[error("connection closed before response")]
    Disconnected,
    /// A batch arrived out of sequence. The only loss signal the event
    /// protocol offers, and the cue to resync rather than to guess.
    #[error("event stream skipped a revision: expected {expected}, got {got}")]
    RevisionGap { expected: u64, got: u64 },
}

impl ClientError {
    /// The typed refusal, when this error is one.
    ///
    /// Every path that can surface a server-minted code funnels through
    /// [`ClientError::Server`], so a state machine matches here instead
    /// of comparing strings at each decision point.
    pub fn server_code(&self) -> Option<ServerCode> {
        match self {
            ClientError::Server { code, .. } => Some(ServerCode::from_wire(code)),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(Error::Io(e))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_documented_code_maps_and_round_trips() {
        // The catalogues in `ipc.md`: the response envelope's, the
        // handshake rejection's, and the data-plane ERROR frame's.
        for code in [
            "connect-required",
            "taken-over",
            "already-connected",
            "shutting-down",
            "build-mismatch",
            "unsupported-kind",
            "too-many-tokens",
            "not-found",
            "invalid-param",
            "protocol-mismatch",
            "invalid-token",
            "snapshot-failed",
            "not-supported",
            "desync",
            "overflow",
            "superseded",
            "protocol-error",
            "unknown-op",
            "not-implemented",
            "parse-error",
            "frame-too-large",
            "internal",
        ] {
            let mapped = ServerCode::from_wire(code);
            assert!(
                !matches!(mapped, ServerCode::Other(_)),
                "{code} has no variant"
            );
            assert_eq!(mapped.as_str(), code);
        }
    }

    #[test]
    fn an_unknown_code_keeps_its_spelling() {
        let mapped = ServerCode::from_wire("some-future-code");
        assert_eq!(mapped, ServerCode::Other("some-future-code".into()));
        assert_eq!(mapped.as_str(), "some-future-code");
    }

    #[test]
    fn a_server_error_exposes_its_typed_code() {
        let error = ClientError::Server {
            code: "build-mismatch".into(),
            message: "…".into(),
        };
        assert_eq!(error.server_code(), Some(ServerCode::BuildMismatch));
        assert_eq!(ClientError::Disconnected.server_code(), None);
    }

    #[test]
    fn pty_frames_decode_seq_then_bytes() {
        let mut payload = 42u64.to_le_bytes().to_vec();
        payload.extend_from_slice(b"hi");
        let frame = DataFrame {
            frame_type: FRAME_PTY,
            payload,
        };
        assert_eq!(
            ServerFrame::decode(frame).unwrap(),
            ServerFrame::Pty {
                seq: 42,
                bytes: b"hi".to_vec()
            }
        );
    }

    /// The widths `ipc.md` calls fatal are the endpoint's to enforce —
    /// the framer hands up anything under the length cap.
    #[test]
    fn a_short_pty_frame_is_a_protocol_error() {
        let frame = DataFrame {
            frame_type: FRAME_PTY,
            payload: 1u64.to_le_bytes().to_vec(),
        };
        match ServerFrame::decode(frame) {
            Err(Error::DataProtocol(_)) => {}
            other => panic!("expected DataProtocol, got {other:?}"),
        }
    }

    #[test]
    fn an_exit_frame_is_exactly_twelve_bytes() {
        let mut payload = 7u64.to_le_bytes().to_vec();
        payload.extend_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(
            ServerFrame::decode(DataFrame {
                frame_type: FRAME_EXIT,
                payload: payload.clone(),
            })
            .unwrap(),
            ServerFrame::Exit {
                final_seq: 7,
                code: -1
            }
        );

        payload.push(0);
        match ServerFrame::decode(DataFrame {
            frame_type: FRAME_EXIT,
            payload,
        }) {
            Err(Error::DataProtocol(_)) => {}
            other => panic!("expected DataProtocol, got {other:?}"),
        }
    }

    #[test]
    fn an_error_frame_decodes_to_a_typed_code() {
        let frame = DataFrame {
            frame_type: FRAME_ERROR,
            payload: br#"{"code":"superseded","message":"another connection"}"#.to_vec(),
        };
        match ServerFrame::decode(frame).unwrap() {
            ServerFrame::Error(error) => {
                assert_eq!(ServerCode::from(&error), ServerCode::Superseded)
            }
            other => panic!("expected an ERROR frame, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_length_snap_is_legal() {
        assert_eq!(
            ServerFrame::decode(DataFrame {
                frame_type: FRAME_SNAP,
                payload: Vec::new(),
            })
            .unwrap(),
            ServerFrame::Snap(Vec::new())
        );
    }

    /// Client→server types have no business arriving from the server,
    /// and neither does a byte nobody defined.
    #[test]
    fn a_type_the_server_may_not_send_is_named_in_the_error() {
        for frame_type in [FRAME_INPUT, FRAME_RESIZE, 0x7E] {
            match ServerFrame::decode(DataFrame {
                frame_type,
                payload: Vec::new(),
            }) {
                Err(Error::DataProtocol(message)) => {
                    assert!(
                        message.contains(&format!("{frame_type:#04x}")),
                        "the error must name the byte: {message}"
                    );
                }
                other => panic!("expected DataProtocol, got {other:?}"),
            }
        }
    }
}
