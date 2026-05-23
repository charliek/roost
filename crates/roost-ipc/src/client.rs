//! `IpcClient` — sequential request/response client over a
//! Unix-domain stream socket.
//!
//! The CLI (`roostctl`) is the primary consumer; it dispatches a small
//! handful of one-shot RPCs per invocation, so a strictly-sequential
//! client (one in-flight request at a time) is enough. A pipelined
//! variant can land later if a use case shows up.
//!
//! Each request gets a monotonically-increasing id. Responses are
//! matched by id; unsolicited event envelopes mid-stream (the server
//! may emit them after `events.subscribe` lands a real implementation
//! — M0/M3a stub never sends any) are silently dropped by the M2
//! client. Future event-aware clients can extend [`IpcClient`] with
//! a frame-level read helper rather than going through
//! [`IpcClient::call_raw`].

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::net::UnixStream;

use crate::framing::{write_frame, FrameReader};
use crate::messages::{ops, IdentifyParams, IdentifyResult, RawRequest, Response};
use crate::Error;

/// Single-connection sequential client.
pub struct IpcClient {
    reader: FrameReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_id: AtomicI64,
}

impl IpcClient {
    /// Dial the socket at `path`.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, Error> {
        let stream = UnixStream::connect(path.as_ref()).await?;
        let (r, w) = stream.into_split();
        Ok(Self {
            reader: FrameReader::new(r),
            writer: w,
            next_id: AtomicI64::new(1),
        })
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
                let err = resp.error.unwrap_or(crate::messages::ResponseError {
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
        serde_json::from_value(raw).map_err(|e| ClientError::Io(Error::from(e)))
    }

    /// Convenience: send an `identify` request and decode the result.
    pub async fn identify(
        &mut self,
        params: IdentifyParams,
    ) -> Result<IdentifyResult, ClientError> {
        self.call(ops::IDENTIFY, params).await
    }
}

/// Client-side errors. Distinct from [`crate::Error`] because the
/// server-error case is meaningful here.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Io(#[from] Error),
    #[error("server returned error: {code} — {message}")]
    Server { code: String, message: String },
    #[error("response id mismatch: expected {expected}, got {got}")]
    IdMismatch { expected: i64, got: i64 },
    #[error("connection closed before response")]
    Disconnected,
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(Error::Io(e))
    }
}
