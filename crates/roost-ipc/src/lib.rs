//! JSON-over-Unix-socket IPC for Roost.
//!
//! This crate is the wire-format home of the post-daemon-removal IPC
//! that `roostctl` (Rust) and Roost's native UIs (Swift on Mac,
//! gtk4-rs on Linux) speak. The full protocol spec lives at
//! [`docs/reference/ipc.md`]; this crate is the Rust implementation
//! of it.
//!
//! Modules:
//! * [`messages`] — serde structs for every operation, response, and
//!   event listed in the spec, plus shared types (`Tab`, `Project`,
//!   `TabState`).
//! * [`framing`] — newline-delimited JSON read/write over a
//!   `tokio::net::UnixStream`. Enforces the 16 MiB line limit.
//! * [`paths`] — `BundleProfile` path resolution. The Mac UI's Swift
//!   side has a byte-for-byte equivalent.
//! * [`client`] — `IpcClient`: typed wrappers around the framed
//!   request/response cycle, one method per op.
//! * [`server`] — `IpcServer` + `Handler` trait. The UI implements
//!   `Handler`; the server crate drives the accept loop.
//! * [`target`] — CLI-side target selection for `roostctl`.
//!
//! The Swift companion lives in `mac/Sources/Roost/IPCServer.swift`
//! (post-M4). Golden cross-language vectors live under
//! `tests/ipc-vectors/*.json` at the workspace root.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod framing;
pub mod messages;
pub mod paths;
pub mod target;

mod client;
mod server;

pub use client::{ClientError, IpcClient};
pub use server::{Handler, HandlerError, IpcServer};

/// The wire-format protocol version. M0 ships `1`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum length of a single framed line (request, response, or
/// event). 16 MiB is sized to accommodate any realistic `tab.write`
/// payload; larger lines are rejected with [`Error::FrameTooLarge`].
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Stable kebab-case error codes that surface to clients. See
/// [`docs/reference/ipc.md`] for the full catalogue.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("frame larger than {MAX_FRAME_BYTES} bytes")]
    FrameTooLarge,
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected eof")]
    UnexpectedEof,
    #[error("unknown op: {0}")]
    UnknownOp(String),
    #[error("invalid id: {0}")]
    InvalidId(String),
}

impl Error {
    /// Stable kebab-case code surfaced over the wire in the response
    /// envelope's `error.code` field. Every value here MUST appear in
    /// the published catalogue in `docs/reference/ipc.md`; undocumented
    /// codes are collapsed to one of the documented ones rather than
    /// leaked to clients.
    ///
    /// Io / UnexpectedEof → `internal` because a transport-level failure
    /// is almost always going to close the connection too — the code is
    /// only useful for debugging in logs, and `internal` is the catch-
    /// all the spec already documents.
    ///
    /// InvalidId → `invalid-param`. The id is a request parameter (the
    /// envelope's `id` field); `invalid-param` is the documented code
    /// for malformed input.
    pub fn code(&self) -> &'static str {
        match self {
            Error::FrameTooLarge => "frame-too-large",
            Error::Parse(_) => "parse-error",
            Error::Io(_) => "internal",
            Error::UnexpectedEof => "internal",
            Error::UnknownOp(_) => "unknown-op",
            Error::InvalidId(_) => "invalid-param",
        }
    }
}
