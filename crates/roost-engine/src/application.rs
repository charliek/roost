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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use roost_ipc::messages::{Project, Tab, TabOpenParams};

use crate::{AttentionSource, PtyError, PtySupervisor, Workspace, WorkspaceError};

/// The one `tab.close` sequence, shared by the served handler, the
/// in-process client and the facade.
///
/// The row leaves the workspace BEFORE the PTY is torn down. The
/// teardown's SIGHUP starts the exit path, whose `close_row` also
/// removes the row; with the PTY first, that removal can land between
/// the two statements and the op reports `not-found` for a tab it just
/// closed (#416). The supervisor is closed on both arms, as before: a
/// stale id still answers `not-found`, and a session with no row still
/// gets its hang-up.
pub fn close_tab(
    workspace: &Workspace,
    supervisor: &PtySupervisor,
    tab_id: i64,
) -> Result<(), WorkspaceError> {
    let removed = workspace.close_tab(tab_id);
    supervisor.close(tab_id);
    removed
}

/// Where a tab opened from `tab_id` starts — `tab.open`'s
/// `cwd_from_tab`, answered once for every surface that honours it.
///
/// Native first, because the direct PTY child's own cwd follows a `cd`
/// in a shell that emits no OSC 7, and is a path on this machine — an
/// OSC 7 cwd reported across an `ssh` hop is the remote's. Either
/// candidate counts only if it is a directory here, as in
/// [`usable_cwd`], and Linux reports a removed cwd as `… (deleted)`.
/// No row means `None`, even while the supervisor still holds the tab's
/// session — a closing tab leaves the workspace first ([`close_tab`]).
pub fn inherited_cwd(
    workspace: &Workspace,
    supervisor: &PtySupervisor,
    tab_id: i64,
) -> Option<String> {
    let tracked = workspace.tab(tab_id).ok()?.cwd;
    [supervisor.foreground_cwd(tab_id), Some(tracked)]
        .into_iter()
        .flatten()
        .find(|cwd| Path::new(cwd).is_dir())
}

/// Where a shell asked to start in `requested` really starts: there if
/// it is a directory, else in the project's cwd if that is one, else at
/// `$HOME` ([`crate::home_dir`]).
///
/// A cwd counts only if it is a directory because portable-pty spawns at
/// `$HOME` for one that is not, without a word, and the row would then
/// record a cwd the shell never started in (#541). An existing directory
/// the shell cannot enter still counts, and still fails the spawn.
pub fn usable_cwd(requested: &str, project_cwd: &str) -> String {
    usable_cwd_or(requested, project_cwd, &crate::home_dir())
}

/// [`usable_cwd`] with `$HOME` stated, so the rule is testable without
/// the process-global environment.
pub fn usable_cwd_or(requested: &str, project_cwd: &str, home: &str) -> String {
    [requested, project_cwd]
        .into_iter()
        .find(|cwd| Path::new(cwd).is_dir())
        .unwrap_or(home)
        .to_string()
}

/// Where a `tab.open` lands, settled in `params` itself, shared by the
/// served handler and the facade: `cwd_from_tab` replaces `cwd` before
/// anything reads it, `ensure_default_project` included, and a `cwd`
/// that is not a directory ([`usable_cwd`]) is dropped so the empty
/// chain places the tab.
pub fn resolve_open_target(
    workspace: &Workspace,
    supervisor: &PtySupervisor,
    params: &mut TabOpenParams,
    activate: bool,
) {
    if let Some(cwd) = params
        .cwd_from_tab
        .and_then(|source| inherited_cwd(workspace, supervisor, source))
    {
        params.cwd = cwd;
    }
    if !Path::new(&params.cwd).is_dir() {
        params.cwd.clear();
    }
    if params.project_id == 0 {
        params.project_id = workspace.ensure_default_project(&params.cwd, activate);
    }
}

/// The one `tab.open` spawn sequence, shared by the served handler and
/// the in-process client.
///
/// The row is in the workspace — and published — before `spawn` reserves
/// the id in `pending`. A `tab.close` landing in that window removes the
/// row and finds neither a live session nor a pending slot, so the
/// teardown is a no-op and this spawn promotes a live PTY for a row
/// nobody can see (#417). Closing that window is the caller's job and
/// not `spawn`'s: `spawn` holds no workspace handle, and the window lies
/// entirely between `open_tab` returning and the reservation. So re-read
/// the row after the promotion and hang the child up if it went away.
///
/// The rollback is `let _ =` because the row may already be gone — that
/// is exactly the race. And a close landing between the promotion and
/// the re-check makes the re-check's `supervisor.close` a second close:
/// once the waiter has taken the entry it finds nothing, but under a
/// latched `shutting_down` the entry stays until the reap, so the
/// hang-up is re-sent. That second SIGHUP is as narrow as every other
/// double close in the tree and no narrower, and it cannot reach a
/// recycled pid: `terminate_child` signals under the child's reap
/// latch, which refuses once the child has been reaped (#470).
///
/// The shell starts in [`usable_cwd`], and a row that names anywhere
/// else is moved there first, with `tab` refreshed to match: every
/// spawn passes here, restores and in-process opens included, and none
/// of them may leave a row that says where the shell did not start.
///
/// `pub` for the same reason [`close_tab`] is: the race test lives in
/// `tests/` with a real PTY and a multi-thread runtime.
pub fn spawn_for_row(
    workspace: &Workspace,
    supervisor: &PtySupervisor,
    tab: &mut Tab,
    argv: &[String],
    cols: u16,
    rows: u16,
    socket_path: &std::path::Path,
) -> Result<()> {
    let project_cwd = workspace.project_cwd(tab.project_id).unwrap_or_default();
    let cwd = usable_cwd(&tab.cwd, &project_cwd);
    if cwd != tab.cwd {
        match workspace
            .set_tab_start_cwd(tab.id, &cwd)
            .and_then(|()| workspace.tab(tab.id))
        {
            Ok(row) => *tab = row,
            Err(WorkspaceError::TabNotFound(_)) => return Err(PtyError::Cancelled(tab.id).into()),
            Err(err) => return Err(err.into()),
        }
    }
    match supervisor.spawn(tab.id, &tab.cwd, argv, cols, rows, socket_path) {
        // The pre-subscribed receiver `spawn` returns is dropped; the
        // supervisor's stashed twin (`take_initial_receiver`) is what an
        // attach consumes, so early output survives however late that
        // attach runs.
        Ok(_rx) => {}
        Err(err) => {
            let _ = workspace.close_tab(tab.id);
            return Err(err);
        }
    }
    if let Err(WorkspaceError::TabNotFound(_)) = workspace.tab(tab.id) {
        supervisor.close(tab.id);
        return Err(PtyError::Cancelled(tab.id).into());
    }
    Ok(())
}

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
        // An empty cwd resolves in `Workspace::open_tab` (project's cwd
        // → $HOME → "/"), and one that is not a directory in
        // `spawn_for_row`, so every caller (this, the facade,
        // `ops::TAB_OPEN`) gets both once.
        let mut tab = self.workspace.open_tab(project_id, cwd, title, true)?;
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
        spawn_for_row(
            &self.workspace,
            &self.supervisor,
            &mut tab,
            argv,
            cols,
            rows,
            &self.socket_path,
        )
        .context("pty spawn failed")?;
        Ok(tab)
    }

    pub async fn close_tab(&self, tab_id: i64) -> Result<()> {
        Ok(close_tab(&self.workspace, &self.supervisor, tab_id)?)
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
pub(crate) fn pty_dim(value: u32, default: u16, field: &str) -> Result<u16> {
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

/// Hangs its tabs up when dropped, a failed assertion included: a test
/// runtime otherwise waits out each child on the way down.
#[cfg(test)]
pub(crate) struct HangUp(pub(crate) Arc<PtySupervisor>, pub(crate) Vec<i64>);

#[cfg(test)]
impl Drop for HangUp {
    fn drop(&mut self) {
        for tab in &self.1 {
            self.0.close(*tab);
        }
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
        let tab_id = workspace.open_tab(pid, "/", "", true).unwrap().id;
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

    fn string(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// A child for `tab_id` whose own cwd is `dir`, hung up on drop.
    fn spawn_in(supervisor: &Arc<PtySupervisor>, tab_id: i64, dir: &Path) -> HangUp {
        let guard = HangUp(supervisor.clone(), vec![tab_id]);
        let argv = ["/bin/sh", "-c", "exec sleep 30"].map(String::from);
        let socket = Path::new("/tmp/roost-inherited-cwd-test.sock");
        let _rx = supervisor
            .spawn(tab_id, &string(dir), &argv, 80, 24, socket)
            .expect("spawn");
        guard
    }

    /// A workspace holding one tab whose tracked cwd is `tracked`.
    fn workspace_with_tab(tracked: &str) -> (Workspace, i64) {
        let workspace = Workspace::new();
        let project = workspace.create_project("p", "/").unwrap().id;
        let tab = workspace.open_tab(project, tracked, "", true).unwrap().id;
        (workspace, tab)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inherited_cwd_prefers_the_childs_own_cwd_to_the_tracked_one() {
        let (native, tracked) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (workspace, tab) = workspace_with_tab(&string(tracked.path()));
        let supervisor = Arc::new(PtySupervisor::new());
        let _guard = spawn_in(&supervisor, tab, native.path());

        let want = string(&std::fs::canonicalize(native.path()).unwrap());
        assert_eq!(
            inherited_cwd(&workspace, &supervisor, tab),
            Some(want),
            "the native cwd must win over the row's {}",
            tracked.path().display()
        );
    }

    #[test]
    fn inherited_cwd_falls_back_to_the_tracked_cwd_without_a_child() {
        let dir = tempfile::tempdir().unwrap();
        let tracked = string(dir.path());
        let (workspace, tab) = workspace_with_tab(&tracked);
        assert_eq!(
            inherited_cwd(&workspace, &PtySupervisor::new(), tab),
            Some(tracked)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inherited_cwd_skips_a_native_cwd_that_is_no_longer_a_directory() {
        let (native, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let tracked = string(dir.path());
        let (workspace, tab) = workspace_with_tab(&tracked);
        let supervisor = Arc::new(PtySupervisor::new());
        let _guard = spawn_in(&supervisor, tab, native.path());

        native.close().expect("remove the child's cwd");
        let native = supervisor.foreground_cwd(tab).expect("a native read");
        assert!(
            !Path::new(&native).is_dir(),
            "the precondition: the native read is not a directory ({native})"
        );
        assert_eq!(inherited_cwd(&workspace, &supervisor, tab), Some(tracked));
    }

    #[test]
    fn inherited_cwd_is_none_when_the_tracked_cwd_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        for tracked in [dir.path().join("missing"), file] {
            let (workspace, tab) = workspace_with_tab(&string(&tracked));
            assert_eq!(
                inherited_cwd(&workspace, &PtySupervisor::new(), tab),
                None,
                "{}",
                tracked.display()
            );
        }
    }

    #[test]
    fn inherited_cwd_is_none_for_an_unknown_tab() {
        let (workspace, tab) = workspace_with_tab("/");
        assert_eq!(
            inherited_cwd(&workspace, &PtySupervisor::new(), tab + 1),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inherited_cwd_is_none_for_a_session_without_a_row() {
        let native = tempfile::tempdir().unwrap();
        let workspace = Workspace::new();
        let supervisor = Arc::new(PtySupervisor::new());
        let orphan = 99;
        let _guard = spawn_in(&supervisor, orphan, native.path());

        assert_eq!(inherited_cwd(&workspace, &supervisor, orphan), None);
    }

    #[test]
    fn usable_cwd_is_the_requested_directory_else_the_projects_else_home() {
        let dir = tempfile::tempdir().unwrap();
        let [requested, project] = ["requested", "project"].map(|name| {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            string(&path)
        });
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let file = string(&file);
        let gone = string(&dir.path().join("gone"));
        let home = "/home-as-given";

        assert_eq!(usable_cwd_or(&requested, &project, home), requested);
        for not_a_directory in [gone.as_str(), file.as_str(), ""] {
            assert_eq!(
                usable_cwd_or(not_a_directory, &project, home),
                project,
                "{not_a_directory:?}"
            );
            assert_eq!(
                usable_cwd_or(not_a_directory, &gone, home),
                home,
                "{not_a_directory:?}"
            );
        }
        assert_eq!(usable_cwd(&gone, &gone), crate::home_dir());
    }

    /// The request half of #541: a `cwd` that is not a directory is
    /// dropped before anything reads it, `ensure_default_project`
    /// included, whether it was sent as is or beside a `cwd_from_tab`
    /// that resolved nothing.
    #[test]
    fn resolve_open_target_drops_a_cwd_that_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let gone = string(&dir.path().join("gone"));
        let (workspace, source) = workspace_with_tab(&gone);
        let project = workspace.tab(source).unwrap().project_id;
        let supervisor = PtySupervisor::new();

        for cwd_from_tab in [None, Some(source)] {
            let mut params = TabOpenParams {
                project_id: project,
                cwd: gone.clone(),
                cwd_from_tab,
                ..TabOpenParams::default()
            };
            resolve_open_target(&workspace, &supervisor, &mut params, true);
            assert_eq!(params.cwd, "", "{cwd_from_tab:?}");
        }

        let kept = string(dir.path());
        let mut params = TabOpenParams {
            project_id: project,
            cwd: kept.clone(),
            ..TabOpenParams::default()
        };
        resolve_open_target(&workspace, &supervisor, &mut params, true);
        assert_eq!(params.cwd, kept, "a directory is kept");

        let empty = Workspace::new();
        let mut params = TabOpenParams {
            cwd: gone.clone(),
            ..TabOpenParams::default()
        };
        resolve_open_target(&empty, &supervisor, &mut params, true);
        assert_eq!(
            empty.project_cwd(params.project_id).as_deref(),
            Some(""),
            "the default project is not created at {gone}"
        );
    }

    /// The spawn half of #541: a row whose cwd is not a directory is
    /// moved to where the shell really starts, and so is the caller's
    /// copy of it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_for_row_records_where_the_shell_really_started() {
        let project_dir = tempfile::tempdir().unwrap();
        let project_cwd = string(project_dir.path());
        let gone = string(&project_dir.path().join("gone"));
        let workspace = Workspace::new();
        let project = workspace.create_project("p", &project_cwd).unwrap().id;
        let mut tab = workspace.open_tab(project, &gone, "", true).unwrap();
        assert_eq!(tab.cwd, gone, "the precondition: the row names {gone}");
        let supervisor = Arc::new(PtySupervisor::new());
        let _guard = HangUp(supervisor.clone(), vec![tab.id]);

        let argv = ["/bin/sh", "-c", "exec sleep 30"].map(String::from);
        let socket = Path::new("/tmp/roost-spawn-for-row-test.sock");
        spawn_for_row(&workspace, &supervisor, &mut tab, &argv, 80, 24, socket).expect("spawn");

        assert_eq!(workspace.tab(tab.id).unwrap().cwd, project_cwd, "the row");
        assert_eq!(tab.cwd, project_cwd, "the caller's copy");
        let started = string(&std::fs::canonicalize(project_dir.path()).unwrap());
        assert_eq!(
            supervisor.foreground_cwd(tab.id),
            Some(started),
            "the shell"
        );
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
