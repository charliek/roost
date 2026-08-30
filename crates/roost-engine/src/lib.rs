//! Toolkit-neutral application engine shared by Roost front ends.
//!
//! The engine owns authoritative workspace transitions, persistence,
//! PTY supervision, ordered events, full-state resynchronization, and
//! target-neutral IPC dispatch. UI adapters own rendering, native input,
//! clipboard and notification integration, and event-loop marshalling.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod application;
// The attach data plane: one forwarder per admitted data connection,
// gated with the tab task it streams from (plan 036 D6).
#[cfg(feature = "server-vt")]
pub mod attach;
pub mod crash;
pub mod event_push;
pub mod events;
// Experimental Swift-facing boundary (`Engine`/`EngineCommand`/owned
// snapshots). No production consumer yet — both Rust UIs use the concrete
// `Workspace`/`LocalClient` APIs — so it stays feature-gated until a UI
// adopts it and proves the seam (roadmap M5).
#[cfg(feature = "facade")]
pub mod facade;
pub mod git_metrics;
pub mod ipc;
pub mod osc;
pub mod persistence;
pub mod pointer;
pub mod process;
pub mod pty;
pub mod reconcile;
pub mod session;
pub mod single_instance;
// Host sessions: the per-tab authoritative server Terminal and its task.
// Feature-gated for AVAILABILITY; the pipeline itself is a runtime
// opt-in (`PtySupervisor::enable_server_vt`) so a feature-unified UI
// build keeps the default reader → broadcast flow (plan 036 D1).
#[cfg(feature = "server-vt")]
pub mod tab_task;
pub mod workspace;

pub use application::LocalClient;
#[cfg(feature = "facade")]
pub use facade::{
    CommandResult, Engine, EngineCommand, EngineError, EngineEvent, EngineEventStream,
    EngineSnapshot,
};
pub use pty::{PtyError, PtyOutputEvent, PtySupervisor, ShutdownReport, SupervisorEvent};
#[cfg(feature = "server-vt")]
pub use tab_task::{
    ResumeAt, ServerVtConfig, ServerVtWorkspace, SnapshotAt, TabCmd, TabError,
    MAX_CONCURRENT_SNAPSHOTS, REPLAY_RING_BYTES, REPLY_PENDING_MAX, SERVER_VT_CONTINUATION_MAX,
    SERVER_VT_SCROLLBACK, TAB_CHANNEL_CHUNKS, TAB_CMD_CAPACITY,
};
pub use workspace::{
    AttentionSource, RestoreLayout, RestoreProject, RestoreTab, TabEffectKind,
    VersionedWorkspaceEvent, Workspace, WorkspaceError, WorkspaceEvent, SIDEBAR_DEFAULT_WIDTH,
    SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
};
