//! `roost_engine::application::spawn_for_row` against a close that beats
//! the supervisor's reservation (#417). The window and why the re-check
//! closes it are stated on the helper.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use roost_engine::application::{close_tab, spawn_for_row};
use roost_engine::{PtyError, PtySupervisor, SupervisorEvent, Workspace};
use roost_ipc::messages::Tab;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;

const REAP_BUDGET: Duration = Duration::from_secs(5);

fn fixture() -> (Workspace, PtySupervisor, PathBuf, Tab) {
    let workspace = Workspace::new();
    let supervisor = PtySupervisor::new();
    let socket = PathBuf::from("/tmp/roost-tab-open-race-test.sock");
    let project = workspace
        .create_project("p", "/tmp")
        .expect("create_project");
    let tab = workspace
        .open_tab(project.id, "/tmp", "")
        .expect("open_tab");
    (workspace, supervisor, socket, tab)
}

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

/// Ordering by end state is exact here: `PtySupervisor::close` branches on
/// its map contents, not on when it ran, so calling the close first *is*
/// the interleaving under test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_that_beats_the_reservation_still_tears_the_child_down() {
    let (workspace, supervisor, socket, tab) = fixture();
    let mut lifecycle = supervisor.subscribe_lifecycle();

    close_tab(&workspace, &supervisor, tab.id).expect("the row was there to close");

    let err = spawn_for_row(
        &workspace,
        &supervisor,
        &tab,
        &quiet_argv(),
        80,
        24,
        &socket,
    )
    .expect_err("the row is gone");
    assert!(
        matches!(err.downcast_ref::<PtyError>(), Some(PtyError::Cancelled(id)) if *id == tab.id),
        "{err:?}"
    );

    assert!(!supervisor.has(tab.id), "the session is gone");
    assert!(
        exited(&mut lifecycle, tab.id).await,
        "the child was hung up and reaped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uncontended_open_keeps_its_row_and_its_session() {
    let (workspace, supervisor, socket, tab) = fixture();

    spawn_for_row(
        &workspace,
        &supervisor,
        &tab,
        &quiet_argv(),
        80,
        24,
        &socket,
    )
    .expect("spawn");

    assert!(workspace.tab(tab.id).is_ok(), "the row is still there");
    assert!(supervisor.has(tab.id), "the session is live");

    close_tab(&workspace, &supervisor, tab.id).expect("close_tab");
}
