//! roost-core — the Roost IPC daemon.
//!
//! Hosts the project/tab state, supervises one PTY per tab, and exposes
//! the gRPC service defined in `proto/roost.proto`. The UIs (Mac/Swift,
//! Linux/Rust) and the shell-integration CLI connect over a Unix domain
//! socket and consume `roost-proto::v1`.
//!
//! Phase 3 implementation is intentionally minimal:
//!   - In-memory project/tab state (no SQLite yet — lands in Phase 5/6a
//!     when persistence becomes required).
//!   - PTY spawn via `portable-pty`.
//!   - Bidirectional `StreamPty` per attached UI.
//!   - Server-stream `WatchEvents` broadcast.
//!   - Control RPCs implemented for the simplest in-memory path.
//!   - `ReportOsc` records but does not yet route notifications.
//!
//! Subsequent phases extend this without breaking the wire contract.

pub mod pty;
pub mod runtime;
pub mod service;
pub mod state;

pub use runtime::{run, Config};
