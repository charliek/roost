//! Compatibility exports for the toolkit-neutral application engine.
//!
//! New UI-independent code should depend on `roost-engine` directly. These
//! exports preserve the GTK crate's established internal and test paths while
//! the presentation adapter is migrated in small, reviewable steps.

pub use roost_engine::persistence::{persist_state, read_state, SnapshotFile};
pub use roost_engine::{
    AttentionSource, PtyError, PtyOutputEvent, PtySupervisor, RestoreLayout, RestoreProject,
    RestoreTab, SupervisorEvent, Workspace, WorkspaceError, WorkspaceEvent,
};

/// Compatibility namespace for callers that previously named `daemon::state`.
pub mod state {
    pub use roost_engine::workspace::*;
}

/// Compatibility namespace for callers that previously named `daemon::store_json`.
pub mod store_json {
    pub use roost_engine::persistence::*;
}

/// Compatibility namespace for callers that previously named `daemon::pty`.
pub mod pty {
    pub use roost_engine::pty::*;
}
