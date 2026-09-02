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

use anyhow::{Context, Result};
use roost_ipc::messages::{Project, Tab};

use crate::{AttentionSource, PtySupervisor, Workspace};

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

    pub async fn resize_tab(&self, tab_id: i64, cols: u32, rows: u32) -> Result<()> {
        // Same validation as `open_tab` — caller-supplied dims via
        // `roostctl tab resize` or via UI live-resize.
        let cols = pty_dim(cols, 80, "cols")?;
        let rows = pty_dim(rows, 24, "rows")?;
        self.supervisor
            .resize(tab_id, cols, rows)
            .await
            .context("pty resize failed")
    }

    pub async fn open_tab(
        &self,
        project_id: i64,
        cwd: &str,
        title: &str,
        argv: &[String],
        cols: u32,
        rows: u32,
    ) -> Result<Tab> {
        // cwd resolution (requested → project's cwd → $HOME → "/")
        // lives in `Workspace::open_tab` now, so every caller
        // (this, the facade, `ops::TAB_OPEN`) gets it once.
        let tab = self.workspace.open_tab(project_id, cwd, title)?;
        // Clamp + validate PTY dims. Zero → terminal default; values
        // exceeding u16 surface as a clear error rather than
        // silently truncating via `as u16` (CR-flagged: a CLI
        // caller passing `--cols 100000` would land with cols=34464
        // and a wildly-misshapen grid). Mirrors the Mac side's
        // `IPCHandlerImpl.ipcDim` validation.
        let cols = pty_dim(cols, 80, "cols")?;
        let rows = pty_dim(rows, 24, "rows")?;
        // Spawn `tab.cwd` — the resolved value `open_tab` just
        // returned — not the `cwd` parameter above, which may still
        // be empty. Spawning the parameter regresses this path back
        // to the UI's own cwd (#266).
        match self
            .supervisor
            .spawn(tab.id, &tab.cwd, argv, cols, rows, &self.socket_path)
        {
            // The pre-subscribed receiver spawn returns is dropped;
            // the supervisor's stashed twin (`take_initial_receiver`)
            // is what the UI's attach consumes, so early output
            // survives however late that attach runs.
            Ok(_rx) => Ok(tab),
            Err(err) => {
                let _ = self.workspace.close_tab(tab.id);
                Err(err.context("pty spawn failed"))
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
        apply_osc(&self.workspace, tab_id, command, payload);
    }
}

/// The workspace half of [`LocalClient::apply_osc`], free-standing so a
/// consumer that holds only a `Workspace` can apply the same transitions
/// — the server-VT tab task does, and routing it through a `LocalClient`
/// would put an `Arc<PtySupervisor>` back inside the supervisor.
pub fn apply_osc(workspace: &Workspace, tab_id: i64, command: u32, payload: &str) {
    match command {
        0..=2 => {
            // Title set from the shell. OSC-from-shell path
            // never overrides a manual rename.
            let _ = workspace.set_tab_title_from_osc(tab_id, payload);
        }
        7 => {
            // OSC 7: cwd as `file://host/path` URI.
            if let Some(path) = parse_osc7_path(payload) {
                let _ = workspace.set_tab_cwd(tab_id, &path);
            }
        }
        9 | 99 | 777 => {
            // Notification payload — surface to the UI via the
            // workspace's notification event. The actual
            // libnotify call happens in the UI layer once it
            // sees the WorkspaceEvent::NotificationFired event.
            //
            // `RawOsc` is dropped while a live agent session is
            // mid-turn: the agent already reports its own attention
            // through `tab.agent_report`, and a wrapper shell
            // echoing OSC 9 on top of that double-notifies. The gate
            // is read inside `raise_attention`'s lock — reading it
            // here first would let a concurrent claim slip between
            // the check and the commit.
            let (title, body) = parse_notification_payload(command, payload);
            let _ = workspace.raise_attention(tab_id, &title, &body, AttentionSource::RawOsc);
        }
        133 => {
            // OSC 133 prompt/command mark → the shell axis. Never
            // gated: the shell and agent axes are independent, and
            // derivation decides which one the tab shows.
            let _ = workspace.apply_shell_mark(tab_id, payload);
        }
        _ => {
            tracing::debug!(tab_id, command, "ignored OSC");
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
    use roost_ipc::agent::{AgentLifecycle, OwnershipAction, TabAgentReportParams};
    use roost_ipc::messages::TabState;

    /// A client over an empty in-memory workspace, plus one open tab.
    /// No PTY is ever spawned — `apply_osc` only touches the workspace.
    fn client_with_tab() -> (LocalClient, i64) {
        let workspace = Arc::new(Workspace::new());
        let pid = workspace.create_project("p", "").unwrap().id;
        let tab_id = workspace.open_tab(pid, "/", "").unwrap().id;
        // Unfocused: these tests are about the OSC gate, not about
        // policy §3.5's focus suppression (covered in `daemon::state`).
        workspace.set_window_focused(false);
        let client = LocalClient::new(
            workspace,
            Arc::new(PtySupervisor::new()),
            PathBuf::from("/tmp/roost-test.sock"),
        );
        (client, tab_id)
    }

    fn claim(tab_id: i64, lifecycle: AgentLifecycle) -> TabAgentReportParams {
        TabAgentReportParams {
            session_id: "s1".into(),
            ..TabAgentReportParams::sessionless(
                tab_id,
                "claude",
                OwnershipAction::Claim,
                Some(lifecycle),
            )
        }
    }

    /// Plan §2.2(b)/§3.4: raw OSC 9 / 99 / 777 is dropped while a live
    /// agent session is mid-turn, and works normally outside one. This
    /// is the documented-but-missing behavior the plan restores.
    #[test]
    fn raw_osc_notifications_are_suppressed_under_a_live_agent() {
        let (client, tab_id) = client_with_tab();

        client.apply_osc(tab_id, 9, "Build done");
        assert!(client.workspace.tab(tab_id).unwrap().has_notification);

        let _ = client.workspace.set_tab_has_notification(tab_id, false);
        client
            .workspace
            .agent_report(&claim(tab_id, AgentLifecycle::Working))
            .unwrap();
        for command in [9, 99, 777] {
            client.apply_osc(tab_id, command, "notify;Wrapper;Noise");
            assert!(
                !client.workspace.tab(tab_id).unwrap().has_notification,
                "OSC {command} should be suppressed mid-turn"
            );
        }

        // The `D` failsafe re-opens the gate even though ownership
        // survives as a label.
        client.apply_osc(tab_id, 133, "D;0");
        assert!(client.workspace.tab(tab_id).unwrap().hook_active);
        client.apply_osc(tab_id, 9, "Build done");
        assert!(client.workspace.tab(tab_id).unwrap().has_notification);
    }

    /// Only *raw* OSC is gated — an explicit `notification.create`
    /// (which routes straight at the workspace, not through
    /// `apply_osc`) is never suppressed.
    #[test]
    fn explicit_notifications_are_never_suppressed() {
        let (client, tab_id) = client_with_tab();
        client
            .workspace
            .agent_report(&claim(tab_id, AgentLifecycle::Working))
            .unwrap();
        assert!(client
            .workspace
            .raise_attention(tab_id, "Roost", "explicit", AttentionSource::Structured)
            .unwrap());
        assert!(client.workspace.tab(tab_id).unwrap().has_notification);
    }

    #[test]
    fn osc133_writes_the_shell_axis_through_apply_osc() {
        let (client, tab_id) = client_with_tab();
        client.apply_osc(tab_id, 133, "C");
        assert_eq!(
            client.workspace.tab(tab_id).unwrap().state,
            TabState::Running
        );
        client.apply_osc(tab_id, 133, "D;0");
        assert_eq!(client.workspace.tab(tab_id).unwrap().state, TabState::None);
        // Undefined mark bodies are no-change.
        client.apply_osc(tab_id, 133, "Z");
        assert_eq!(client.workspace.tab(tab_id).unwrap().state, TabState::None);
    }

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
