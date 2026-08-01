//! Toolkit-neutral pointer DTOs used by IPC and UI input adapters.

/// Pointer action accepted by a terminal mouse encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerAction {
    Press,
    Release,
    Motion,
}

/// Logical terminal mouse button. Wheel up/down map to buttons four/five.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Four,
    Five,
}
