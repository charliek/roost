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

use crate::codes;
use crate::dataframe::{
    write_data_frame, DataFrame, DataFrameReader, InputTake, ResizePayload, FRAME_DETACH,
    FRAME_ERROR, FRAME_EXIT, FRAME_GRID, FRAME_INPUT, FRAME_PTY, FRAME_RESIZE, FRAME_SNAP,
    FRAME_TAKE, MAX_DATA_FRAME_BYTES,
};
use crate::framing::{write_frame, FrameReader};
use crate::messages::{
    ops, AttachAccepted, AttachHandshake, AttachHandshakeReply, EventBatch, EventsSubscribeParams,
    EventsSubscribeResult, GridFrame, IdentifyParams, IdentifyResult, RawRequest, Response,
    ResponseError, SessionStoppingEvent, StreamEndedEvent, SESSION_STOPPING_EVENT,
    STREAM_ENDED_EVENT,
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
                    code: codes::INTERNAL.into(),
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
    pub async fn subscribe_events(self) -> Result<EventStream, ClientError> {
        self.subscribe(None, None).await
    }

    /// Resume a stream instead of starting one: `from_revision` is what
    /// the client already has, `session_id` the incarnation it fenced
    /// against ([`crate::messages::SessionIdentify::session_id`]).
    ///
    /// The batches from the gap are replayed ahead of the live ones and
    /// the whole sequence stays consecutive, so a caller's gap check
    /// does not change. A session that cannot reach that far back
    /// refuses by name — [`ServerCode::ReplayExpired`],
    /// [`ServerCode::RevisionAhead`], [`ServerCode::SessionMismatch`] —
    /// and the recovery is a plain [`Self::subscribe_events`] plus a
    /// `tab.list` snapshot.
    pub async fn resume_events(
        self,
        from_revision: u64,
        session_id: &str,
    ) -> Result<EventStream, ClientError> {
        self.subscribe(Some(from_revision), Some(session_id.to_string()))
            .await
    }

    async fn subscribe(
        mut self,
        from_revision: Option<u64>,
        session_id: Option<String>,
    ) -> Result<EventStream, ClientError> {
        let ack: EventsSubscribeResult = self
            .call(
                ops::EVENTS_SUBSCRIBE,
                EventsSubscribeParams {
                    // HS-2 scope: subscribe unfiltered and filter
                    // client-side. A non-zero value is refused.
                    tab_id_filter: 0,
                    from_revision,
                    session_id,
                },
            )
            .await?;
        Ok(EventStream {
            reader: self.reader,
            writer: self.writer,
            revision: ack.revision,
            session_id: ack.session_id,
            next_revision: ack.revision.saturating_add(1),
            stopping: None,
            ended: None,
        })
    }
}

// ============================================================================
// Events push reader
// ============================================================================

/// One frame off a subscribed connection.
///
/// Every frame is a batch except the two control envelopes. Envelopes
/// that carry neither a `revision` nor one of those names are skipped
/// rather than surfaced — additive server-side control frames must not
/// break a client that predates them (`ipc.md` #versioning).
#[derive(Debug, Clone, PartialEq)]
pub enum EventFrame {
    /// One workspace commit. Empty commits are pushed too, which is
    /// what makes a revision gap mean loss and nothing else.
    Batch(EventBatch),
    /// The stream is over and says why. Always the last frame before
    /// the close.
    Stopping(SessionStoppingEvent),
    /// A UI socket ended its in-process stream on purpose. Always the
    /// last frame before the close, like [`Self::Stopping`].
    Ended(StreamEndedEvent),
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
    session_id: String,
    next_revision: u64,
    stopping: Option<SessionStoppingEvent>,
    ended: Option<StreamEndedEvent>,
}

impl EventStream {
    /// Dial `path` and subscribe on the connection in one step.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        IpcClient::connect(path).await?.subscribe_events().await
    }

    /// The ack's fence: the commit this subscription starts from.
    ///
    /// The client already has everything at or below it, so a
    /// `tab.list` taken alongside is fenced by discarding every batch
    /// `<=` this and applying the rest (`ipc.md` #eventssubscribe).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The incarnation that answered the subscribe —
    /// [`EventsSubscribeResult::session_id`], which says why a client
    /// compares it with the one `session.identify` returned.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Why the stream ended, once the terminal envelope has arrived.
    /// `"stop"` — the session is shutting down.
    pub fn stopping_reason(&self) -> Option<&str> {
        self.stopping.as_ref().map(|s| s.reason.as_str())
    }

    /// Why a UI socket ended the stream, once its `stream.ended` envelope
    /// has arrived. `"backend-switch"` — the UI's tabs are moving to
    /// another local backend.
    pub fn ended_reason(&self) -> Option<&str> {
        self.ended.as_ref().map(|e| e.reason.as_str())
    }

    /// The next pushed frame, or `Ok(None)` when the server closed the
    /// stream.
    ///
    /// A close is itself a documented signal — it is how the session
    /// says "resync" — so it is not an error. Check
    /// [`Self::stopping_reason`] or [`Self::ended_reason`] to tell a
    /// labeled goodbye from a bare EOF; both mean reconnect, the label
    /// says whether it is worth trying.
    pub async fn next(&mut self) -> Result<Option<EventFrame>, ClientError> {
        // The terminal envelope latches: it is defined as the last frame,
        // so a peer that keeps the socket open (or writes more) after it
        // must not have this reader block on — or worse, yield —
        // post-terminal frames.
        if self.stopping.is_some() || self.ended.is_some() {
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
                if name != SESSION_STOPPING_EVENT && name != STREAM_ENDED_EVENT {
                    tracing::debug!(event = %name, "ignoring an unrecognized push envelope");
                    continue;
                }
                let reason = terminal_reason(&value)?;
                // A labeled goodbye with no label is not one: fall
                // through to the bare-EOF path the contract already
                // prescribes for an unlabeled close.
                if reason.is_empty() {
                    tracing::debug!(event = %name, "terminal envelope without a reason; treating as EOF");
                    continue;
                }
                if name == SESSION_STOPPING_EVENT {
                    let stopping = SessionStoppingEvent { reason };
                    self.stopping = Some(stopping.clone());
                    return Ok(Some(EventFrame::Stopping(stopping)));
                }
                let ended = StreamEndedEvent { reason };
                self.ended = Some(ended.clone());
                return Ok(Some(EventFrame::Ended(ended)));
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

/// The `data.reason` of a terminal envelope; empty when it has none.
///
/// Both terminal envelopes carry exactly this one field, so one decode
/// serves them.
fn terminal_reason(value: &serde_json::Value) -> Result<String, Error> {
    let data: Option<SessionStoppingEvent> = value
        .get("data")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?;
    Ok(data.unwrap_or_default().reason)
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
    /// The tab's grid or size owner changed, after `PTY` frame
    /// `grid.seq` and before the one after it. Consumes no seq: it is
    /// outside the contiguity rule `Pty` is under.
    Grid(GridFrame),
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
            FRAME_GRID => {
                let grid: GridFrame =
                    serde_json::from_slice(&frame.payload).map_err(Error::from)?;
                Ok(ServerFrame::Grid(grid))
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
        write: W,
        request: &AttachHandshake,
    ) -> Result<(AttachAccepted, Self), ClientError> {
        Self::handshake_unless(read, write, request, std::future::pending(), |_| {})
            .await?
            .ok_or(ClientError::Disconnected)
    }

    /// [`Self::handshake`], given up when `detach` resolves before the
    /// connection is ready: a `DETACH` frame goes out right behind the
    /// handshake line, which cancels an attach the server has not
    /// committed and is a deliberate detach of one it has, and the
    /// answer is `Ok(None)`. A connection that just ended would read to
    /// the server as a disconnect, which holds the tab's size for a
    /// grace period.
    ///
    /// `accepted` hears the accepted reply the moment it is parsed —
    /// the server committed the attach when it wrote that line, before
    /// the preamble that follows it.
    pub async fn handshake_unless(
        read: R,
        mut write: W,
        request: &AttachHandshake,
        detach: impl std::future::Future<Output = ()>,
        accepted: impl FnOnce(&AttachAccepted),
    ) -> Result<Option<(AttachAccepted, Self)>, ClientError> {
        let body = serde_json::to_vec(request).map_err(Error::from)?;
        write_frame(&mut write, &body).await?;

        tokio::pin!(detach);
        let mut lines = FrameReader::new(read);
        let line = tokio::select! {
            line = lines.read_line() => line?,
            () = &mut detach => {
                write_data_frame(&mut write, FRAME_DETACH, &[]).await?;
                return Ok(None);
            }
        };
        let line = match line {
            Some(line) => line,
            None => return Err(ClientError::Disconnected),
        };
        let reply = match serde_json::from_slice(&line).map_err(Error::from)? {
            AttachHandshakeReply::Accepted(reply) => reply,
            AttachHandshakeReply::Rejected(error) => {
                return Err(ClientError::Server {
                    code: error.code,
                    message: error.message,
                })
            }
        };
        accepted(&reply);

        // The handshake line and the first binary bytes routinely
        // arrive in one read, so what the line reader already buffered
        // has to be consumed before the socket is touched again.
        let (read, residue) = lines.into_parts();
        let mut reader = DataFrameReader::new(read, residue);
        tokio::select! {
            preamble = reader.read_preamble() => preamble?,
            () = &mut detach => {
                write_data_frame(&mut write, FRAME_DETACH, &[]).await?;
                return Ok(None);
            }
        }
        let accepted = reply;
        Ok(Some((
            accepted,
            DataConnection {
                reader,
                writer: DataWriter { writer: write },
            },
        )))
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
    pub async fn send_input(&mut self, bytes: &[u8], take: InputTake) -> Result<(), Error> {
        self.writer.send_input(bytes, take).await
    }

    /// See [`DataWriter::send_resize`].
    pub async fn send_resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_w_px: u16,
        cell_h_px: u16,
        focused: bool,
    ) -> Result<(), Error> {
        self.writer
            .send_resize(cols, rows, cell_w_px, cell_h_px, focused)
            .await
    }

    /// See [`DataWriter::send_take`].
    pub async fn send_take(&mut self) -> Result<(), Error> {
        self.writer.send_take().await
    }

    /// See [`DataWriter::send_detach`].
    pub async fn send_detach(&mut self) -> Result<(), Error> {
        self.writer.send_detach().await
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

    /// [`Self::dial`] by way of [`Self::handshake_unless`].
    pub async fn dial_unless(
        path: impl AsRef<Path>,
        request: &AttachHandshake,
        detach: impl std::future::Future<Output = ()>,
        accepted: impl FnOnce(&AttachAccepted),
    ) -> Result<Option<(AttachAccepted, Self)>, ClientError> {
        let stream = UnixStream::connect(path.as_ref())
            .await
            .map_err(Error::from)?;
        let (read, write) = stream.into_split();
        Self::handshake_unless(read, write, request, detach, accepted).await
    }
}

/// The write half of a split [`DataConnection`].
pub struct DataWriter<W = OwnedWriteHalf> {
    writer: W,
}

impl<W: AsyncWrite + Unpin> DataWriter<W> {
    /// Client → server keystrokes and pastes, **split at the 1 MiB
    /// frame cap** (less the flag byte) — that split is the client's
    /// job, not something the server does for it. An empty slice writes
    /// nothing: an `INPUT` frame with no bytes is a protocol error.
    ///
    /// `take` is what the whole input asks of the tab's size, so a split
    /// paste carries [`InputTake::Before`] on its first frame only and
    /// [`InputTake::After`] on its last: the take happens once, on the
    /// side of the bytes it was asked for.
    pub async fn send_input(&mut self, bytes: &[u8], take: InputTake) -> Result<(), Error> {
        let chunks = bytes.chunks(MAX_DATA_FRAME_BYTES - 1);
        let last = chunks.len().saturating_sub(1);
        let mut payload = Vec::new();
        for (at, chunk) in chunks.enumerate() {
            let flags = match take {
                InputTake::Before if at == 0 => take.flags(),
                InputTake::After if at == last => take.flags(),
                _ => InputTake::None.flags(),
            };
            payload.clear();
            payload.push(flags);
            payload.extend_from_slice(chunk);
            write_data_frame(&mut self.writer, FRAME_INPUT, &payload).await?;
        }
        Ok(())
    }

    /// Client → server geometry. The pixel dimensions are load-bearing,
    /// not decoration: the server terminal's resize and its mode-2048
    /// size reports both need them. `focused` is whether the client's
    /// window had focus when it resized — a focused resize takes the
    /// tab's size, an unfocused one only records this client's grid.
    pub async fn send_resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_w_px: u16,
        cell_h_px: u16,
        focused: bool,
    ) -> Result<(), Error> {
        let payload = ResizePayload {
            cols,
            rows,
            cell_w_px,
            cell_h_px,
            focused,
        }
        .encode();
        write_data_frame(&mut self.writer, FRAME_RESIZE, &payload).await
    }

    /// Client → server: take the tab's size at this connection's grid —
    /// after a copy, or once a local scroll has settled.
    pub async fn send_take(&mut self) -> Result<(), Error> {
        write_data_frame(&mut self.writer, FRAME_TAKE, &[]).await
    }

    /// Client → server: a deliberate detach. The server hands the size
    /// on at once and closes; nothing may be sent after it.
    pub async fn send_detach(&mut self) -> Result<(), Error> {
        write_data_frame(&mut self.writer, FRAME_DETACH, &[]).await
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
    // -- session lifecycle -----------------------------------------------
    /// `session.stop` has latched.
    ShuttingDown,
    // -- events resume ---------------------------------------------------
    /// The `from_revision` a resume presented is older than the replay
    /// ring still holds. Snapshot with `tab.list` and subscribe afresh.
    ReplayExpired,
    /// This session never produced that revision — a fence from another
    /// incarnation, or a client bug.
    RevisionAhead,
    /// The `session_id` a resume named is not this session's: the
    /// session restarted, and its revisions restarted with it.
    SessionMismatch,
    // -- attach negotiation ----------------------------------------------
    /// The two ends' libghostty builds disagree — the upgrade flow.
    BuildMismatch,
    /// No payload kind in common.
    UnsupportedKind,
    /// This session already serves `MAX_DATA_CONNS_PER_SESSION` data
    /// connections. Detach something before attaching again.
    TooManyAttaches,
    /// No such tab, or it was respawned between the handshake's
    /// admission and the hand-off that serves it.
    NotFound,
    /// Malformed parameter.
    InvalidParam,
    /// The handshake's `protocol_version` is not this session's.
    ProtocolMismatch,
    /// The terminal could not be encoded right now. Re-attach.
    SnapshotFailed,
    /// This socket serves no data connections (a UI socket), or the
    /// session was built without the store `session.put_file` needs.
    NotSupported,
    // -- file transfer -----------------------------------------------------
    /// One file is over `MAX_PUT_FILE_BYTES`. Distinct from
    /// [`StoreFull`](Self::StoreFull) because the two say different
    /// things to a user: this one is "over 10 MiB".
    TooLarge,
    /// The host's file store has no room for the next file. Nothing was
    /// deleted to make room — a path already handed out stays valid —
    /// so the way out is restarting the session.
    StoreFull,
    // -- data-plane stream faults ----------------------------------------
    /// A gap or duplicate `seq`, a lagged tee, a blown attach budget.
    /// Re-attach.
    Desync,
    /// The client is not reading fast enough.
    Overflow,
    /// The client sent something the framing forbids.
    ProtocolError,
    // -- UI socket only ---------------------------------------------------
    /// A **UI** socket could not reach the host session an op named: the
    /// connection is gone, died mid-op, or its queue is full — and also
    /// what a session's own refusal folds onto when its code is one a UI
    /// socket does not speak (`shutting-down` above is the standing
    /// example; see `docs/reference/ipc.md`'s reorder matrix). The
    /// mirror of `shutting-down`: a session never sends this one.
    HostUnavailable,
    /// A local-backend switch is in flight. Poll `identify` until
    /// `local_backend_switch` is absent, then re-read ids.
    Busy,
    // -- size ownership --------------------------------------------------
    /// A background `tab.resize` met a tab another client sizes. Not a
    /// retry: the tab is resized again only once it has no owner.
    Owned,
    // -- generic ----------------------------------------------------------
    /// A test seam called without `ROOST_TEST_MODE=1` at launch.
    NotEnabled,
    UnknownOp,
    UnknownField,
    MissingParam,
    DuplicateId,
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
            codes::SHUTTING_DOWN => ServerCode::ShuttingDown,
            codes::REPLAY_EXPIRED => ServerCode::ReplayExpired,
            codes::REVISION_AHEAD => ServerCode::RevisionAhead,
            codes::SESSION_MISMATCH => ServerCode::SessionMismatch,
            codes::BUILD_MISMATCH => ServerCode::BuildMismatch,
            codes::UNSUPPORTED_KIND => ServerCode::UnsupportedKind,
            codes::TOO_MANY_ATTACHES => ServerCode::TooManyAttaches,
            codes::NOT_FOUND => ServerCode::NotFound,
            codes::INVALID_PARAM => ServerCode::InvalidParam,
            codes::PROTOCOL_MISMATCH => ServerCode::ProtocolMismatch,
            codes::SNAPSHOT_FAILED => ServerCode::SnapshotFailed,
            codes::NOT_SUPPORTED => ServerCode::NotSupported,
            codes::TOO_LARGE => ServerCode::TooLarge,
            codes::STORE_FULL => ServerCode::StoreFull,
            codes::DESYNC => ServerCode::Desync,
            codes::OVERFLOW => ServerCode::Overflow,
            codes::PROTOCOL_ERROR => ServerCode::ProtocolError,
            codes::HOST_UNAVAILABLE => ServerCode::HostUnavailable,
            codes::BUSY => ServerCode::Busy,
            codes::OWNED => ServerCode::Owned,
            codes::NOT_ENABLED => ServerCode::NotEnabled,
            codes::UNKNOWN_OP => ServerCode::UnknownOp,
            codes::UNKNOWN_FIELD => ServerCode::UnknownField,
            codes::MISSING_PARAM => ServerCode::MissingParam,
            codes::DUPLICATE_ID => ServerCode::DuplicateId,
            codes::NOT_IMPLEMENTED => ServerCode::NotImplemented,
            codes::PARSE_ERROR => ServerCode::ParseError,
            codes::FRAME_TOO_LARGE => ServerCode::FrameTooLarge,
            codes::INTERNAL => ServerCode::Internal,
            other => ServerCode::Other(other.to_string()),
        }
    }

    /// The wire spelling, round-tripping [`Self::from_wire`].
    pub fn as_str(&self) -> &str {
        match self {
            ServerCode::ShuttingDown => codes::SHUTTING_DOWN,
            ServerCode::ReplayExpired => codes::REPLAY_EXPIRED,
            ServerCode::RevisionAhead => codes::REVISION_AHEAD,
            ServerCode::SessionMismatch => codes::SESSION_MISMATCH,
            ServerCode::BuildMismatch => codes::BUILD_MISMATCH,
            ServerCode::UnsupportedKind => codes::UNSUPPORTED_KIND,
            ServerCode::TooManyAttaches => codes::TOO_MANY_ATTACHES,
            ServerCode::NotFound => codes::NOT_FOUND,
            ServerCode::InvalidParam => codes::INVALID_PARAM,
            ServerCode::ProtocolMismatch => codes::PROTOCOL_MISMATCH,
            ServerCode::SnapshotFailed => codes::SNAPSHOT_FAILED,
            ServerCode::NotSupported => codes::NOT_SUPPORTED,
            ServerCode::TooLarge => codes::TOO_LARGE,
            ServerCode::StoreFull => codes::STORE_FULL,
            ServerCode::Desync => codes::DESYNC,
            ServerCode::Overflow => codes::OVERFLOW,
            ServerCode::ProtocolError => codes::PROTOCOL_ERROR,
            ServerCode::HostUnavailable => codes::HOST_UNAVAILABLE,
            ServerCode::Busy => codes::BUSY,
            ServerCode::Owned => codes::OWNED,
            ServerCode::NotEnabled => codes::NOT_ENABLED,
            ServerCode::UnknownOp => codes::UNKNOWN_OP,
            ServerCode::UnknownField => codes::UNKNOWN_FIELD,
            ServerCode::MissingParam => codes::MISSING_PARAM,
            ServerCode::DuplicateId => codes::DUPLICATE_ID,
            ServerCode::NotImplemented => codes::NOT_IMPLEMENTED,
            ServerCode::ParseError => codes::PARSE_ERROR,
            ServerCode::FrameTooLarge => codes::FRAME_TOO_LARGE,
            ServerCode::Internal => codes::INTERNAL,
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
    fn every_code_has_a_variant_and_round_trips() {
        for &code in codes::ALL {
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
            payload: br#"{"code":"desync","message":"a gap in the seq"}"#.to_vec(),
        };
        match ServerFrame::decode(frame).unwrap() {
            ServerFrame::Error(error) => {
                assert_eq!(ServerCode::from(&error), ServerCode::Desync)
            }
            other => panic!("expected an ERROR frame, got {other:?}"),
        }
    }

    #[test]
    fn a_grid_frame_decodes_its_owner_or_its_absence() {
        let owned = DataFrame {
            frame_type: FRAME_GRID,
            payload: br#"{"seq":9,"cols":100,"rows":30,"owner":{"client_id":"a1","label":"desk"}}"#
                .to_vec(),
        };
        match ServerFrame::decode(owned).unwrap() {
            ServerFrame::Grid(grid) => {
                assert_eq!((grid.seq, grid.cols, grid.rows), (9, 100, 30));
                let owner = grid.owner.expect("an owner");
                assert_eq!(
                    (owner.client_id.as_str(), owner.label.as_str()),
                    ("a1", "desk")
                );
            }
            other => panic!("expected a GRID frame, got {other:?}"),
        }
        let unowned = DataFrame {
            frame_type: FRAME_GRID,
            payload: br#"{"seq":0,"cols":80,"rows":24,"owner":null}"#.to_vec(),
        };
        assert!(matches!(
            ServerFrame::decode(unowned).unwrap(),
            ServerFrame::Grid(GridFrame { owner: None, .. })
        ));
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

    fn test_handshake() -> crate::messages::AttachHandshake {
        crate::messages::AttachHandshake::snapshot(
            7,
            crate::messages::AttachHandshakeTerms {
                session_id: "s".into(),
                kinds: Vec::new(),
                cols: 80,
                rows: 24,
                cell_w_px: 0,
                cell_h_px: 0,
                libghostty_build: "gb".into(),
                take: true,
                client_id: "c1".into(),
            },
        )
    }

    /// The frames that follow a handshake line, read to EOF on the
    /// server's side.
    async fn frames_after_the_line(
        lines: FrameReader<tokio::net::unix::OwnedReadHalf>,
    ) -> Vec<DataFrame> {
        let (read, residue) = lines.into_parts();
        let mut frames = DataFrameReader::new(read, residue);
        let mut read_back = Vec::new();
        while let Some(frame) = frames.next_frame().await.expect("read") {
            read_back.push(frame);
        }
        read_back
    }

    /// A detach while the handshake waits for its reply goes out as
    /// `DETACH` right behind the handshake line, and nothing waits for
    /// the reply after it.
    #[tokio::test]
    async fn a_detach_before_the_reply_follows_the_handshake_line() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let (read, write) = client.into_split();
        let (detach_tx, detach_rx) = tokio::sync::oneshot::channel::<()>();
        let attempt = tokio::spawn(async move {
            let detach = async {
                let _ = detach_rx.await;
            };
            DataConnection::handshake_unless(read, write, &test_handshake(), detach, |_| {}).await
        });

        let (server_read, _server_write) = server.into_split();
        let mut lines = FrameReader::new(server_read);
        let line = lines.read_line().await.expect("read").expect("a line");
        let sent: serde_json::Value = serde_json::from_slice(&line).expect("json");
        assert_eq!(sent["attach"], "7");
        detach_tx.send(()).expect("the handshake is waiting");
        assert!(
            attempt.await.expect("join").expect("no error").is_none(),
            "a detached handshake answers None"
        );
        let frames = frames_after_the_line(lines).await;
        assert_eq!(
            frames.iter().map(|f| f.frame_type).collect::<Vec<_>>(),
            [FRAME_DETACH],
            "DETACH and nothing after it"
        );
        assert!(frames[0].payload.is_empty());
    }

    /// The accepted line is heard the moment it is parsed, and a detach
    /// that comes while the preamble behind it is late still goes out as
    /// `DETACH`: the server committed the attach when it wrote that line,
    /// so a bare close would read to it as a disconnect.
    #[tokio::test]
    async fn a_detach_while_the_preamble_is_late_still_sends_detach() {
        use crate::messages::{
            AttachAccepted, AttachHandshakeReply, AttachMode, AttachPayloadKind,
        };

        let (client, server) = UnixStream::pair().expect("socket pair");
        let (read, write) = client.into_split();
        let (detach_tx, detach_rx) = tokio::sync::oneshot::channel::<()>();
        let (heard_tx, heard_rx) = tokio::sync::oneshot::channel::<u64>();
        let attempt = tokio::spawn(async move {
            let detach = async {
                let _ = detach_rx.await;
            };
            DataConnection::handshake_unless(read, write, &test_handshake(), detach, |reply| {
                let _ = heard_tx.send(reply.seq);
            })
            .await
        });

        let (server_read, mut server_write) = server.into_split();
        let mut lines = FrameReader::new(server_read);
        lines.read_line().await.expect("read").expect("a line");
        let reply = AttachHandshakeReply::Accepted(AttachAccepted {
            kind: AttachPayloadKind::from("ghostty-snapshot"),
            mode: AttachMode::Snapshot,
            seq: 41,
            server_epoch: 1,
            tab_generation: 1,
            snapshot_cols: 80,
            snapshot_rows: 24,
            owner: None,
        });
        let body = serde_json::to_vec(&reply).expect("encode the reply");
        crate::framing::write_frame(&mut server_write, &body)
            .await
            .expect("reply");
        assert_eq!(
            heard_rx
                .await
                .expect("the accepted reply was heard before any preamble"),
            41
        );
        detach_tx
            .send(())
            .expect("the handshake is waiting on the preamble");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
                .await
                .expect("the detach ended the wait for the preamble")
                .expect("join")
                .expect("no error")
                .is_none(),
            "a detached handshake answers None"
        );
        let frames = frames_after_the_line(lines).await;
        assert_eq!(
            frames.iter().map(|f| f.frame_type).collect::<Vec<_>>(),
            [FRAME_DETACH]
        );
    }

    /// Client→server types have no business arriving from the server,
    /// and neither does a byte nobody defined.
    #[test]
    fn a_type_the_server_may_not_send_is_named_in_the_error() {
        for frame_type in [FRAME_INPUT, FRAME_RESIZE, FRAME_TAKE, FRAME_DETACH, 0x7E] {
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
