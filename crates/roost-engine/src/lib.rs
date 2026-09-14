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

/// Where a project with no directory of its own is placed.
///
/// An unset `HOME` is not the only way to have no home: an **empty** one
/// (`HOME=`) is what `env::var` hands back as `Ok("")`, and a relative
/// one resolves against a cwd the caller does not control — a
/// `roost-session` daemon has already `chdir("/")`d by the time it asks.
/// Both would otherwise reach a PTY as "spawn wherever you happen to
/// be", so both fall back to `/` alongside the unset case.
///
/// Every place that seeds or resolves a project cwd goes through here,
/// because `project.create` and `tab.open` resolving it differently is
/// exactly the disagreement plan 063 §D4 set out to remove.
pub fn home_dir() -> String {
    resolve_home(std::env::var("HOME").ok().as_deref())
}

/// [`home_dir`] without the environment, so the rule above can be tested
/// without a process-global `HOME` two parallel tests would race over.
fn resolve_home(raw: Option<&str>) -> String {
    match raw {
        Some(home) if std::path::Path::new(home).is_absolute() => home.to_string(),
        _ => "/".into(),
    }
}

pub use application::LocalClient;
#[cfg(feature = "facade")]
pub use facade::{
    CommandResult, Engine, EngineCommand, EngineError, EngineEvent, EngineEventStream,
    EngineSnapshot,
};
pub use pty::{Geometry, PtyError, PtyOutputEvent, PtySupervisor, ShutdownReport, SupervisorEvent};
#[cfg(feature = "server-vt")]
pub use tab_task::{
    ResumeAt, ServerVtConfig, ServerVtWorkspace, SnapshotAt, TabCmd, TabError,
    MAX_CONCURRENT_SNAPSHOTS, REPLAY_RING_BYTES, REPLY_PENDING_MAX, SERVER_VT_CONTINUATION_MAX,
    SERVER_VT_SCROLLBACK, TAB_CHANNEL_CHUNKS, TAB_CMD_CAPACITY,
};
pub use workspace::{
    AttentionSource, ReplayBounds, RestoreLayout, RestoreProject, RestoreTab, ResumeCut,
    ResumeError, TabEffectKind, VersionedWorkspaceEvent, Workspace, WorkspaceError, WorkspaceEvent,
    SIDEBAR_DEFAULT_WIDTH, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
};

#[cfg(test)]
mod home_tests {
    use super::resolve_home;

    #[test]
    fn only_an_absolute_home_is_somewhere_to_put_a_project() {
        assert_eq!(resolve_home(Some("/home/x")), "/home/x");
        assert_eq!(resolve_home(Some("/")), "/");
        // `HOME=` reaches us as `Ok("")`, not as "unset" — the case the
        // old `unwrap_or_else(|_| ..)` spelling silently let through.
        assert_eq!(resolve_home(Some("")), "/");
        // Relative resolves against a cwd the caller does not control;
        // a daemon has already `chdir("/")`d by the time it asks.
        assert_eq!(resolve_home(Some("home/x")), "/");
        assert_eq!(resolve_home(Some("./x")), "/");
        assert_eq!(resolve_home(None), "/");
    }
}
