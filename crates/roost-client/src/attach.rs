//! One tab's data plane, transport-agnostic and ffi-free: the handshake,
//! frame validation, seq contiguity, resume bookkeeping and re-dial.
//! The terminal itself belongs to the consumer, which receives
//! [`AttachEvent`]s in wire order.

use roost_ipc::messages::{AttachMode, SizeOwner};

/// What to attach to, at what size, and whether the attach takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachRequest {
    pub tab_id: i64,
    pub cols: u16,
    pub rows: u16,
    pub cell_w_px: u16,
    pub cell_h_px: u16,
    pub take: bool,
}

/// The payload kind an accepted attach settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    GhosttySnapshot,
    Vt,
}

/// Delivered in wire order on one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    /// A new stream. [`AttachMode::Snapshot`]: build a fresh terminal at
    /// this geometry and feed it the [`AttachEvent::Snap`] bytes that
    /// follow. [`AttachMode::Resume`]: keep the terminal; only `Pty`
    /// follows.
    Accepted {
        kind: PayloadKind,
        mode: AttachMode,
        cols: u16,
        rows: u16,
        cell_w_px: u16,
        cell_h_px: u16,
        owner: Option<SizeOwner>,
    },
    /// Snapshot bytes (GHOSTSNP stream or `vt` replay). An empty `vt`
    /// frame is that payload's terminator.
    Snap(Vec<u8>),
    /// Live output; contiguity is already checked.
    Pty(Vec<u8>),
    /// The tab's grid, cell size or owner changed at this point in the
    /// stream.
    Grid {
        cols: u16,
        rows: u16,
        cell_w_px: u16,
        cell_h_px: u16,
        owner: Option<SizeOwner>,
    },
    /// The tab's program exited. Terminal.
    Exit { code: i32 },
    /// The stream dropped and is being re-dialed.
    Reconnecting { reason: String },
    /// Given up: the tab is gone, or the caller detached. Terminal.
    Ended { reason: String },
}
