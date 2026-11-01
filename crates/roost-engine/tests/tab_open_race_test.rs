//! `roost_engine::application::open_and_spawn` against what can land
//! inside it (#417, #640): a close racing the open never leaves a child
//! running, and a project deleted before the publish takes the child
//! with it. The window is the spawn's own promotion seam, the only point
//! inside the open a test can stand in.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use roost_engine::application::{close_tab, open_and_spawn, NewTab};
use roost_engine::{
    PtyError, PtySupervisor, SupervisorEvent, TabOpener, Workspace, WorkspaceError, WorkspaceEvent,
};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;

const REAP_BUDGET: Duration = Duration::from_secs(5);

fn fixture() -> (Arc<Workspace>, Arc<PtySupervisor>, PathBuf, i64) {
    let workspace = Arc::new(Workspace::new());
    let supervisor = Arc::new(PtySupervisor::new());
    let socket = PathBuf::from("/tmp/roost-tab-open-race-test.sock");
    let project = workspace
        .create_project("p", "/tmp")
        .expect("create_project");
    (workspace, supervisor, socket, project.id)
}

/// A child that outlives the test by far but not forever: if a
/// teardown assertion fails, the runtime's shutdown waits on this
/// child's reaper, so its length bounds how long that failure hangs.
fn quiet_argv() -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), "exec sleep 60".into()]
}

fn open(
    workspace: &Workspace,
    supervisor: &PtySupervisor,
    socket: &Path,
    project_id: i64,
    argv: &[String],
) -> anyhow::Result<roost_ipc::messages::Tab> {
    open_and_spawn(
        workspace,
        supervisor,
        socket,
        NewTab {
            project_id,
            cwd: "/tmp",
            title: "",
            activate: true,
            opener: &TabOpener::None,
            argv,
            cols: 80,
            rows: 24,
        },
    )
}

/// Wait for the supervisor to report `tab_id` reaped: proof the
/// teardown was initiated, not just that the map entry went away.
async fn exited(lifecycle: &mut broadcast::Receiver<SupervisorEvent>, tab_id: i64) -> bool {
    let deadline = Instant::now() + REAP_BUDGET;
    while Instant::now() < deadline {
        match lifecycle.try_recv() {
            Ok(SupervisorEvent::TabExited { tab_id: id, .. }) if id == tab_id => return true,
            Ok(_) => {}
            Err(TryRecvError::Empty) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(other) => panic!("lifecycle recv error: {other:?}"),
        }
    }
    false
}

/// A close landing inside the open, while the tab is still unlisted,
/// finds no row; it still takes the spawn's reservation back, so the
/// open fails, announces nothing, and its child is hung up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_inside_the_open_tears_the_child_down_and_lists_nothing() {
    let (workspace, supervisor, socket, project) = fixture();
    let mut lifecycle = supervisor.subscribe_lifecycle();
    let mut events = workspace.subscribe();
    let closed: Arc<Mutex<Option<Result<(), WorkspaceError>>>> = Arc::default();
    supervisor.set_promotion_hook({
        let (workspace, supervisor) = (Arc::downgrade(&workspace), Arc::downgrade(&supervisor));
        let closed = Arc::clone(&closed);
        move |tab_id| {
            let (Some(workspace), Some(supervisor)) = (workspace.upgrade(), supervisor.upgrade())
            else {
                return;
            };
            *closed.lock().unwrap() = Some(close_tab(&workspace, &supervisor, tab_id));
        }
    });

    let err = open(&workspace, &supervisor, &socket, project, &quiet_argv())
        .expect_err("the close cancelled the spawn");
    let Some(PtyError::Cancelled(tab_id)) = err.downcast_ref::<PtyError>() else {
        panic!("{err:#}");
    };
    let tab_id = *tab_id;

    assert!(
        matches!(
            closed.lock().unwrap().take(),
            Some(Err(WorkspaceError::TabNotFound(id))) if id == tab_id
        ),
        "the close found no row to remove"
    );
    assert!(workspace.tab(tab_id).is_err(), "no row");
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, WorkspaceEvent::TabOpened(_))),
        "nothing was announced"
    );
    assert!(!supervisor.has(tab_id), "the session is gone");
    assert!(
        exited(&mut lifecycle, tab_id).await,
        "the child was hung up and reaped"
    );
}

/// #640's D5 (f): the project goes away between the reservation and the
/// publish. The open errors and its child is reaped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_deleted_before_the_publish_takes_the_child_with_it() {
    let (workspace, supervisor, socket, project) = fixture();
    let mut lifecycle = supervisor.subscribe_lifecycle();
    let spawned = Arc::new(Mutex::new(None));
    supervisor.set_promotion_hook({
        let workspace = Arc::downgrade(&workspace);
        let spawned = Arc::clone(&spawned);
        move |tab_id| {
            *spawned.lock().unwrap() = Some(tab_id);
            if let Some(workspace) = workspace.upgrade() {
                workspace.delete_project(project).expect("delete_project");
            }
        }
    });

    let err = open(&workspace, &supervisor, &socket, project, &quiet_argv())
        .expect_err("the project is gone");
    let tab_id = spawned.lock().unwrap().expect("the spawn ran");

    assert!(
        matches!(
            err.downcast_ref::<WorkspaceError>(),
            Some(WorkspaceError::ProjectNotFound(id)) if *id == project
        ),
        "{err:#}"
    );
    assert!(workspace.tab(tab_id).is_err(), "no row");
    assert!(
        exited(&mut lifecycle, tab_id).await,
        "the child was hung up and reaped"
    );
    assert!(!supervisor.has(tab_id), "the session is gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uncontended_open_keeps_its_row_and_its_session() {
    let (workspace, supervisor, socket, project) = fixture();

    let tab = open(&workspace, &supervisor, &socket, project, &quiet_argv()).expect("open");

    assert!(workspace.tab(tab.id).is_ok(), "the row is there");
    assert!(supervisor.has(tab.id), "the session is live");

    close_tab(&workspace, &supervisor, tab.id).expect("close_tab");
}
