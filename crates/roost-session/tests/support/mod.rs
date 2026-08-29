//! Shared scaffolding for the session integration tests.
//!
//! Everything here drives the real `serve` path in-process: real
//! `Workspace`, real `PtySupervisor`, real shells, real socket, real
//! instance locks. What it skips is the process shell around it — the
//! fork, the stdio redirect, the process-global subscriber — because
//! those are process-level facts a `#[tokio::test]` cannot own, and C6's
//! pytest lane covers them against the actual binary.
//!
//! No sleeps as synchronization: every wait is a poll against a
//! deadline, and every deadline scales with `ROOST_TEST_TIMEOUT_SCALE`
//! (the same knob the Python harness reads; CI sets it to 3).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use roost_engine::single_instance::{self, InstanceLocks};
use roost_ipc::messages::{
    ops, IdentifyParams, IdentifyResult, SessionConnectParams, SessionConnectResult,
    SessionIdentify, SessionIdentifyParams, SessionStopParams, SessionStopResult, Tab,
    TabDumpParams, TabDumpResolvedParams, TabDumpResolvedResult, TabDumpResult, TabListResult,
    TabOpenParams, TabOpenResult, TabResizeParams, TabSetTitleParams,
};
use roost_ipc::IpcClient;
use roost_session::{Readiness, SessionConfig};
use tempfile::TempDir;

pub const APP_LABEL: &str = "RoostSessionTest";
pub const APP_ID: &str = "ai.stridelabs.Roost.session.test";

/// Gap between polls. Short enough that a test is not dominated by it,
/// long enough not to spin a core.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Budget for anything that involves spawning a shell and waiting for it
/// to do something.
const DEFAULT_BUDGET: Duration = Duration::from_secs(20);

/// Scale a budget by `ROOST_TEST_TIMEOUT_SCALE`, through the same reader
/// the daemon itself uses for its own waits.
pub fn scaled(base: Duration) -> Duration {
    base.mul_f64(roost_session::consts::timeout_scale())
}

/// A deadline for the default budget.
pub fn deadline() -> Instant {
    Instant::now() + scaled(DEFAULT_BUDGET)
}

pub async fn tick() {
    tokio::time::sleep(POLL_INTERVAL).await;
}

/// The directories one session run needs, and the paths derived from
/// them. Survives a stop, so a test can start a second session over the
/// same state and watch it restore.
pub struct Layout {
    dir: TempDir,
    pub launch_cwd: PathBuf,
}

impl Layout {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let launch_cwd = dir.path().join("launch");
        std::fs::create_dir_all(&launch_cwd).expect("create the launch dir");
        Self { dir, launch_cwd }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn socket_path(&self) -> PathBuf {
        self.dir.path().join("roost.sock")
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.path().join("state.json")
    }

    pub fn socket_lock_path(&self) -> PathBuf {
        self.dir.path().join("roost.lock")
    }

    pub fn state_lock_path(&self) -> PathBuf {
        self.dir.path().join("state.lock")
    }

    /// Create a sub-directory under the layout root and hand back its
    /// path — somewhere for a tab to live, or for a shell to write to.
    pub fn subdir(&self, name: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::create_dir_all(&path).expect("create a layout subdir");
        path
    }

    pub fn config(&self, launch_cwd: &Path) -> SessionConfig {
        SessionConfig {
            socket_path: self.socket_path(),
            state_path: self.state_path(),
            app_label: APP_LABEL.into(),
            app_id: APP_ID.into(),
            launch_cwd: launch_cwd.to_path_buf(),
            // Always on here: `tab.feed_pty_bytes` and
            // `tab.capture_pty_input` are how a test puts known bytes
            // through a real tab without scripting a shell, and the
            // sessions these tests start are torn down within seconds,
            // so the capture buffer's growth is bounded by the test.
            test_mode: true,
        }
    }

    pub fn locks(&self) -> InstanceLocks {
        single_instance::acquire_locks(self.socket_lock_path(), self.state_lock_path())
            .expect("take the instance locks")
    }

    /// Start a session over this layout, seeded from `launch_cwd`.
    ///
    /// The returned handle resolves when the session has stopped and its
    /// tail has run — socket unlinked, locks released — so a test can
    /// await it and then start another run over the same state.
    pub fn spawn(&self, launch_cwd: &Path) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        let config = self.config(launch_cwd);
        let locks = self.locks();
        tokio::spawn(async move {
            // The verdict has no reader in-process; tests wait on the
            // socket instead, which is the same thing the parent's
            // `ready` line means.
            let mut readiness = Readiness::Discard;
            roost_session::serve(config, locks, &mut readiness).await
        })
    }
}

/// Dial the session socket, retrying until it answers or the budget
/// runs out. Standing in for the readiness verdict an out-of-process
/// caller would read.
pub async fn connect(socket_path: &Path) -> IpcClient {
    let deadline = deadline();
    loop {
        match IpcClient::connect(socket_path).await {
            Ok(client) => return client,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "session never answered on {}: {error}",
                    socket_path.display()
                );
                tick().await;
            }
        }
    }
}

pub async fn identify(client: &mut IpcClient) -> IdentifyResult {
    client
        .call(
            ops::IDENTIFY,
            IdentifyParams {
                client_name: "roost-session-test".into(),
                client_version: "0".into(),
            },
        )
        .await
        .expect("identify")
}

pub async fn session_identify(client: &mut IpcClient) -> SessionIdentify {
    client
        .call(ops::SESSION_IDENTIFY, SessionIdentifyParams {})
        .await
        .expect("session.identify")
}

/// Take the session's interactive lease. Every lease-gated op needs one,
/// and a test that only wants the lease does not care who held it, so
/// this always takes over.
pub async fn session_connect(client: &mut IpcClient) -> SessionConnectResult {
    client
        .call(
            ops::SESSION_CONNECT,
            SessionConnectParams { takeover: true },
        )
        .await
        .expect("session.connect")
}

pub async fn session_stop(client: &mut IpcClient) -> SessionStopResult {
    client
        .call(ops::SESSION_STOP, SessionStopParams {})
        .await
        .expect("session.stop")
}

pub async fn tab_list(client: &mut IpcClient) -> TabListResult {
    client
        .call(ops::TAB_LIST, serde_json::json!({}))
        .await
        .expect("tab.list")
}

/// Every tab across every project, in project-then-position order.
pub async fn tabs(client: &mut IpcClient) -> Vec<Tab> {
    tab_list(client)
        .await
        .projects
        .into_iter()
        .flat_map(|project| project.tabs)
        .collect()
}

pub async fn open_tab(
    client: &mut IpcClient,
    project_id: i64,
    cwd: &Path,
    title: &str,
    argv: &[&str],
) -> Tab {
    let result: TabOpenResult = client
        .call(
            ops::TAB_OPEN,
            TabOpenParams {
                project_id,
                cwd: cwd.to_string_lossy().into_owned(),
                argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
                cols: 0,
                rows: 0,
                title: title.into(),
            },
        )
        .await
        .expect("tab.open");
    result.tab
}

/// `tab.resize` — reaches the supervisor, so its success is a statement
/// that the tab has a live PTY. The error is surfaced rather than
/// unwrapped because callers assert on both arms.
pub async fn resize_tab(
    client: &mut IpcClient,
    tab_id: i64,
    cols: u32,
    rows: u32,
) -> Result<(), roost_ipc::ClientError> {
    client
        .call::<_, serde_json::Value>(ops::TAB_RESIZE, TabResizeParams { tab_id, cols, rows })
        .await
        .map(|_| ())
}

/// `tab.dump` — served from the tab's own server terminal, with no UI
/// anywhere in the process. On a UI socket this same op hops to the main
/// thread; here it is a round trip through the tab task.
pub async fn tab_dump(client: &mut IpcClient, tab_id: i64) -> TabDumpResult {
    client
        .call(ops::TAB_DUMP, TabDumpParams { tab_id })
        .await
        .expect("tab.dump")
}

/// `tab.dump_resolved` — the same walk, through the production color
/// resolver. Served from the tab task too, so a headless dump and a UI
/// dump come out of one implementation.
pub async fn tab_dump_resolved(client: &mut IpcClient, tab_id: i64) -> TabDumpResolvedResult {
    client
        .call(ops::TAB_DUMP_RESOLVED, TabDumpResolvedParams { tab_id })
        .await
        .expect("tab.dump_resolved")
}

pub async fn set_tab_title(client: &mut IpcClient, tab_id: i64, title: &str) {
    let _: serde_json::Value = client
        .call(
            ops::TAB_SET_TITLE,
            TabSetTitleParams {
                tab_id,
                title: title.into(),
            },
        )
        .await
        .expect("tab.set_title");
}

/// Poll `tab.list` until `predicate` accepts the snapshot.
pub async fn wait_for_tabs(
    client: &mut IpcClient,
    what: &str,
    mut predicate: impl FnMut(&[Tab]) -> bool,
) -> Vec<Tab> {
    let deadline = deadline();
    loop {
        let snapshot = tabs(client).await;
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; tabs were {:?}",
            snapshot
                .iter()
                .map(|tab| (tab.id, tab.title.clone(), tab.cwd.clone()))
                .collect::<Vec<_>>()
        );
        tick().await;
    }
}

/// Poll for a file a spawned shell is expected to write, and return its
/// trimmed contents.
pub async fn wait_for_file(path: &Path) -> String {
    let deadline = deadline();
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if !contents.trim().is_empty() {
                return contents.trim().to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        tick().await;
    }
}

/// Resolve a directory path, falling back to the path itself.
///
/// Two things make raw string comparison of a tab's cwd unsafe. `pwd`
/// reports the kernel's view, and the seeded tab runs the developer's
/// real login shell, whose prompt hook may report its directory over
/// OSC 7 — both resolved, where a tempdir path is not (`/var` is a
/// symlink to `/private/var` on macOS). Whether the OSC lands before a
/// given assertion is pure timing, so compare directories, never
/// strings.
pub fn canonical(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Read `state.json` back through the engine's own reader, so a test
/// asserts on the persisted content rather than on a return value.
pub fn read_state(path: &Path) -> roost_engine::persistence::SnapshotFile {
    roost_engine::persistence::read_state(path)
        .expect("read state.json")
        .expect("state.json must exist after a flush")
}
