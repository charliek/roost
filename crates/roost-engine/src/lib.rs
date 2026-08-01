//! Toolkit-neutral application engine shared by Roost front ends.
//!
//! The engine owns authoritative workspace transitions, persistence,
//! PTY supervision, ordered events, full-state resynchronization, and
//! target-neutral IPC dispatch. UI adapters own rendering, native input,
//! clipboard and notification integration, and event-loop marshalling.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod application;
pub mod events;
pub mod facade;
pub mod ipc;
pub mod persistence;
pub mod pointer;
pub mod pty;
pub mod reconcile;
pub mod session;
pub mod single_instance;
pub mod workspace;

pub use application::LocalClient;
pub use facade::{
    CommandResult, Engine, EngineCommand, EngineError, EngineEvent, EngineEventStream,
    EngineSnapshot,
};
pub use pty::{PtyError, PtyOutputEvent, PtySupervisor, SupervisorEvent};
pub use workspace::{
    AttentionSource, RestoreLayout, RestoreProject, RestoreTab, VersionedWorkspaceEvent, Workspace,
    WorkspaceError, WorkspaceEvent,
};
