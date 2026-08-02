//! Explicit command, snapshot, event, and error boundary for UI adapters.
//!
//! The existing concrete [`Workspace`](crate::Workspace) and
//! [`LocalClient`](crate::LocalClient) methods remain available to keep the
//! GTK migration mechanical. New adapters should prefer this façade: every
//! value crossing it is owned, serializable, and independent of a UI toolkit.

use std::path::PathBuf;
use std::sync::Arc;

use roost_ipc::agent::TabAgentReportParams;
use roost_ipc::messages::{
    Project, ProjectCreateParams, ProjectDeleteParams, ProjectRenameParams, ProjectReorderParams,
    Tab, TabClearNotificationParams, TabCloseParams, TabFocusParams, TabOpenParams,
    TabReorderParams, TabResizeParams, TabSetStateParams, TabSetTitleParams, TabWriteParams,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::{LocalClient, PtyError, PtySupervisor, Workspace, WorkspaceError, WorkspaceEvent};

/// Versioned, serializable commands suitable for Rust UI adapters and a future
/// serialized C ABI. Existing `roost-ipc` DTOs are reused deliberately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "params", rename_all = "snake_case")]
pub enum EngineCommand {
    ProjectCreate(ProjectCreateParams),
    ProjectRename(ProjectRenameParams),
    ProjectDelete(ProjectDeleteParams),
    ProjectReorder(ProjectReorderParams),
    TabOpen(TabOpenParams),
    TabClose(TabCloseParams),
    TabFocus(TabFocusParams),
    TabSetTitle(TabSetTitleParams),
    TabSetState(TabSetStateParams),
    TabReorder(TabReorderParams),
    TabClearNotification(TabClearNotificationParams),
    TabAgentReport(TabAgentReportParams),
    TabWrite(TabWriteParams),
    TabResize(TabResizeParams),
    Shutdown,
}

/// Owned result from [`Engine::execute`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
pub enum CommandResult {
    Ack,
    Project(Project),
    Tab(Tab),
    DeletedTabs(#[serde(with = "roost_ipc::messages::vec_string_int64")] Vec<i64>),
    PreviousSelection {
        #[serde(with = "roost_ipc::messages::string_int64")]
        previous_project_id: i64,
        #[serde(with = "roost_ipc::messages::string_int64")]
        previous_tab_id: i64,
    },
    AgentReport {
        accepted: bool,
        tab: Tab,
    },
}

/// Complete UI-recoverable engine state. It owns all data and can replace a
/// consumer's projection after startup, lag, or a revision discontinuity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSnapshot {
    pub schema_version: u32,
    pub revision: u64,
    pub projects: Vec<Project>,
    #[serde(with = "roost_ipc::messages::string_int64")]
    pub active_project_id: i64,
    #[serde(with = "roost_ipc::messages::string_int64")]
    pub active_tab_id: i64,
    pub sidebar_collapsed: bool,
}

impl EngineSnapshot {
    pub const SCHEMA_VERSION: u32 = 1;
}

/// Ordered incremental event or a complete recovery snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EngineEvent {
    Delta {
        schema_version: u32,
        revision: u64,
        event: WorkspaceEvent,
    },
    Resync(EngineSnapshot),
}

/// Stable error categories at the engine boundary. Detailed messages are
/// owned strings; no toolkit or `anyhow` value leaks across the interface.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Pty(#[from] PtyError),
    #[error("engine operation failed: {0}")]
    Operation(String),
    #[error("invalid engine command: {0}")]
    InvalidArgument(String),
}

impl EngineError {
    /// Machine-readable status key for IPC and future FFI adapters.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Workspace(WorkspaceError::ProjectNotFound(_)) => "project_not_found",
            Self::Workspace(WorkspaceError::TabNotFound(_)) => "tab_not_found",
            Self::Workspace(WorkspaceError::TabProjectMismatch { .. }) => "tab_project_mismatch",
            Self::Workspace(WorkspaceError::Io(_)) => "io_error",
            Self::Workspace(WorkspaceError::Json(_)) => "invalid_state",
            Self::Pty(PtyError::NotFound(_)) | Self::Pty(PtyError::Closed(_)) => "tab_not_found",
            Self::Pty(PtyError::Cancelled(_)) => "cancelled",
            Self::Pty(PtyError::DuplicateTab(_)) => "duplicate_tab",
            Self::Operation(_) => "operation_failed",
            Self::InvalidArgument(_) => "invalid_argument",
        }
    }
}

/// Toolkit-neutral application engine. It owns no runtime singleton and never
/// calls a UI. Commands are serialized so command/result ordering is explicit
/// even when multiple IPC or UI tasks invoke the same handle concurrently.
pub struct Engine {
    workspace: Arc<Workspace>,
    supervisor: Arc<PtySupervisor>,
    client: LocalClient,
    command_lock: tokio::sync::Mutex<()>,
}

impl Engine {
    pub fn new(
        workspace: Arc<Workspace>,
        supervisor: Arc<PtySupervisor>,
        socket_path: PathBuf,
    ) -> Self {
        let client = LocalClient::new(workspace.clone(), supervisor.clone(), socket_path);
        Self {
            workspace,
            supervisor,
            client,
            command_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn open(state_path: PathBuf, socket_path: PathBuf) -> Self {
        Self::new(
            Arc::new(Workspace::open(state_path)),
            Arc::new(PtySupervisor::new()),
            socket_path,
        )
    }

    pub fn workspace(&self) -> &Arc<Workspace> {
        &self.workspace
    }

    pub fn supervisor(&self) -> &Arc<PtySupervisor> {
        &self.supervisor
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        let (revision, projects, active_project_id, active_tab_id, sidebar_collapsed) =
            self.workspace.snapshot_parts();
        EngineSnapshot {
            schema_version: EngineSnapshot::SCHEMA_VERSION,
            revision,
            projects,
            active_project_id,
            active_tab_id,
            sidebar_collapsed,
        }
    }

    /// Subscribe before taking the initial snapshot to avoid a startup gap.
    /// The stream itself emits that snapshot first, then ordered deltas.
    pub fn subscribe(self: &Arc<Self>) -> EngineEventStream {
        EngineEventStream {
            engine: self.clone(),
            receiver: self.workspace.subscribe_versioned(),
            initial: true,
            last_revision: 0,
            discard_through: None,
        }
    }

    pub async fn execute(&self, command: EngineCommand) -> Result<CommandResult, EngineError> {
        let _serial = self.command_lock.lock().await;
        match command {
            EngineCommand::ProjectCreate(p) => self
                .workspace
                .create_project(&p.name, &p.cwd)
                .map(CommandResult::Project)
                .map_err(Into::into),
            EngineCommand::ProjectRename(p) => {
                self.workspace.rename_project(p.project_id, &p.name)?;
                Ok(CommandResult::Ack)
            }
            EngineCommand::ProjectDelete(p) => {
                let ids = self.workspace.delete_project(p.project_id)?;
                for id in &ids {
                    self.supervisor.close(*id);
                }
                Ok(CommandResult::DeletedTabs(ids))
            }
            EngineCommand::ProjectReorder(p) => {
                self.workspace.reorder_projects(&p.project_ids)?;
                Ok(CommandResult::Ack)
            }
            EngineCommand::TabOpen(p) => self.open_tab(p).await.map(CommandResult::Tab),
            EngineCommand::TabClose(p) => {
                self.supervisor.close(p.tab_id);
                self.workspace.close_tab(p.tab_id)?;
                Ok(CommandResult::Ack)
            }
            EngineCommand::TabFocus(p) => {
                let (previous_project_id, previous_tab_id) = self.workspace.focus_tab(p.tab_id)?;
                Ok(CommandResult::PreviousSelection {
                    previous_project_id,
                    previous_tab_id,
                })
            }
            EngineCommand::TabSetTitle(p) => {
                self.workspace.set_tab_title(p.tab_id, &p.title)?;
                Ok(CommandResult::Ack)
            }
            EngineCommand::TabSetState(p) => {
                self.workspace.set_tab_state(p.tab_id, p.state)?;
                Ok(CommandResult::Ack)
            }
            EngineCommand::TabReorder(p) => {
                self.workspace.reorder_tabs(p.project_id, &p.tab_ids)?;
                Ok(CommandResult::Ack)
            }
            EngineCommand::TabClearNotification(p) => {
                self.workspace.set_tab_has_notification(p.tab_id, false)?;
                Ok(CommandResult::Ack)
            }
            EngineCommand::TabAgentReport(p) => {
                let (accepted, tab) = self.workspace.agent_report(&p)?;
                Ok(CommandResult::AgentReport { accepted, tab })
            }
            EngineCommand::TabWrite(p) => {
                self.supervisor.write(p.tab_id, p.data).await?;
                Ok(CommandResult::Ack)
            }
            EngineCommand::TabResize(p) => self.resize_tab(p).await.map(|()| CommandResult::Ack),
            EngineCommand::Shutdown => {
                self.workspace.flush();
                for project in self.workspace.snapshot() {
                    for tab in project.tabs {
                        self.supervisor.close(tab.id);
                    }
                }
                Ok(CommandResult::Ack)
            }
        }
    }

    async fn open_tab(&self, mut params: TabOpenParams) -> Result<Tab, EngineError> {
        validate_dimension(params.cols, "cols")?;
        validate_dimension(params.rows, "rows")?;
        if params.project_id == 0 {
            params.project_id = self.workspace.ensure_default_project(&params.cwd);
        }
        self.client
            .open_tab(
                params.project_id,
                &params.cwd,
                &params.title,
                &params.argv,
                params.cols,
                params.rows,
            )
            .await
            .map_err(application_error)
    }

    async fn resize_tab(&self, params: TabResizeParams) -> Result<(), EngineError> {
        validate_dimension(params.cols, "cols")?;
        validate_dimension(params.rows, "rows")?;
        self.client
            .resize_tab(params.tab_id, params.cols, params.rows)
            .await
            .map_err(application_error)
    }
}

fn validate_dimension(value: u32, field: &str) -> Result<(), EngineError> {
    if value != 0 && u16::try_from(value).is_err() {
        return Err(EngineError::InvalidArgument(format!(
            "{field} out of u16 range: {value}"
        )));
    }
    Ok(())
}

fn application_error(error: anyhow::Error) -> EngineError {
    match error.downcast::<WorkspaceError>() {
        Ok(error) => EngineError::Workspace(error),
        Err(error) => match error.downcast::<PtyError>() {
            Ok(error) => EngineError::Pty(error),
            Err(error) => EngineError::Operation(error.to_string()),
        },
    }
}

pub struct EngineEventStream {
    engine: Arc<Engine>,
    receiver: tokio::sync::broadcast::Receiver<crate::VersionedWorkspaceEvent>,
    initial: bool,
    last_revision: u64,
    /// Startup/resync snapshots can be newer than already-buffered events.
    /// Discard those stale deltas before resuming incremental delivery.
    discard_through: Option<u64>,
}

impl EngineEventStream {
    /// Returns `None` only when the workspace event source has closed.
    /// Lag is recoverable and yields a complete replacement snapshot.
    pub async fn recv(&mut self) -> Option<EngineEvent> {
        if std::mem::take(&mut self.initial) {
            let snapshot = self.engine.snapshot();
            self.last_revision = snapshot.revision;
            self.discard_through = Some(snapshot.revision);
            return Some(EngineEvent::Resync(snapshot));
        }
        loop {
            match self.receiver.recv().await {
                Ok(item) => {
                    if self
                        .discard_through
                        .is_some_and(|revision| item.revision <= revision)
                    {
                        continue;
                    }
                    self.discard_through = None;
                    if item.revision < self.last_revision {
                        continue;
                    }
                    if item.revision > self.last_revision.saturating_add(1) {
                        let snapshot = self.engine.snapshot();
                        self.last_revision = snapshot.revision;
                        self.discard_through = Some(snapshot.revision);
                        return Some(EngineEvent::Resync(snapshot));
                    }
                    self.last_revision = item.revision;
                    return Some(EngineEvent::Delta {
                        schema_version: EngineSnapshot::SCHEMA_VERSION,
                        revision: item.revision,
                        event: item.event,
                    });
                }
                Err(RecvError::Lagged(_)) => {
                    let snapshot = self.engine.snapshot();
                    self.last_revision = snapshot.revision;
                    self.discard_through = Some(snapshot.revision);
                    return Some(EngineEvent::Resync(snapshot));
                }
                Err(RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn commands_advance_revision_and_emit_in_order() {
        let engine = Arc::new(Engine::new(
            Arc::new(Workspace::new()),
            Arc::new(PtySupervisor::new()),
            PathBuf::from("/tmp/roost-engine-test.sock"),
        ));
        let mut events = engine.subscribe();
        assert!(matches!(events.recv().await, Some(EngineEvent::Resync(_))));

        let result = engine
            .execute(EngineCommand::ProjectCreate(ProjectCreateParams {
                name: "one".into(),
                cwd: "/tmp".into(),
            }))
            .await
            .unwrap();
        assert!(matches!(result, CommandResult::Project(_)));
        assert_eq!(engine.snapshot().revision, 1);
        assert!(matches!(
            events.recv().await,
            Some(EngineEvent::Delta {
                schema_version: EngineSnapshot::SCHEMA_VERSION,
                revision: 1,
                event: WorkspaceEvent::ProjectCreated(_)
            })
        ));
    }

    #[tokio::test]
    async fn deterministic_error_code_does_not_expose_error_layout() {
        let engine = Engine::new(
            Arc::new(Workspace::new()),
            Arc::new(PtySupervisor::new()),
            PathBuf::from("/tmp/roost-engine-test.sock"),
        );
        let error = engine
            .execute(EngineCommand::TabFocus(TabFocusParams { tab_id: 404 }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "tab_not_found");
        assert_eq!(error.to_string(), "tab 404 not found");

        let missing_project = engine
            .execute(EngineCommand::TabOpen(TabOpenParams {
                project_id: 999,
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(missing_project.code(), "project_not_found");

        let invalid_dimension = engine
            .execute(EngineCommand::TabOpen(TabOpenParams {
                cols: u32::MAX,
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(invalid_dimension.code(), "invalid_argument");
    }

    #[tokio::test]
    async fn slow_consumer_recovers_with_full_resync() {
        let engine = Arc::new(Engine::new(
            Arc::new(Workspace::new()),
            Arc::new(PtySupervisor::new()),
            PathBuf::from("/tmp/roost-engine-test.sock"),
        ));
        let mut events = engine.subscribe();
        assert!(matches!(events.recv().await, Some(EngineEvent::Resync(_))));
        for i in 0..300 {
            engine
                .execute(EngineCommand::ProjectCreate(ProjectCreateParams {
                    name: format!("p{i}"),
                    cwd: "/tmp".into(),
                }))
                .await
                .unwrap();
        }
        match events.recv().await {
            Some(EngineEvent::Resync(snapshot)) => {
                assert_eq!(snapshot.revision, 300);
                assert_eq!(snapshot.projects.len(), 300);
            }
            other => panic!("expected lag resync, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn serialized_boundary_keeps_ipc_identifier_encoding() {
        let snapshot = EngineSnapshot {
            schema_version: EngineSnapshot::SCHEMA_VERSION,
            revision: 7,
            projects: Vec::new(),
            active_project_id: i64::MAX,
            active_tab_id: 42,
            sidebar_collapsed: false,
        };
        let value = serde_json::to_value(EngineEvent::Resync(snapshot)).unwrap();
        assert_eq!(value["data"]["active_project_id"], i64::MAX.to_string());
        assert_eq!(value["data"]["active_tab_id"], "42");

        let value = serde_json::to_value(WorkspaceEvent::TabsReordered {
            project_id: 9,
            tab_ids: vec![10, 11],
        })
        .unwrap();
        assert_eq!(value["data"]["project_id"], "9");
        assert_eq!(value["data"]["tab_ids"], serde_json::json!(["10", "11"]));
    }

    #[tokio::test]
    async fn compound_events_share_revision_and_keep_commit_order() {
        let workspace = Arc::new(Workspace::new());
        let project = workspace.create_project("p", "/tmp").unwrap();
        let first = workspace.open_tab(project.id, "/tmp", "one").unwrap();
        let second = workspace.open_tab(project.id, "/tmp", "two").unwrap();
        let mut events = workspace.subscribe_versioned();

        workspace.delete_project(project.id).unwrap();
        let mut batch = Vec::new();
        for _ in 0..4 {
            batch.push(events.recv().await.unwrap());
        }
        assert!(batch.iter().all(|item| item.revision == 4));
        assert!(matches!(
            batch[0].event,
            WorkspaceEvent::TabClosed { tab_id } if tab_id == first.id
        ));
        assert!(matches!(
            batch[1].event,
            WorkspaceEvent::TabClosed { tab_id } if tab_id == second.id
        ));
        assert!(matches!(
            batch[2].event,
            WorkspaceEvent::ProjectDeleted { project_id } if project_id == project.id
        ));
        assert!(matches!(
            batch[3].event,
            WorkspaceEvent::ActiveChanged {
                project_id: 0,
                tab_id: 0
            }
        ));
    }
}
