//! The one `tab.close` sequence (`roost_engine::application::close_tab`):
//! the row leaves the workspace before the PTY is torn down, and the
//! teardown runs whether or not there was a row (#416).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use roost_engine::application::close_tab;
use roost_engine::{PtySupervisor, SupervisorEvent, Workspace, WorkspaceError};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;

const REAP_BUDGET: Duration = Duration::from_secs(5);

/// A child that outlives the test by far but not forever: if a
/// teardown assertion fails, the runtime's shutdown waits on this
/// child's reaper, so its length bounds how long that failure hangs.
fn quiet_argv() -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), "exec sleep 60".into()]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_tab_closes_cleanly_and_its_child_is_reaped() {
    let workspace = Workspace::new();
    let supervisor = PtySupervisor::new();
    let mut lifecycle = supervisor.subscribe_lifecycle();
    let socket = PathBuf::from("/tmp/roost-close-tab-test.sock");

    let project = workspace
        .create_project("p", "/tmp")
        .expect("create_project");
    let tab = workspace
        .open_tab(project.id, "/tmp", "")
        .expect("open_tab");
    let _rx = supervisor
        .spawn(tab.id, "/tmp", &quiet_argv(), 80, 24, &socket)
        .expect("spawn");

    close_tab(&workspace, &supervisor, tab.id).expect("close_tab");

    assert!(workspace.tab(tab.id).is_err(), "the row is gone");
    assert!(!supervisor.has(tab.id), "the session is gone");
    assert!(
        exited(&mut lifecycle, tab.id).await,
        "the child was hung up and reaped"
    );
}

/// A session with no workspace row still gets its hang-up: `not-found`
/// is the answer, and the teardown is not skipped on the way to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_without_a_row_is_still_torn_down() {
    let workspace = Workspace::new();
    let supervisor = PtySupervisor::new();
    let mut lifecycle = supervisor.subscribe_lifecycle();
    let socket = PathBuf::from("/tmp/roost-close-tab-test.sock");
    let orphan = 99;

    let _rx = supervisor
        .spawn(orphan, "/tmp", &quiet_argv(), 80, 24, &socket)
        .expect("spawn");
    assert!(supervisor.has(orphan));

    let err = close_tab(&workspace, &supervisor, orphan).expect_err("no row");
    assert!(
        matches!(err, WorkspaceError::TabNotFound(id) if id == orphan),
        "{err}"
    );

    assert!(!supervisor.has(orphan), "the session is gone");
    assert!(
        exited(&mut lifecycle, orphan).await,
        "the child was hung up and reaped despite the missing row"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_id_is_not_found() {
    let workspace = Workspace::new();
    let supervisor = PtySupervisor::new();
    let err = close_tab(&workspace, &supervisor, 42).expect_err("nothing to close");
    assert!(matches!(err, WorkspaceError::TabNotFound(42)), "{err}");
}
