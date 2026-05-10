//! gRPC service implementation.
//!
//! Translates between the `roost_proto::v1::Roost` interface and the
//! in-process `Workspace` + `PtySupervisor`. Most RPCs are thin wrappers
//! over `Workspace` mutations; the streaming RPCs (`StreamPty`,
//! `WatchEvents`) wire in the PTY supervisor and the broadcast channel.

use std::path::PathBuf;
use std::pin::Pin;
use std::process;
use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use tokio::sync::broadcast::error::RecvError;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

use roost_proto::v1::pty_client_message::Kind as PtyClientKind;
use roost_proto::v1::pty_server_message::Kind as PtyServerKind;
use roost_proto::v1::roost_server::Roost;
use roost_proto::v1::{
    ClearTabNotificationRequest, ClearTabNotificationResponse, CloseTabRequest, CloseTabResponse,
    CreateNotificationRequest, CreateNotificationResponse, CreateProjectRequest,
    CreateProjectResponse, DeleteProjectRequest, DeleteProjectResponse, Event, FocusTabRequest,
    FocusTabResponse, IdentifyRequest, IdentifyResponse, ListTabsRequest, ListTabsResponse,
    OpenTabRequest, OpenTabResponse, Project, PtyClientMessage, PtyExit, PtyOutput,
    PtyServerMessage, RenameProjectRequest, RenameProjectResponse, ReportOscRequest,
    ReportOscResponse, SetHookActiveRequest, SetHookActiveResponse, SetTabStateRequest,
    SetTabStateResponse, SetTabTitleRequest, SetTabTitleResponse, TabState, WatchEventsRequest,
};

use crate::pty::PtySupervisor;
use crate::state::{Workspace, WorkspaceError};

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

pub struct RoostService {
    workspace: Arc<Workspace>,
    ptys: Arc<PtySupervisor>,
    socket_path: PathBuf,
}

impl RoostService {
    pub fn new(workspace: Arc<Workspace>, socket_path: PathBuf) -> Self {
        Self {
            workspace,
            ptys: Arc::new(PtySupervisor::new()),
            socket_path,
        }
    }
}

#[tonic::async_trait]
impl Roost for RoostService {
    async fn identify(
        &self,
        req: Request<IdentifyRequest>,
    ) -> Result<Response<IdentifyResponse>, Status> {
        let inner = req.into_inner();
        debug!(client = %inner.client_name, version = %inner.client_version, "identify");
        let (active_project_id, active_tab_id) = self.workspace.active();
        Ok(Response::new(IdentifyResponse {
            socket_path: self.socket_path.to_string_lossy().to_string(),
            pid: process::id() as i32,
            active_project_id,
            active_tab_id,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: roost_proto::PROTOCOL_VERSION,
        }))
    }

    async fn open_tab(
        &self,
        req: Request<OpenTabRequest>,
    ) -> Result<Response<OpenTabResponse>, Status> {
        let r = req.into_inner();
        let cwd = if r.cwd.is_empty() {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "/".into())
        } else {
            r.cwd
        };

        // If the caller didn't pin a project, auto-create a default one.
        let project_id = if r.project_id == 0 {
            self.workspace.ensure_default_project(&cwd)
        } else {
            r.project_id
        };

        let tab = self
            .workspace
            .open_tab(project_id, &cwd, &r.title)
            .map_err(map_err)?;

        let cols = if r.cols == 0 {
            DEFAULT_COLS
        } else {
            r.cols as u16
        };
        let rows = if r.rows == 0 {
            DEFAULT_ROWS
        } else {
            r.rows as u16
        };

        // The cols/rows fields above bound the size StreamPty would
        // attach with, but PTY creation itself is deferred to the
        // first StreamPty call. The previous draft of this handler
        // also called `ptys.spawn(...)` here — that produced two
        // shells per OpenTab+StreamPty sequence (the first orphaned)
        // because the streaming handler unconditionally spawns again.
        // Removing the duplicate spawn is a clean win until Phase 5's
        // multi-UI reattach semantics land; that work will introduce
        // a real lifecycle for the supervisor handle.
        let _ = (cols, rows);
        info!(tab_id = tab.id, project_id, "tab opened");

        Ok(Response::new(OpenTabResponse {
            tab: Some(roost_proto::v1::Tab {
                id: tab.id,
                project_id: tab.project_id,
                title: tab.title,
                cwd: tab.cwd,
                state: tab.state as i32,
                has_notification: tab.has_notification,
                is_active: false,
                user_titled: tab.user_titled,
                position: tab.position,
                created_at: tab.created_at,
                last_active: tab.last_active,
                hook_active: tab.hook_active,
            }),
        }))
    }

    async fn close_tab(
        &self,
        req: Request<CloseTabRequest>,
    ) -> Result<Response<CloseTabResponse>, Status> {
        let tab_id = req.into_inner().tab_id;
        self.workspace.close_tab(tab_id).map_err(map_err)?;
        self.ptys.close(tab_id);
        Ok(Response::new(CloseTabResponse {}))
    }

    async fn list_tabs(
        &self,
        _req: Request<ListTabsRequest>,
    ) -> Result<Response<ListTabsResponse>, Status> {
        Ok(Response::new(ListTabsResponse {
            projects: self.workspace.snapshot(),
        }))
    }

    async fn create_project(
        &self,
        req: Request<CreateProjectRequest>,
    ) -> Result<Response<CreateProjectResponse>, Status> {
        let r = req.into_inner();
        let cwd = if r.cwd.is_empty() {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "/".into())
        } else {
            r.cwd
        };
        let stored = self.workspace.create_project(&r.name, &cwd).map_err(map_err)?;
        info!(project_id = stored.id, name = %stored.name, "project created");
        Ok(Response::new(CreateProjectResponse {
            project: Some(Project {
                id: stored.id,
                name: stored.name,
                cwd: stored.cwd,
                position: stored.position,
                created_at: stored.created_at,
                tabs: vec![],
            }),
        }))
    }

    async fn rename_project(
        &self,
        req: Request<RenameProjectRequest>,
    ) -> Result<Response<RenameProjectResponse>, Status> {
        let r = req.into_inner();
        self.workspace
            .rename_project(r.project_id, &r.name)
            .map_err(map_err)?;
        Ok(Response::new(RenameProjectResponse {}))
    }

    async fn delete_project(
        &self,
        req: Request<DeleteProjectRequest>,
    ) -> Result<Response<DeleteProjectResponse>, Status> {
        let r = req.into_inner();
        self.workspace
            .delete_project(r.project_id)
            .map_err(map_err)?;
        info!(project_id = r.project_id, "project deleted");
        Ok(Response::new(DeleteProjectResponse {}))
    }

    type StreamPtyStream =
        Pin<Box<dyn Stream<Item = Result<PtyServerMessage, Status>> + Send + 'static>>;

    async fn stream_pty(
        &self,
        req: Request<Streaming<PtyClientMessage>>,
    ) -> Result<Response<Self::StreamPtyStream>, Status> {
        let mut inbound = req.into_inner();
        let ptys = self.ptys.clone();
        let workspace = self.workspace.clone();

        // First message MUST be PtyAttach.
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("StreamPty: empty stream"))?;
        let attach = match first.kind {
            Some(PtyClientKind::Attach(a)) => a,
            _ => {
                return Err(Status::invalid_argument(
                    "StreamPty: first message must be PtyAttach",
                ))
            }
        };

        let cols = if attach.cols == 0 {
            DEFAULT_COLS
        } else {
            attach.cols as u16
        };
        let rows = if attach.rows == 0 {
            DEFAULT_ROWS
        } else {
            attach.rows as u16
        };

        // Resolve the tab's cwd before spawning. The `OpenTab` path already
        // spawns once, but Phase-3 simplification: re-spawn on attach. This
        // is replaced in Phase 5 when the supervisor truly de-duplicates.
        let tab = workspace
            .tab(attach.tab_id)
            .ok_or_else(|| Status::not_found(format!("tab {} not found", attach.tab_id)))?;

        let mut handle = ptys
            .spawn(attach.tab_id, &tab.cwd, &[], cols, rows)
            .map_err(|e| Status::internal(format!("spawn: {e}")))?;

        let tab_id = attach.tab_id;

        // Spawn an inbound-pump task that translates client messages to
        // PtySupervisor calls. This way the outbound stream below can run
        // independently and we avoid deadlock if either side stalls.
        let ptys_for_input = ptys.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                match msg.kind {
                    Some(PtyClientKind::Input(i)) => {
                        if let Err(err) = ptys_for_input.write(tab_id, i.data).await {
                            debug!(tab_id, ?err, "pty write failed");
                            break;
                        }
                    }
                    Some(PtyClientKind::Resize(r)) => {
                        let _ = ptys_for_input
                            .resize(tab_id, r.cols as u16, r.rows as u16)
                            .await;
                    }
                    Some(PtyClientKind::Attach(_)) => {
                        warn!(tab_id, "duplicate PtyAttach received; ignoring");
                    }
                    None => continue,
                }
            }
            debug!(tab_id, "client input stream closed");
        });

        let outbound = stream! {
            loop {
                tokio::select! {
                    Some(bytes) = handle.output_rx.recv() => {
                        yield Ok(PtyServerMessage {
                            kind: Some(PtyServerKind::Output(PtyOutput { data: bytes })),
                        });
                    }
                    Some(code) = handle.exit_rx.recv() => {
                        yield Ok(PtyServerMessage {
                            kind: Some(PtyServerKind::Exit(PtyExit {
                                status: code,
                                reason: String::new(),
                            })),
                        });
                        break;
                    }
                    else => break,
                }
            }
            debug!(tab_id, "outbound stream completed");
        };

        Ok(Response::new(Box::pin(outbound) as Self::StreamPtyStream))
    }

    type WatchEventsStream = Pin<Box<dyn Stream<Item = Result<Event, Status>> + Send + 'static>>;

    async fn watch_events(
        &self,
        req: Request<WatchEventsRequest>,
    ) -> Result<Response<Self::WatchEventsStream>, Status> {
        let filter = req.into_inner().tab_id_filter;
        let mut rx = self.workspace.subscribe();
        let workspace = self.workspace.clone();

        // Trigger an immediate structural resync so a freshly-attached
        // client doesn't have to call ListTabs separately.
        workspace.broadcast_structural_resync();

        let stream = stream! {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if filter == 0 || event_matches_tab(&event, filter) {
                            yield Ok(event);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(filter, lagged = n, "WatchEvents subscriber lagged");
                        // Lagged subscribers have irrecoverably missed `n`
                        // events. For structural events (tab opened, tab
                        // deleted, reorders) those gaps would leave a
                        // client with a stale model. Re-emit the current
                        // structural snapshot so the client can resync
                        // without explicitly calling ListTabs again.
                        // Per-tab event content (titles, cwd, state) the
                        // client may still miss gets re-asserted on the
                        // next mutation that touches each tab.
                        workspace.broadcast_structural_resync();
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        };

        Ok(Response::new(Box::pin(stream) as Self::WatchEventsStream))
    }

    async fn create_notification(
        &self,
        req: Request<CreateNotificationRequest>,
    ) -> Result<Response<CreateNotificationResponse>, Status> {
        let r = req.into_inner();
        self.workspace
            .fire_notification(r.tab_id, &r.title, &r.body)
            .map_err(map_err)?;
        Ok(Response::new(CreateNotificationResponse {}))
    }

    async fn set_tab_title(
        &self,
        req: Request<SetTabTitleRequest>,
    ) -> Result<Response<SetTabTitleResponse>, Status> {
        let r = req.into_inner();
        self.workspace
            .set_tab_title(r.tab_id, &r.title, true)
            .map_err(map_err)?;
        Ok(Response::new(SetTabTitleResponse {}))
    }

    async fn focus_tab(
        &self,
        req: Request<FocusTabRequest>,
    ) -> Result<Response<FocusTabResponse>, Status> {
        let r = req.into_inner();
        let (prev_project, prev_tab) = self.workspace.focus_tab(r.tab_id).map_err(map_err)?;
        Ok(Response::new(FocusTabResponse {
            previous_project_id: prev_project,
            previous_tab_id: prev_tab,
        }))
    }

    async fn set_tab_state(
        &self,
        req: Request<SetTabStateRequest>,
    ) -> Result<Response<SetTabStateResponse>, Status> {
        let r = req.into_inner();
        let state = TabState::try_from(r.state)
            .map_err(|_| Status::invalid_argument(format!("unknown tab state {}", r.state)))?;
        self.workspace
            .set_tab_state(r.tab_id, state)
            .map_err(map_err)?;
        Ok(Response::new(SetTabStateResponse {}))
    }

    async fn clear_tab_notification(
        &self,
        req: Request<ClearTabNotificationRequest>,
    ) -> Result<Response<ClearTabNotificationResponse>, Status> {
        let r = req.into_inner();
        self.workspace
            .set_tab_notification(r.tab_id, false)
            .map_err(map_err)?;
        Ok(Response::new(ClearTabNotificationResponse {}))
    }

    async fn set_hook_active(
        &self,
        req: Request<SetHookActiveRequest>,
    ) -> Result<Response<SetHookActiveResponse>, Status> {
        let r = req.into_inner();
        self.workspace
            .set_hook_active(r.tab_id, r.active)
            .map_err(map_err)?;
        Ok(Response::new(SetHookActiveResponse {}))
    }

    async fn report_osc(
        &self,
        req: Request<ReportOscRequest>,
    ) -> Result<Response<ReportOscResponse>, Status> {
        let r = req.into_inner();
        // Phase 3 routing is intentionally minimal: we only honour OSC 7
        // (cwd) and OSC 9/777 (notification). OSC 0/1/2 (titles) are
        // mostly handled UI-side; the UI calls SetTabTitle when it wants
        // to upgrade to a server-side write.
        match r.osc_command {
            7 => {
                if !r.payload.is_empty() {
                    let cwd = parse_cwd_from_osc7(&r.payload).unwrap_or(r.payload);
                    if let Err(err) = self.workspace.set_tab_cwd(r.tab_id, &cwd) {
                        debug!(tab_id = r.tab_id, ?err, "set_tab_cwd from OSC 7 failed");
                    }
                }
            }
            9 => {
                let title = r.payload;
                if let Err(err) = self.workspace.fire_notification(r.tab_id, &title, "") {
                    debug!(
                        tab_id = r.tab_id,
                        ?err,
                        "fire_notification from OSC 9 failed"
                    );
                }
            }
            777 => {
                let (title, body) = parse_osc_777(&r.payload);
                if let Err(err) = self.workspace.fire_notification(r.tab_id, &title, &body) {
                    debug!(
                        tab_id = r.tab_id,
                        ?err,
                        "fire_notification from OSC 777 failed"
                    );
                }
            }
            _ => {
                debug!(osc = r.osc_command, "ignored OSC sequence");
            }
        }
        Ok(Response::new(ReportOscResponse {}))
    }
}

fn event_matches_tab(event: &Event, tab_id: i64) -> bool {
    match &event.kind {
        Some(roost_proto::v1::event::Kind::Notification(e)) => e.tab_id == tab_id,
        Some(roost_proto::v1::event::Kind::TabState(e)) => e.tab_id == tab_id,
        Some(roost_proto::v1::event::Kind::TabNotification(e)) => e.tab_id == tab_id,
        Some(roost_proto::v1::event::Kind::TabDeleted(e)) => e.tab_id == tab_id,
        Some(roost_proto::v1::event::Kind::TabTitle(e)) => e.tab_id == tab_id,
        Some(roost_proto::v1::event::Kind::TabCwd(e)) => e.tab_id == tab_id,
        Some(roost_proto::v1::event::Kind::TabOpened(e)) => {
            e.tab.as_ref().map(|t| t.id) == Some(tab_id)
        }
        Some(roost_proto::v1::event::Kind::HookActive(e)) => e.tab_id == tab_id,
        // Structural / global events are workspace-wide; always include them.
        Some(roost_proto::v1::event::Kind::ProjectsReordered(_)) => true,
        Some(roost_proto::v1::event::Kind::TabsReordered(_)) => true,
        Some(roost_proto::v1::event::Kind::Active(_)) => true,
        Some(roost_proto::v1::event::Kind::ProjectCreated(_)) => true,
        Some(roost_proto::v1::event::Kind::ProjectRenamed(_)) => true,
        Some(roost_proto::v1::event::Kind::ProjectDeleted(_)) => true,
        None => false,
    }
}

/// Parse the cwd out of an OSC 7 payload. The payload is `file://<host>/<path>`,
/// where `<path>` is percent-encoded. We strip the scheme + host and
/// percent-decode so that `cd "/Users/me/Documents/My Project"` stored
/// as `/Users/me/Documents/My%20Project` round-trips back to the
/// original path with the space restored.
fn parse_cwd_from_osc7(payload: &str) -> Option<String> {
    // OSC 7 payload is `file://<host>/<path>`. Strip the scheme + host,
    // then percent-decode the path (spaces, unicode, etc.).
    let s = payload.strip_prefix("file://")?;
    let path_start = s.find('/')?;
    let raw_path = &s[path_start..];
    percent_encoding::percent_decode_str(raw_path)
        .decode_utf8()
        .ok()
        .map(|cow| cow.into_owned())
}

fn parse_osc_777(payload: &str) -> (String, String) {
    // `notify;Title;Body`. We arrive here without the leading `notify;`
    // already if the UI strips it; tolerate both forms.
    let s = payload.strip_prefix("notify;").unwrap_or(payload);
    if let Some((title, body)) = s.split_once(';') {
        (title.to_string(), body.to_string())
    } else {
        (s.to_string(), String::new())
    }
}

#[allow(clippy::needless_pass_by_value)] // call sites use it directly via `.map_err(map_err)`.
fn map_err(err: WorkspaceError) -> Status {
    match err {
        WorkspaceError::ProjectNotFound(_) | WorkspaceError::TabNotFound(_) => {
            Status::not_found(err.to_string())
        }
        WorkspaceError::Store(_) => Status::internal(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc7_decodes_percent_escapes() {
        // Spaces in cd-target paths come over the wire percent-encoded
        // (cd "/Users/me/My Project" → file://host/Users/me/My%20Project).
        let cwd = parse_cwd_from_osc7("file://host/Users/me/My%20Project");
        assert_eq!(cwd.as_deref(), Some("/Users/me/My Project"));
    }

    #[test]
    fn osc7_handles_unicode() {
        // Multi-byte UTF-8 percent sequences should round-trip too.
        let cwd = parse_cwd_from_osc7("file://host/tmp/r%C3%B6ost");
        assert_eq!(cwd.as_deref(), Some("/tmp/röost"));
    }

    #[test]
    fn osc7_rejects_non_file_scheme() {
        assert!(parse_cwd_from_osc7("https://example.com/foo").is_none());
    }
}
