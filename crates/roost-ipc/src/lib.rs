//! JSON-over-Unix-socket IPC for Roost.
//!
//! This crate is the wire-format home of the post-daemon-removal IPC
//! that `roostctl` (Rust) and Roost's native UIs (Swift on Mac, iced
//! on Linux) speak. The full protocol spec lives at
//! [`docs/reference/ipc.md`]; this crate is the Rust implementation
//! of it.
//!
//! Modules:
//! * [`messages`] — serde structs for every operation, response, and
//!   event listed in the spec, plus shared types (`Tab`, `Project`,
//!   `TabState`).
//! * [`agent`] — the agent state model (shell / lifecycle / ownership
//!   axes) and the pure state machine that derives `TabState` from it.
//!   Shared with the Swift port via `tests/agent-state-fixtures/`.
//! * [`framing`] — newline-delimited JSON read/write over a
//!   `tokio::net::UnixStream`. Enforces the 16 MiB line limit.
//! * [`dataframe`] — the binary framing a host-session attach
//!   connection switches to after its JSON handshake.
//! * [`paths`] — `BundleProfile` path resolution. The Mac UI's Swift
//!   side has a byte-for-byte equivalent.
//! * [`socket_state`] — the one shared answer to "is a listener alive
//!   at this socket path?", used by the bind path and `roostctl
//!   doctor` and mirrored in Swift.
//! * [`client`] — the three client transports: `IpcClient` (typed
//!   wrappers around the framed request/response cycle), `EventStream`
//!   (subscribe-then-stream), and `DataConnection` (the attach data
//!   plane), plus the `ServerCode` mapping of every refusal they can
//!   hand back.
//! * [`server`] — `IpcServer` + `Handler` trait. The UI implements
//!   `Handler`; the server crate drives the accept loop.
//! * [`peer`] — peer-credential lookup ([`peer_uid`]) behind the
//!   server's opt-in same-UID enforcement.
//! * [`runtime_dir`] — create-or-validate the directory a socket is
//!   bound in ([`validate_runtime_dir`]).
//! * [`target`] — CLI-side target selection for `roostctl`.
//! * [`session_launch`] — the launch-cwd hint and the readiness verdict
//!   line `roostctl session start` and `roost-session` exchange.
//! * [`ssh`] — the SSH transport (host-sessions HS-3): classification of
//!   a saved host's `target` string, the generated `ssh_config`, the
//!   argv builders, `sun_path` sizing, connection-failure
//!   classification, and the [`ssh::SshTunnel`] runtime those add up to
//!   — a local bridge socket whose every connection gets an `ssh` exec
//!   over one shared mux.
//!
//! The Swift companion lives in `mac/Sources/Roost/IPCServer.swift`
//! (post-M4). Golden cross-language vectors live under
//! `tests/ipc-vectors/*.json` at the workspace root.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod agent;
pub mod dataframe;
pub mod framing;
pub mod messages;
pub mod paths;
pub mod session_launch;
pub mod socket_state;
pub mod ssh;
pub mod target;

pub mod client;

mod peer;
mod runtime_dir;
mod server;

pub use client::{ClientError, IpcClient};
pub use peer::{current_euid, peer_uid};
pub use runtime_dir::validate_runtime_dir;
pub use server::{
    CloseReason, ConnAction, ConnCloseWatch, ConnCloser, ConnCtx, DataConn, Handler, HandlerError,
    HandlerOutcome, IpcServer, PushSource, StopFinalizer, CLOSE_LABEL_DEADLINE,
    DEFAULT_PUSH_WRITE_DEADLINE,
};

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
    #[error("payload contains a literal newline (use a non-newline encoding)")]
    EmbeddedNewline,
    #[error(
        "data frame larger than {} bytes",
        crate::dataframe::MAX_DATA_FRAME_BYTES
    )]
    DataFrameTooLarge,
    #[error("data connection did not open with the expected preamble")]
    BadPreamble,
    /// A data frame the framer accepted but the endpoint's own rules
    /// forbid: a fixed-width payload at the wrong width, or a type byte
    /// this direction may not carry. [`dataframe`] stops at the length
    /// cap on purpose — that one has to precede the allocation — and
    /// this is the other half of the split.
    #[error("data-plane protocol error: {0}")]
    DataProtocol(String),
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
            Error::EmbeddedNewline => "internal",
            Error::DataFrameTooLarge => "frame-too-large",
            // A bad preamble means the peer is not speaking this
            // protocol at all — the same class of failure as an
            // unparseable line.
            Error::BadPreamble => "parse-error",
            // The data plane's own `ERROR` catalogue spells this one.
            Error::DataProtocol(_) => "protocol-error",
        }
    }
}
