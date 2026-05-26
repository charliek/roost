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
    ops, AppActivateParams, IdentifyParams, IdentifyResult, NotificationCreateParams,
    PaletteActivateParams, PaletteDismissParams, PaletteOpenParams, PaletteQueryParams,
    PaletteStateParams, PaletteStateResult, ProjectCreateParams, ProjectCreateResult,
    ProjectDeleteParams, ProjectRenameParams, ProjectReorderParams, ScreenshotParams,
    ScreenshotResult, TabClearNotificationParams, TabCloseParams, TabDumpCursor, TabDumpParams,
    TabDumpResult, TabFocusParams, TabFocusResult, TabListResult, TabOpenParams, TabOpenResult,
    TabReorderParams, TabResizeParams, TabSetHookActiveParams, TabSetStateParams,
    TabSetTitleParams, TabWriteParams,
};
use roost_ipc::{Handler, HandlerError};

/// Text snapshot of a tab's terminal viewport, produced on the GTK main
/// thread for the `tab.dump` op. Neutral (lib-side) types so this crate
/// stays independent of the bin's `TerminalView`; the UI fills it from
/// `TerminalView::dump`. `cursor` is `(row, col, visible)`.
pub struct DumpData {
    pub cols: u32,
    pub rows: u32,
    pub cursor: Option<(u32, u32, bool)>,
    pub rows_text: Vec<String>,
}

/// Reply for a [`UiRequest::Screenshot`]: `(png_bytes, width, height)`
/// on success, an error message on failure.
type ScreenshotReply = tokio::sync::oneshot::Sender<Result<(Vec<u8>, u32, u32), String>>;

/// Reply for a [`UiRequest::Dump`]: the viewport text on success, an
/// error message (e.g. tab not found / no live terminal) on failure.
type DumpReply = tokio::sync::oneshot::Sender<Result<DumpData, String>>;

/// Reply for the `palette.*` [`UiRequest`]s: the resulting palette state.
/// Shared by all five — each mutating op answers with the state it
/// produced, so a driver needs no follow-up `palette.state`. Only
/// `PaletteActivate` ever returns the `Err` arm (no palette open, or no
/// row with the given id); the rest always answer `Ok`.
type PaletteReply = tokio::sync::oneshot::Sender<Result<PaletteStateResult, String>>;

/// One unit of work the IPC handler (a tokio worker thread) hands to the
/// GTK main thread — the single seam for anything an op needs to do
/// against GTK / libghostty, which are main-thread-only. The UI drains
/// one channel of these and matches; request-reply variants carry a
/// `oneshot` the main thread answers on. Adding a UI-touching op is a
/// new variant here + one arm in the UI's drain loop, instead of a
/// fresh per-op channel + handler field + setter + receiver + wiring.
pub enum UiRequest {
    /// Raise + focus the running window (#6). Fire-and-forget.
    Activate,
    /// Render the whole window (sidebar + tabs + active terminal) to a
    /// PNG.
    Screenshot { scale: u32, reply: ScreenshotReply },
    /// Read a tab's terminal viewport as text.
    Dump { tab_id: i64, reply: DumpReply },
    /// Open a command-palette root frame and reply with its state.
    /// `kind`: "" / "commands" → command palette; "launcher" → the
    /// custom-command launcher.
    PaletteOpen { kind: String, reply: PaletteReply },
    /// Reply with the current palette state (open?, frame, query, rows).
    PaletteState { reply: PaletteReply },
    /// Set the current frame's filter; reply with the filtered state.
    PaletteQuery { query: String, reply: PaletteReply },
    /// Activate the visible row with this item id — the same dispatch as
    /// its keybind — and reply with the resulting state.
    PaletteActivate { id: String, reply: PaletteReply },
    /// Dismiss any open palette; reply with the (closed) state.
    PaletteDismiss { reply: PaletteReply },
}

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
    /// Set by the running UI: ops that must touch GTK / libghostty
    /// (activate, screenshot, dump) forward a [`UiRequest`] here for the
    /// main thread to service. `None` in headless contexts (tests), so
    /// those ops no-op (activate) or error `internal` (screenshot/dump).
    ui_tx: Option<tokio::sync::mpsc::UnboundedSender<UiRequest>>,
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
            ui_tx: None,
        }
    }

    /// Wire the UI request channel so main-thread-only ops (activate,
    /// screenshot, dump) can reach GTK / libghostty. The UI installs the
    /// sender; the matching receiver is drained on the GTK main thread.
    pub fn with_ui(mut self, tx: tokio::sync::mpsc::UnboundedSender<UiRequest>) -> Self {
        self.ui_tx = Some(tx);
        self
    }

    /// Hand a request-reply [`UiRequest`] to the GTK main thread and
    /// await its answer. The outer `Result` reports channel/UI health
    /// (no UI attached, UI gone, reply dropped); the inner `Result` is
    /// the op's own outcome, which the caller maps to the right error
    /// code (e.g. `not-found` for a missing tab). Shared by the
    /// screenshot + dump arms so the oneshot plumbing lives in one place.
    async fn ui_call<T>(
        &self,
        make: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> UiRequest,
    ) -> Result<Result<T, String>, HandlerError> {
        let tx = self
            .ui_tx
            .as_ref()
            .ok_or_else(|| HandlerError::new("internal", "no UI attached"))?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(make(reply_tx))
            .map_err(|_| HandlerError::new("internal", "UI gone"))?;
        reply_rx
            .await
            .map_err(|_| HandlerError::new("internal", "UI dropped reply"))
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
            // provide one. Reject out-of-range cols/rows with
            // `invalid-param` instead of silently truncating —
            // CR-flagged on PR #78.
            let cols = if p.cols == 0 {
                80u16
            } else {
                u16::try_from(p.cols)
                    .map_err(|_| HandlerError::invalid_param("cols out of u16 range"))?
            };
            let rows = if p.rows == 0 {
                24u16
            } else {
                u16::try_from(p.rows)
                    .map_err(|_| HandlerError::invalid_param("rows out of u16 range"))?
            };
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
            let cols = u16::try_from(p.cols)
                .map_err(|_| HandlerError::invalid_param("cols out of u16 range"))?;
            let rows = u16::try_from(p.rows)
                .map_err(|_| HandlerError::invalid_param("rows out of u16 range"))?;
            h.supervisor
                .resize(p.tab_id, cols, rows)
                .await
                .map_err(pty_err)?;
            Ok(serde_json::json!({}))
        }
        ops::TAB_DUMP => {
            let p: TabDumpParams = decode(params)?;
            let data = h
                .ui_call(|reply| UiRequest::Dump {
                    tab_id: p.tab_id,
                    reply,
                })
                .await?
                .map_err(HandlerError::not_found)?;
            encode(&TabDumpResult {
                cols: data.cols,
                rows: data.rows,
                cursor: data
                    .cursor
                    .map(|(row, col, visible)| TabDumpCursor { row, col, visible }),
                rows_text: data.rows_text,
            })
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
        ops::APP_ACTIVATE => {
            // Validate the envelope like every other op (rejects
            // unknown fields) rather than ACK-ing arbitrary payloads.
            let _p: AppActivateParams = decode(params)?;
            // Second-launch window raise (#6). Best-effort: forward to
            // the GTK main thread if wired. A dropped receiver (window
            // gone) or a headless handler is a no-op.
            if let Some(tx) = &h.ui_tx {
                let _ = tx.send(UiRequest::Activate);
            }
            Ok(serde_json::json!({}))
        }
        ops::SCREENSHOT => {
            let p: ScreenshotParams = decode(params)?;
            if !(1..=2).contains(&p.scale) {
                return Err(HandlerError::invalid_param(format!(
                    "scale must be 1 or 2, got {}",
                    p.scale
                )));
            }
            let (png, width, height) = h
                .ui_call(|reply| UiRequest::Screenshot {
                    scale: p.scale,
                    reply,
                })
                .await?
                .map_err(|m| HandlerError::new("internal", m))?;
            // Preflight the 16 MiB IPC frame cap: the response rides one
            // newline-delimited JSON frame, and `png` dominates it once
            // base64-expanded (~4/3). Fail with a structured error here
            // rather than letting the oversized frame fail late during
            // transport (`frame-too-large` on the wire).
            screenshot_frame_guard(png.len())?;
            encode(&ScreenshotResult {
                png,
                width,
                height,
                scale: p.scale,
            })
        }
        ops::PALETTE_OPEN => {
            let p: PaletteOpenParams = decode(params)?;
            if !matches!(p.kind.as_str(), "" | "commands" | "launcher") {
                return Err(HandlerError::invalid_param(format!(
                    "unknown palette kind {:?} (want \"commands\" or \"launcher\")",
                    p.kind
                )));
            }
            let state = h
                .ui_call(|reply| UiRequest::PaletteOpen {
                    kind: p.kind,
                    reply,
                })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::PALETTE_STATE => {
            // Nullary, but still validate the envelope (reject stray
            // fields) like every other op — matches the Mac handler.
            let _p: PaletteStateParams = decode(params)?;
            let state = h
                .ui_call(|reply| UiRequest::PaletteState { reply })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::PALETTE_QUERY => {
            let p: PaletteQueryParams = decode(params)?;
            let state = h
                .ui_call(|reply| UiRequest::PaletteQuery {
                    query: p.query,
                    reply,
                })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::PALETTE_ACTIVATE => {
            let p: PaletteActivateParams = decode(params)?;
            let state = h
                .ui_call(|reply| UiRequest::PaletteActivate { id: p.id, reply })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::PALETTE_DISMISS => {
            let _p: PaletteDismissParams = decode(params)?;
            let state = h
                .ui_call(|reply| UiRequest::PaletteDismiss { reply })
                .await?
                .map_err(palette_err)?;
            encode(&state)
        }
        ops::EVENTS_SUBSCRIBE => {
            // Honest failure rather than a false ACK: the server never
            // pushes events on the connection yet, so a client that
            // "subscribed" would wait forever. Surface not-implemented
            // so it can fall back (e.g. poll `tab.list`). Real
            // streaming lands with its first consumer — the planned
            // `roostctl watch` (#9).
            Err(HandlerError::new(
                "not-implemented",
                "events.subscribe is not yet implemented",
            ))
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

/// Reject a screenshot whose base64-encoded PNG would overflow the IPC
/// frame cap. base64 expands by 4/3 (`ceil(n/3)*4`); a small margin
/// covers the JSON envelope (`id` / `ok` / `result` / dims).
fn screenshot_frame_guard(png_len: usize) -> Result<(), HandlerError> {
    const ENVELOPE_MARGIN: usize = 1024;
    let encoded = (png_len + 2) / 3 * 4;
    if encoded + ENVELOPE_MARGIN > roost_ipc::MAX_FRAME_BYTES {
        return Err(HandlerError::new(
            "internal",
            format!(
                "screenshot too large: {encoded} base64 bytes exceeds the {} byte IPC frame cap (try --scale 1)",
                roost_ipc::MAX_FRAME_BYTES
            ),
        ));
    }
    Ok(())
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

/// Map a `palette.activate` failure to the wire. Both cases — no palette
/// open, or no visible row with the requested id — are "act on something
/// that isn't there", i.e. `not-found`.
fn palette_err(msg: String) -> HandlerError {
    HandlerError::not_found(msg)
}
