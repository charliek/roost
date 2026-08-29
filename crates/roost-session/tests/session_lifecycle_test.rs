//! The whole arc of one session: bind, identify, open a tab, watch it
//! close itself when its shell exits, stop, and leave nothing behind.
//!
//! Everything here is asserted through the socket — the same ops
//! `roostctl` and the pytest lane drive — rather than against internal
//! state, because the socket is the session's entire contract.

mod support;

use std::time::Duration;

use roost_ipc::messages::AttachPayloadKind;
use roost_ipc::messages::SESSION_PROTOCOL_VERSION;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_serves_identifies_reaps_and_stops_clean() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();
    let served = layout.spawn(&launch_cwd);
    let socket_path = layout.socket_path();

    let mut client = support::connect(&socket_path).await;

    // `identify` — the op every client leads with, answered by a
    // handler with no UI behind it.
    let id = support::identify(&mut client).await;
    assert_eq!(id.app_label, support::APP_LABEL);
    assert_eq!(id.app_id, support::APP_ID);
    assert_eq!(id.socket_path, socket_path.to_string_lossy());
    assert!(id.pid > 0);

    // `session.identify` — the op that only answers because the handler
    // was promoted with `with_session`. A UI socket returns `unknown-op`
    // here, which is what makes this the proof of promotion.
    let session = support::session_identify(&mut client).await;
    assert_eq!(session.session_protocol, SESSION_PROTOCOL_VERSION);
    assert_eq!(session.app_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(session.session_id.len(), 32, "{}", session.session_id);
    assert!(session.started_at.ends_with('Z'), "{}", session.started_at);
    // Every tab has a server terminal behind it, so the snapshot kind
    // is a promise this session can keep — and the build string is what
    // a client checks its own libghostty pin against. Asserted by shape,
    // not by value: the exact sha is `roost-vt`'s to pin (it has a test
    // for the literal), and repeating it here would make every pin bump
    // a two-crate edit.
    assert_eq!(
        session.payload_kinds,
        vec![AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT)]
    );
    assert!(
        session.libghostty_build.starts_with("ghostty-"),
        "{}",
        session.libghostty_build
    );

    // Hydration seeded one project from the launch directory.
    let seeded = support::tabs(&mut client).await;
    assert_eq!(seeded.len(), 1, "a fresh session opens exactly one tab");
    let project_id = seeded[0].project_id;

    // `tab.dump` with no UI in the process: the answer comes from the
    // tab's own server terminal, at the session's stated geometry.
    let dump = support::tab_dump(&mut client, seeded[0].id).await;
    assert_eq!((dump.cols, dump.rows), (120, 40));
    assert_eq!(dump.rows_text.len(), 40, "a dump covers the whole viewport");

    // And its resolved twin: every cell, through the same color resolver
    // the UIs paint from.
    let resolved = support::tab_dump_resolved(&mut client, seeded[0].id).await;
    assert_eq!((resolved.cols, resolved.rows), (120, 40));
    assert_eq!(resolved.cells.len(), 120 * 40, "a resolved dump is dense");

    // A tab whose shell exits on its own must lose its workspace row
    // without anybody asking — that is the tab task's job, and nothing
    // else in the process is watching the PTY.
    let transient_cwd = layout.subdir("transient");
    let transient = support::open_tab(
        &mut client,
        project_id,
        &transient_cwd,
        "",
        &["/bin/sh", "-c", "exit 0"],
    )
    .await;
    let remaining = support::wait_for_tabs(&mut client, "the exited tab to close itself", |tabs| {
        tabs.iter().all(|tab| tab.id != transient.id)
    })
    .await;
    assert_eq!(remaining.len(), 1, "only the seeded tab should be left");

    // `session.stop` answers with the reap report before the process
    // tail runs.
    let report = support::session_stop(&mut client).await;
    // At least the seeded tab. The transient one may or may not still
    // be in the supervisor's map — its row closed the moment the drain
    // saw the exit, which can lead its wait task by a hair — and either
    // way it is accounted for rather than abandoned.
    let accounted = report.reaped.len() + report.killed.len() + report.abandoned.len();
    assert!(
        accounted >= 1,
        "the seeded tab's shell must be accounted for: {report:?}"
    );
    assert!(
        report.abandoned.is_empty(),
        "nothing should have been abandoned: {report:?}"
    );

    // The tail: the socket is gone, and `serve` returns only once it is.
    served
        .await
        .expect("the serve task must not panic")
        .expect("serve must end cleanly");
    assert!(
        !socket_path.exists(),
        "{} survived the stop",
        socket_path.display()
    );

    // The flush is asserted from the file's contents, not from the
    // stop's return value: the point is that a *later* process reading
    // this path sees the layout.
    let state = support::read_state(&layout.state_path());
    assert_eq!(state.projects.len(), 1);
    let project = &state.projects[0];
    assert_eq!(project.cwd, launch_cwd.to_string_lossy());
    assert_eq!(
        project.tabs.len(),
        1,
        "the exited tab must not be persisted: {:?}",
        project.tabs
    );
    assert_eq!(
        support::canonical(&project.tabs[0].cwd),
        support::canonical(&launch_cwd)
    );
}

/// The locks are released by the same tail that unlinks the socket, so a
/// replacement session can start the moment the first has returned. If
/// they leaked, this would refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_session_releases_its_locks() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();

    let first = layout.spawn(&launch_cwd);
    let mut client = support::connect(&layout.socket_path()).await;
    support::session_stop(&mut client).await;
    first.await.expect("join").expect("first run");

    // Would panic inside `Layout::locks` if either flock were still
    // held.
    let second = layout.spawn(&launch_cwd);
    let mut client = support::connect(&layout.socket_path()).await;
    support::session_stop(&mut client).await;
    tokio::time::timeout(support::scaled(Duration::from_secs(30)), second)
        .await
        .expect("the second session must stop within its budget")
        .expect("join")
        .expect("second run");
}
