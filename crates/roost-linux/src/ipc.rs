//! JSON IPC handler for the Linux UI.
//!
//! M3a of the daemon-removal refactor — adds the handler now so
//! M3b can wire it into the gtk4-rs application. The handler
//! consumes a shared [`daemon::Workspace`] + [`daemon::PtySupervisor`]
//! and dispatches each request from the [`roost_ipc::IpcServer`]
//! against them.
//!
//! Threading: the handler trait is `Send + Sync`. tokio drives the
//! accept + read loops on worker threads; the handler itself
//! mutates the workspace via its own internal `Mutex`, so there's
//! no need for the UI's glib main loop to be involved. The actual
//! UI updates flow through `Workspace::subscribe` — `app.rs`
//! (M3b) installs a `glib::MainContext::channel` and listens
//! there.

use std::path::PathBuf;
use std::sync::Arc;

use roost_ipc::messages::{
    ops, IdentifyParams, IdentifyResult, NotificationCreateParams, ProjectCreateParams,
    ProjectCreateResult, ProjectDeleteParams, ProjectRenameParams, ProjectReorderParams,
    TabClearNotificationParams, TabCloseParams, TabFocusParams, TabFocusResult, TabListResult,
    TabOpenParams, TabOpenResult, TabReorderParams, TabResizeParams, TabSetHookActiveParams,
    TabSetStateParams, TabSetTitleParams, TabWriteParams,
};
use roost_ipc::{Handler, HandlerError};

use crate::daemon::state::WorkspaceError;
use crate::daemon::{PtySupervisor, Workspace};

/// Glue between the JSON IPC server and the in-process workspace +
/// PTY supervisor.
pub struct IpcHandler {
    pub workspace: Arc<Workspace>,
    pub supervisor: Arc<PtySupervisor>,
    /// Absolute path to the IPC socket. Echoed in `identify` and
    /// injected as `ROOST_SOCKET` into spawned shells.
    pub socket_path: PathBuf,
    /// App label / app id pair from the active bundle profile.
    pub app_label: String,
    pub app_id: String,
}

impl IpcHandler {
    pub fn new(
        workspace: Arc<Workspace>,
        supervisor: Arc<PtySupervisor>,
        socket_path: PathBuf,
        app_label: impl Into<String>,
        app_id: impl Into<String>,
    ) -> Self {
        Self {
            workspace,
            supervisor,
            socket_path,
            app_label: app_label.into(),
            app_id: app_id.into(),
        }
    }
}

impl Handler for IpcHandler {
    fn handle<'a>(
        &'a self,
        op: &'a str,
        params: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, HandlerError>> + Send + 'a>,
    > {
        Box::pin(async move { dispatch(self, op, params).await })
    }
}

async fn dispatch(
    h: &IpcHandler,
    op: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, HandlerError> {
    match op {
        ops::IDENTIFY => {
            let _p: IdentifyParams = decode(params)?;
            let (active_project_id, active_tab_id) = h.workspace.active();
            let result = IdentifyResult {
                socket_path: h.socket_path.to_string_lossy().into(),
                pid: std::process::id() as i32,
                active_project_id,
                active_tab_id,
                app_label: h.app_label.clone(),
                app_id: h.app_id.clone(),
                ui_version: env!("CARGO_PKG_VERSION").into(),
                protocol_version: roost_ipc::PROTOCOL_VERSION,
            };
            encode(&result)
        }
        ops::TAB_OPEN => {
            let p: TabOpenParams = decode(params)?;
            let project_id = if p.project_id == 0 {
                h.workspace.ensure_default_project(&p.cwd)
            } else {
                p.project_id
            };
            let tab = h
                .workspace
                .open_tab(project_id, &p.cwd, &p.title)
                .map_err(ws_err)?;
            // Spawn the PTY. Use the tab's cwd, the requested argv,
            // and a sensible default winsize when the caller doesn't
            // provide one.
            let cols = if p.cols == 0 { 80 } else { p.cols as u16 };
            let rows = if p.rows == 0 { 24 } else { p.rows as u16 };
            if let Err(err) =
                h.supervisor
                    .spawn(tab.id, &tab.cwd, &p.argv, cols, rows, &h.socket_path)
            {
                // PTY spawn failed — roll back the tab so the
                // workspace doesn't carry a phantom.
                let _ = h.workspace.close_tab(tab.id);
                // A `Cancelled` here means the user (or another
                // caller) closed the same tab id between our
                // workspace insert and the supervisor's promote.
                // Surface that as `not-found` so the client sees
                // the same code as any other "tab gone" path
                // rather than misclassifying it as a server fault.
                if let Some(crate::daemon::PtyError::Cancelled(_)) =
                    err.downcast_ref::<crate::daemon::PtyError>()
                {
                    return Err(HandlerError::not_found(err.to_string()));
                }
                return Err(HandlerError::new(
                    "internal",
                    format!("pty spawn failed: {err}"),
                ));
            }
            encode(&TabOpenResult { tab })
        }
        ops::TAB_CLOSE => {
            let p: TabCloseParams = decode(params)?;
            h.supervisor.close(p.tab_id);
            h.workspace.close_tab(p.tab_id).map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_LIST => {
            let result = TabListResult {
                projects: h.workspace.snapshot(),
            };
            encode(&result)
        }
        ops::TAB_WRITE => {
            let p: TabWriteParams = decode(params)?;
            h.supervisor
                .write(p.tab_id, p.data)
                .await
                .map_err(pty_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_RESIZE => {
            let p: TabResizeParams = decode(params)?;
            h.supervisor
                .resize(p.tab_id, p.cols as u16, p.rows as u16)
                .await
                .map_err(pty_err)?;
            Ok(serde_json::json!({}))
        }
        ops::PROJECT_CREATE => {
            let p: ProjectCreateParams = decode(params)?;
            let project = h
                .workspace
                .create_project(&p.name, &p.cwd)
                .map_err(ws_err)?;
            encode(&ProjectCreateResult { project })
        }
        ops::PROJECT_RENAME => {
            let p: ProjectRenameParams = decode(params)?;
            h.workspace
                .rename_project(p.project_id, &p.name)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::PROJECT_DELETE => {
            let p: ProjectDeleteParams = decode(params)?;
            let cascaded = h.workspace.delete_project(p.project_id).map_err(ws_err)?;
            for tab_id in cascaded {
                h.supervisor.close(tab_id);
            }
            Ok(serde_json::json!({}))
        }
        ops::TAB_REORDER => {
            let p: TabReorderParams = decode(params)?;
            h.workspace
                .reorder_tabs(p.project_id, &p.tab_ids)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::PROJECT_REORDER => {
            let p: ProjectReorderParams = decode(params)?;
            h.workspace
                .reorder_projects(&p.project_ids)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_FOCUS => {
            let p: TabFocusParams = decode(params)?;
            let (previous_project_id, previous_tab_id) =
                h.workspace.focus_tab(p.tab_id).map_err(ws_err)?;
            encode(&TabFocusResult {
                previous_project_id,
                previous_tab_id,
            })
        }
        ops::TAB_SET_TITLE => {
            let p: TabSetTitleParams = decode(params)?;
            h.workspace
                .set_tab_title(p.tab_id, &p.title)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_SET_STATE => {
            let p: TabSetStateParams = decode(params)?;
            h.workspace
                .set_tab_state(p.tab_id, p.state)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_CLEAR_NOTIFICATION => {
            let p: TabClearNotificationParams = decode(params)?;
            h.workspace
                .set_tab_has_notification(p.tab_id, false)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_SET_HOOK_ACTIVE => {
            let p: TabSetHookActiveParams = decode(params)?;
            h.workspace
                .set_tab_hook_active(p.tab_id, p.active)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::NOTIFICATION_CREATE => {
            let p: NotificationCreateParams = decode(params)?;
            // Mark the tab as having a pending notification; emit
            // the lifecycle event for any subscriber. The actual
            // OS-level notification (libnotify / NSUserNotification)
            // is the UI layer's job in M3b.
            h.workspace
                .set_tab_has_notification(p.tab_id, true)
                .map_err(ws_err)?;
            h.workspace
                .fire_notification(p.tab_id, &p.title, &p.body)
                .map_err(ws_err)?;
            Ok(serde_json::json!({}))
        }
        ops::EVENTS_SUBSCRIBE => {
            // M0/M3a stub: reply OK without ever pushing events on
            // the connection. The full subscribe wiring lands when
            // a CLI consumer needs events (post-M5 at the earliest).
            Ok(serde_json::json!({}))
        }
        other => Err(HandlerError::unknown_op(other)),
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, HandlerError> {
    serde_json::from_value(value).map_err(|e| {
        // Drop the field key out of the error message for users;
        // `serde_json::Error::Display` already includes a useful
        // "missing field `foo` at line ..." form.
        let msg = e.to_string();
        if msg.contains("unknown field") {
            HandlerError::new("unknown-field", msg)
        } else if msg.contains("missing field") {
            HandlerError::new("missing-param", msg)
        } else {
            HandlerError::invalid_param(msg)
        }
    })
}

fn encode<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, HandlerError> {
    serde_json::to_value(value).map_err(|e| HandlerError::new("internal", e.to_string()))
}

fn ws_err(e: WorkspaceError) -> HandlerError {
    match e {
        WorkspaceError::ProjectNotFound(_) | WorkspaceError::TabNotFound(_) => {
            HandlerError::not_found(e.to_string())
        }
        WorkspaceError::TabProjectMismatch { .. } => HandlerError::invalid_param(e.to_string()),
        WorkspaceError::Io(_) | WorkspaceError::Json(_) => {
            HandlerError::new("internal", e.to_string())
        }
    }
}

fn pty_err(e: crate::daemon::PtyError) -> HandlerError {
    match e {
        crate::daemon::PtyError::NotFound(_)
        | crate::daemon::PtyError::Closed(_)
        | crate::daemon::PtyError::Cancelled(_) => HandlerError::not_found(e.to_string()),
        crate::daemon::PtyError::DuplicateTab(_) => HandlerError::invalid_param(e.to_string()),
    }
}
