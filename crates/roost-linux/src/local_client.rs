//! In-process workspace adapter. Replaces the daemon-era
//! [`crate::client::RoostClient`] (gRPC) at M3b of the
//! daemon-removal refactor.
//!
//! `LocalClient` owns shared handles to a [`Workspace`] and a
//! [`PtySupervisor`] and exposes the small set of methods `app.rs`
//! invokes from its async-spawn closures. The shape mirrors the old
//! `RoostClient` so the call-sites in `app.rs` change minimally —
//! same method names, similar argument lists, results returning
//! `roost_ipc::messages` types (which have the same fields as the
//! retired proto types they replace).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use roost_ipc::messages::{Project, Tab};
use tokio::sync::broadcast;

use crate::daemon::{PtyOutputEvent, PtySupervisor, Workspace};

/// In-process workspace + PTY supervisor handle.
#[derive(Clone)]
pub struct LocalClient {
    pub workspace: Arc<Workspace>,
    pub supervisor: Arc<PtySupervisor>,
    /// Socket path for `ROOST_SOCKET` env injection in spawned shells.
    pub socket_path: Arc<PathBuf>,
}

impl LocalClient {
    pub fn new(
        workspace: Arc<Workspace>,
        supervisor: Arc<PtySupervisor>,
        socket_path: PathBuf,
    ) -> Self {
        Self {
            workspace,
            supervisor,
            socket_path: Arc::new(socket_path),
        }
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        Ok(self.workspace.snapshot())
    }

    pub async fn create_project(&self, name: &str, cwd: &str) -> Result<Project> {
        Ok(self.workspace.create_project(name, cwd)?)
    }

    pub async fn rename_project(&self, project_id: i64, name: &str) -> Result<()> {
        Ok(self.workspace.rename_project(project_id, name)?)
    }

    /// Delete a project and its tabs. Returns the cascaded tab ids
    /// so the caller can close the supervisor sessions.
    pub async fn delete_project(&self, project_id: i64) -> Result<Vec<i64>> {
        let cascaded = self.workspace.delete_project(project_id)?;
        for tab_id in &cascaded {
            self.supervisor.close(*tab_id);
        }
        Ok(cascaded)
    }

    pub async fn reorder_projects(&self, project_ids: Vec<i64>) -> Result<()> {
        Ok(self.workspace.reorder_projects(&project_ids)?)
    }

    pub async fn reorder_tabs(&self, project_id: i64, tab_ids: Vec<i64>) -> Result<()> {
        Ok(self.workspace.reorder_tabs(project_id, &tab_ids)?)
    }

    /// Open a tab and spawn the shell. Returns the tab snapshot
    /// plus a `broadcast::Receiver` subscribed BEFORE the supervisor's
    /// reader task started producing — `TabSession::attach_with_receiver`
    /// consumes it, no early-byte loss.
    pub async fn resize_tab(&self, tab_id: i64, cols: u32, rows: u32) -> Result<()> {
        // Same validation as `open_tab` — caller-supplied dims via
        // `roostctl tab resize` or via UI live-resize.
        let cols = pty_dim(cols, 80, "cols")?;
        let rows = pty_dim(rows, 24, "rows")?;
        self.supervisor
            .resize(tab_id, cols, rows)
            .await
            .map_err(|e| anyhow::anyhow!("pty resize failed: {e:?}"))
    }

    pub async fn open_tab(
        &self,
        project_id: i64,
        cwd: &str,
        title: &str,
        cols: u32,
        rows: u32,
    ) -> Result<(Tab, broadcast::Receiver<PtyOutputEvent>)> {
        // Resolve cwd: caller-supplied → project's cwd → $HOME →
        // "/". Matches the Mac side's `LocalClient.openTab`
        // fallback. Both the UI's open-new-tab path and the
        // `roostctl tab open` IPC call site land here, so the
        // fallback is centralized.
        let resolved_cwd: String = if !cwd.is_empty() {
            cwd.to_string()
        } else {
            let projects = self.workspace.snapshot();
            projects
                .into_iter()
                .find(|p| p.id == project_id)
                .map(|p| p.cwd)
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()))
        };
        let tab = self.workspace.open_tab(project_id, &resolved_cwd, title)?;
        // Clamp + validate PTY dims. Zero → terminal default; values
        // exceeding u16 surface as a clear error rather than
        // silently truncating via `as u16` (CR-flagged: a CLI
        // caller passing `--cols 100000` would land with cols=34464
        // and a wildly-misshapen grid). Mirrors the Mac side's
        // `IPCHandlerImpl.ipcDim` validation.
        let cols = pty_dim(cols, 80, "cols")?;
        let rows = pty_dim(rows, 24, "rows")?;
        match self
            .supervisor
            .spawn(tab.id, &resolved_cwd, &[], cols, rows, &self.socket_path)
        {
            Ok(rx) => Ok((tab, rx)),
            Err(err) => {
                let _ = self.workspace.close_tab(tab.id);
                Err(anyhow::anyhow!("pty spawn failed: {err:?}"))
            }
        }
    }

    pub async fn close_tab(&self, tab_id: i64) -> Result<()> {
        self.supervisor.close(tab_id);
        Ok(self.workspace.close_tab(tab_id)?)
    }

    pub async fn set_tab_title(&self, tab_id: i64, title: &str) -> Result<()> {
        Ok(self.workspace.set_tab_title(tab_id, title)?)
    }

    /// Apply an OSC routing decision directly to the workspace.
    /// The legacy code path round-tripped this through the daemon
    /// via `ReportOsc`; in M3b the UI parses OSC in-process and
    /// updates state locally with no round-trip.
    pub fn apply_osc(&self, tab_id: i64, command: u32, payload: &str) {
        match command {
            0..=2 => {
                // Title set from the shell. OSC-from-shell path
                // never overrides a manual rename.
                let _ = self.workspace.set_tab_title_from_osc(tab_id, payload);
            }
            7 => {
                // OSC 7: cwd as `file://host/path` URI.
                if let Some(path) = parse_osc7_path(payload) {
                    let _ = self.workspace.set_tab_cwd(tab_id, &path);
                }
            }
            9 | 99 | 777 => {
                // Notification payload — surface to the UI via the
                // workspace's notification event. The actual
                // libnotify call happens in the UI layer once it
                // sees the WorkspaceEvent::NotificationFired event.
                let (title, body) = parse_notification_payload(command, payload);
                let _ = self.workspace.set_tab_has_notification(tab_id, true);
                let _ = self.workspace.fire_notification(tab_id, &title, &body);
            }
            _ => {
                tracing::debug!(tab_id, command, "ignored OSC");
            }
        }
    }
}

fn parse_osc7_path(payload: &str) -> Option<String> {
    // OSC 7 carries `file://host/abs/path`. The path portion starts
    // at the FIRST `/` after the host (or at index 0 if the host is
    // empty, e.g. `file:///tmp`). A malformed payload with no `/`
    // after the host returns None — the previous implementation's
    // `unwrap_or(0)` would have returned the host segment itself as
    // a "path," writing `host` into the workspace's cwd. CR-flagged.
    let after_scheme = payload.strip_prefix("file://")?;
    let path_start = after_scheme.find('/')?;
    Some(after_scheme[path_start..].to_string())
}

/// Validate + clamp a caller-supplied PTY dimension. Zero → the
/// supplied default; values exceeding `u16::MAX` return an error
/// instead of truncating via `as u16` (which would silently
/// produce e.g. cols=34464 for cols=100000). Mirrors the Rust
/// IPC handler's `u16::try_from` validation in `crates/roost-
/// linux/src/ipc.rs`.
fn pty_dim(value: u32, default: u16, field: &str) -> Result<u16> {
    if value == 0 {
        return Ok(default);
    }
    u16::try_from(value).map_err(|_| anyhow::anyhow!("{field} out of u16 range: {value}"))
}

fn parse_notification_payload(command: u32, payload: &str) -> (String, String) {
    match command {
        // OSC 777 ;notify;Title;Body — drop the leading `notify;`.
        777 => {
            let trimmed = payload.strip_prefix("notify;").unwrap_or(payload);
            let mut parts = trimmed.splitn(2, ';');
            let title = parts.next().unwrap_or("").to_string();
            let body = parts.next().unwrap_or("").to_string();
            (title, body)
        }
        // OSC 9 / 99 carry the title only.
        _ => (payload.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc7_strips_host_prefix() {
        assert_eq!(
            parse_osc7_path("file://host/Users/me"),
            Some("/Users/me".into())
        );
    }

    #[test]
    fn osc7_handles_empty_host() {
        assert_eq!(parse_osc7_path("file:///tmp"), Some("/tmp".into()));
    }

    #[test]
    fn osc7_returns_none_for_host_without_path() {
        // `file://host` (no path after host) must not return "host"
        // as the path — that's the CR-flagged regression. Returns
        // None so the workspace cwd is left unchanged.
        assert_eq!(parse_osc7_path("file://host"), None);
    }

    #[test]
    fn osc777_splits_title_and_body() {
        assert_eq!(
            parse_notification_payload(777, "notify;Build;Passed"),
            ("Build".into(), "Passed".into())
        );
    }

    #[test]
    fn osc9_uses_payload_as_title() {
        assert_eq!(
            parse_notification_payload(9, "Hello"),
            ("Hello".into(), String::new())
        );
    }
}
