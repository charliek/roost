//! In-process daemon — PTY supervision + state + JSON persistence.
//!
//! Copied + adapted from `crates/roost-core/src/{pty,state}.rs` at
//! the daemon-removal M3. The roost-core originals stay in place
//! through M6 to keep the workspace building, and are deleted in
//! M7 along with the rest of the daemon.

pub mod pty;
pub mod state;
pub mod store_json;

pub use pty::{PtyError, PtyOutputEvent, PtySupervisor, SupervisorEvent};
pub use state::{Workspace, WorkspaceError, WorkspaceEvent};
pub use store_json::{persist_state, read_state, SnapshotFile};
