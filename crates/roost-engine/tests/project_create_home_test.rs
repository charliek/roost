//! `project.create`'s empty-cwd resolution (plan 063 §D4): the engine
//! substitutes `$HOME` (falling back to `/`) for an empty `cwd`, at
//! create time, so the project row and its first tab agree on where the
//! project "is" — an empty cwd is never stored verbatim the way it used
//! to be.
//!
//! Lives in its own integration-test binary because it mutates the
//! process-global `HOME` env var — the same reason
//! `workspace_open_tab_test.rs` is split out. Both tests here touch
//! `HOME`, so they additionally serialize on a mutex rather than relying
//! on being the file's only test.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};

use roost_engine::ipc::IpcHandler;
use roost_engine::{PtySupervisor, Workspace};
use roost_ipc::messages::{ops, ProjectCreateParams, ProjectCreateResult};
use roost_ipc::{IpcClient, IpcServer};
use tempfile::tempdir;
use tokio::sync::Mutex;

// An async-aware lock, not `std::sync::Mutex`: both tests below hold it
// across real `.await` points (the whole in-process `serve()` round
// trip), and a std guard held across an await is exactly what
// `clippy::await_holding_lock` exists to catch.
static HOME_LOCK: Mutex<()> = Mutex::const_new(());

/// Sets `HOME` for its lifetime and restores whatever the var held
/// beforehand on drop, so a failing assertion can't leak a clobbered
/// `HOME` into a later test.
struct HomeVar {
    prev: Option<OsString>,
}

impl HomeVar {
    fn set(value: &str) -> Self {
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", value);
        HomeVar { prev }
    }
}

impl Drop for HomeVar {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

async fn connect_with_retry(socket_path: &std::path::Path) -> IpcClient {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match IpcClient::connect(socket_path).await {
            Ok(client) => return client,
            Err(error) => {
                assert!(Instant::now() < deadline, "server never came up: {error}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// A bare handler + server over an in-memory workspace, dialed and
/// ready. `project.create` never spawns a PTY, so nothing here needs a
/// real shell.
async fn serve() -> (IpcClient, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");

    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let handler = IpcHandler::new(
        workspace,
        supervisor,
        socket_path.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );

    let server = IpcServer::bind(&socket_path, handler).await.expect("bind");
    let server_socket = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    (connect_with_retry(&server_socket).await, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_cwd_resolves_to_home_at_create_time() {
    let _lock = HOME_LOCK.lock().await;
    let _home = HomeVar::set("/tmp/roost-project-create-home-test-home");
    let (mut client, _dir) = serve().await;

    let created: ProjectCreateResult = client
        .call(
            ops::PROJECT_CREATE,
            ProjectCreateParams {
                name: "".into(),
                cwd: "".into(),
            },
        )
        .await
        .expect("project.create");

    assert_eq!(created.project.name, "Untitled 1");
    assert_eq!(
        created.project.cwd, "/tmp/roost-project-create-home-test-home",
        "an empty cwd must resolve to $HOME before the project row is stored"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_empty_cwd_is_left_untouched() {
    let _lock = HOME_LOCK.lock().await;
    let _home = HomeVar::set("/should-not-be-used");
    let (mut client, _dir) = serve().await;

    let created: ProjectCreateResult = client
        .call(
            ops::PROJECT_CREATE,
            ProjectCreateParams {
                name: "mine".into(),
                cwd: "/var/tmp".into(),
            },
        )
        .await
        .expect("project.create");

    assert_eq!(created.project.cwd, "/var/tmp");
}
