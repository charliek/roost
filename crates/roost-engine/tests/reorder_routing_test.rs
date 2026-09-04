//! Where a `tab.reorder` / `project.reorder` request goes (plan 044
//! §3.1 d6).
//!
//! The op set is one contract for both instances: the same two ops
//! reorder the local workspace and a connected host's session, and the
//! refs decide which. What this pins is the decision — every row of the
//! routing matrix, the payload the host route hands the app, and the
//! three refusals (mixed refs, no UI, a session socket).
//!
//! Driven through the [`Handler`] trait rather than a socket: the
//! framing is `roost-ipc`'s to pin, and a direct call is what lets a
//! test stand in for the app's main thread synchronously.

use std::sync::Arc;

use roost_engine::ipc::{IpcHandler, SessionInfo, StopHandle, UiRequest};
use roost_engine::{PtySupervisor, Workspace};
use roost_ipc::messages::ops;
use roost_ipc::{ConnCtx, Handler, HandlerError, HandlerOutcome};
use tokio::sync::mpsc::UnboundedReceiver;

/// What the app's main thread was asked to do, flattened for assertion.
#[derive(Debug, PartialEq, Eq)]
enum Routed {
    Tabs {
        host: u32,
        project_id: i64,
        tab_ids: Vec<i64>,
    },
    Projects {
        host: u32,
        project_ids: Vec<i64>,
    },
}

struct Fixture {
    handler: IpcHandler,
    workspace: Arc<Workspace>,
    ui_rx: Option<UnboundedReceiver<UiRequest>>,
    _dir: tempfile::TempDir,
}

#[derive(Clone, Copy)]
enum Socket {
    /// A UI socket with an app behind it — the only shape that routes.
    Ui,
    /// A UI socket with nothing driving it (the headless engine).
    Headless,
    /// The host-session daemon's socket.
    Session,
}

fn fixture(socket: Socket) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(Workspace::open(dir.path().join("state.json")));
    let mut handler = IpcHandler::new(
        workspace.clone(),
        Arc::new(PtySupervisor::new()),
        dir.path().join("roost.sock"),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );
    let mut ui_rx = None;
    match socket {
        Socket::Ui => {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            handler = handler.with_ui(tx);
            ui_rx = Some(rx);
        }
        Socket::Headless => {}
        Socket::Session => {
            handler = handler.with_session(
                SessionInfo {
                    session_id: "01K3S8TQ4F0Q9YB2K6WZ5D7XN".into(),
                    started_at: "2026-09-04T00:00:00Z".into(),
                    app_version: "9.9.9".into(),
                    payload_kinds: Vec::new(),
                    libghostty_build: String::new(),
                    default_tab_size: (120, 40),
                    test_mode: false,
                },
                StopHandle::new(|| async {}),
            );
        }
    }
    Fixture {
        handler,
        workspace,
        ui_rx,
        _dir: dir,
    }
}

async fn call(
    f: &Fixture,
    op: &str,
    params: serde_json::Value,
) -> Result<HandlerOutcome, HandlerError> {
    let (ctx, _close) = ConnCtx::new(1);
    f.handler.handle(&ctx, op, params).await
}

/// Serve one host-routed request the way the app does, with the given
/// answer, and report what it was asked for.
///
/// Spawned rather than awaited in line: the dispatcher blocks on the
/// oneshot, so the reply has to come from somewhere else.
fn stand_in_for_the_app(
    mut ui_rx: UnboundedReceiver<UiRequest>,
    answer: Result<(), roost_engine::ipc::HostOpFailure>,
) -> tokio::task::JoinHandle<Routed> {
    tokio::spawn(async move {
        match ui_rx.recv().await.expect("the app was never asked") {
            UiRequest::HostTabReorder {
                host,
                project_id,
                tab_ids,
                reply,
            } => {
                let _ = reply.send(answer);
                Routed::Tabs {
                    host,
                    project_id,
                    tab_ids,
                }
            }
            UiRequest::HostProjectReorder {
                host,
                project_ids,
                reply,
            } => {
                let _ = reply.send(answer);
                Routed::Projects { host, project_ids }
            }
            _ => panic!("a reorder woke some other UI request"),
        }
    })
}

fn server_error(error: HandlerError) -> (String, String) {
    (error.code, error.message)
}

/// One project with two tabs, so a local reorder has something to move
/// and a host reorder has something to leave alone.
fn seed(workspace: &Workspace) -> (i64, Vec<i64>) {
    let project = workspace.create_project("one", "/tmp").unwrap();
    let a = workspace.open_tab(project.id, "/tmp", "").unwrap();
    let b = workspace.open_tab(project.id, "/tmp", "").unwrap();
    (project.id, vec![a.id, b.id])
}

fn tab_order(workspace: &Workspace, project_id: i64) -> Vec<i64> {
    workspace
        .snapshot()
        .into_iter()
        .find(|p| p.id == project_id)
        .expect("project")
        .tabs
        .iter()
        .map(|t| t.id)
        .collect()
}

/// All-bare refs keep the workspace path they have always had — the
/// empty list included, which names no instance and so cannot route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_refs_reorder_the_local_workspace() {
    let f = fixture(Socket::Ui);
    let (project_id, tabs) = seed(&f.workspace);
    let reversed: Vec<String> = tabs.iter().rev().map(i64::to_string).collect();

    call(
        &f,
        ops::TAB_REORDER,
        serde_json::json!({"project_id": project_id.to_string(), "tab_ids": reversed}),
    )
    .await
    .expect("a bare tab.reorder");
    assert_eq!(
        tab_order(&f.workspace, project_id),
        tabs.iter().rev().copied().collect::<Vec<_>>()
    );

    // An empty list on either op is the local no-op it has always been.
    call(
        &f,
        ops::TAB_REORDER,
        serde_json::json!({"project_id": project_id.to_string(), "tab_ids": []}),
    )
    .await
    .expect("an empty bare tab.reorder");
    call(
        &f,
        ops::PROJECT_REORDER,
        serde_json::json!({"project_ids": []}),
    )
    .await
    .expect("an empty project.reorder");
    assert_eq!(
        tab_order(&f.workspace, project_id),
        tabs.iter().rev().copied().collect::<Vec<_>>()
    );
    assert!(
        f.ui_rx.unwrap().try_recv().is_err(),
        "a local reorder must never wake the app's host path"
    );
}

/// A `Host` project with tabs on the same incarnation routes to the app,
/// which gets the ids already narrowed to the session's bare id-space.
/// The local workspace is not touched (AC2 at the engine seam).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_qualified_refs_route_to_the_app_and_leave_the_workspace_alone() {
    let mut f = fixture(Socket::Ui);
    let (project_id, tabs) = seed(&f.workspace);
    let app = stand_in_for_the_app(f.ui_rx.take().unwrap(), Ok(()));

    call(
        &f,
        ops::TAB_REORDER,
        serde_json::json!({"project_id": "h3.4", "tab_ids": ["h3.9", "h3.7"]}),
    )
    .await
    .expect("a host tab.reorder");
    assert_eq!(
        app.await.unwrap(),
        Routed::Tabs {
            host: 3,
            project_id: 4,
            tab_ids: vec![9, 7],
        }
    );
    assert_eq!(
        tab_order(&f.workspace, project_id),
        tabs,
        "a host reorder must not mutate the local workspace"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_qualified_projects_route_to_the_app() {
    let mut f = fixture(Socket::Ui);
    let app = stand_in_for_the_app(f.ui_rx.take().unwrap(), Ok(()));

    call(
        &f,
        ops::PROJECT_REORDER,
        serde_json::json!({"project_ids": ["h2.5", "h2.1"]}),
    )
    .await
    .expect("a host project.reorder");
    assert_eq!(
        app.await.unwrap(),
        Routed::Projects {
            host: 2,
            project_ids: vec![5, 1],
        }
    );
}

/// An empty `tab_ids` under a `Host` project still names an instance, so
/// it routes — the session's own partial-order rules decide what an
/// empty order means, exactly as they do locally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_project_with_no_tabs_still_routes() {
    let mut f = fixture(Socket::Ui);
    let app = stand_in_for_the_app(f.ui_rx.take().unwrap(), Ok(()));

    call(
        &f,
        ops::TAB_REORDER,
        serde_json::json!({"project_id": "h3.4", "tab_ids": []}),
    )
    .await
    .expect("a host tab.reorder with no ids");
    assert_eq!(
        app.await.unwrap(),
        Routed::Tabs {
            host: 3,
            project_id: 4,
            tab_ids: vec![],
        }
    );
}

/// The session's own refusal crosses the seam intact: its code, not a
/// flattened `internal`. This is what lets a caller match on the same
/// code whether the local engine or a host answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sessions_own_wire_code_survives() {
    let mut f = fixture(Socket::Ui);
    let app = stand_in_for_the_app(
        f.ui_rx.take().unwrap(),
        Err(roost_engine::ipc::HostOpFailure::new(
            "invalid-param",
            "tab 9 is not in project 4",
        )),
    );

    let error = call(
        &f,
        ops::TAB_REORDER,
        serde_json::json!({"project_id": "h3.4", "tab_ids": ["h3.9"]}),
    )
    .await
    .expect_err("the session refused it");
    assert_eq!(
        server_error(error),
        (
            "invalid-param".to_string(),
            "tab 9 is not in project 4".to_string()
        )
    );
    app.await.unwrap();
}

/// Every mixed form, refused by name. A list half in a host's numbering
/// and half in ours would reorder something nobody asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_refs_are_refused() {
    let f = fixture(Socket::Ui);
    for (op, params) in [
        // Two different incarnations.
        (
            ops::TAB_REORDER,
            serde_json::json!({"project_id": "h3.4", "tab_ids": ["h3.9", "h4.7"]}),
        ),
        // A host project with a bare tab.
        (
            ops::TAB_REORDER,
            serde_json::json!({"project_id": "h3.4", "tab_ids": ["9"]}),
        ),
        // A bare project with a qualified tab.
        (
            ops::TAB_REORDER,
            serde_json::json!({"project_id": "1", "tab_ids": ["h3.9"]}),
        ),
        (
            ops::PROJECT_REORDER,
            serde_json::json!({"project_ids": ["h3.4", "h4.5"]}),
        ),
        // A bare project among qualified ones, in either position.
        (
            ops::PROJECT_REORDER,
            serde_json::json!({"project_ids": ["h3.4", "5"]}),
        ),
        (
            ops::PROJECT_REORDER,
            serde_json::json!({"project_ids": ["5", "h3.4"]}),
        ),
    ] {
        let (code, message) = server_error(
            call(&f, op, params.clone())
                .await
                .expect_err("a mixed request must be refused"),
        );
        assert_eq!(code, "invalid-param", "{op} {params}");
        assert!(
            message.contains("must name the same instance"),
            "the refusal has to name the rule: {message}"
        );
    }
    assert!(
        f.ui_rx.unwrap().try_recv().is_err(),
        "a refused request must never reach the app"
    );
}

/// The host form on a socket with no UI: there is no connection set to
/// route through, and inventing one would answer for a host this
/// process is not talking to. The `tab.focus` precedent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_form_needs_a_ui() {
    let f = fixture(Socket::Headless);
    for (op, params) in [
        (
            ops::TAB_REORDER,
            serde_json::json!({"project_id": "h3.4", "tab_ids": ["h3.9"]}),
        ),
        (
            ops::PROJECT_REORDER,
            serde_json::json!({"project_ids": ["h3.4"]}),
        ),
    ] {
        let (code, message) = server_error(
            call(&f, op, params)
                .await
                .expect_err("no UI, no host route"),
        );
        assert_eq!(code, "invalid-param", "{op}");
        assert!(message.contains("needs a UI"), "{op}: {message}");
    }
    // The bare form is unaffected: it never needed a UI.
    let (project_id, tabs) = seed(&f.workspace);
    call(
        &f,
        ops::TAB_REORDER,
        serde_json::json!({
            "project_id": project_id.to_string(),
            "tab_ids": tabs.iter().rev().map(i64::to_string).collect::<Vec<_>>(),
        }),
    )
    .await
    .expect("a bare reorder needs no UI");
}

/// A session's ids are one bare id-space by design; the qualified
/// spelling names a UI's client-side row and is refused there rather
/// than narrowed to some unrelated number — the rule `tab.dump` applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_socket_refuses_qualified_refs() {
    let f = fixture(Socket::Session);
    for (op, params) in [
        (
            ops::TAB_REORDER,
            serde_json::json!({"project_id": "h3.4", "tab_ids": ["h3.9"]}),
        ),
        (
            ops::TAB_REORDER,
            serde_json::json!({"project_id": "1", "tab_ids": ["h3.9"]}),
        ),
        (
            ops::PROJECT_REORDER,
            serde_json::json!({"project_ids": ["h3.4"]}),
        ),
    ] {
        let (code, message) = server_error(
            call(&f, op, params)
                .await
                .expect_err("a session socket must refuse a qualified ref"),
        );
        assert_eq!(code, "invalid-param", "{op}");
        assert!(
            message.contains("UI-socket form"),
            "the refusal names the id-space: {message}"
        );
    }

    // Bare traffic on a session socket is untouched.
    let (project_id, tabs) = seed(&f.workspace);
    call(
        &f,
        ops::TAB_REORDER,
        serde_json::json!({
            "project_id": project_id.to_string(),
            "tab_ids": tabs.iter().rev().map(i64::to_string).collect::<Vec<_>>(),
        }),
    )
    .await
    .expect("a bare reorder on a session socket");
    assert_eq!(
        tab_order(&f.workspace, project_id),
        tabs.iter().rev().copied().collect::<Vec<_>>()
    );
}

/// The seam itself is a conduit: whatever code the app puts in a
/// `HostOpFailure` is the code on the wire, unbounded.
///
/// That is deliberate, and it is why the *app* is where session-only
/// codes are folded (`host_op_failure` in `roost-iced`'s
/// `app/servicing.rs`, fenced by
/// `a_host_failure_reports_only_codes_a_ui_socket_speaks`): one guard,
/// at the one producer, rather than a second copy of the table here
/// that would drift from it. What this pins is that the engine adds no
/// second opinion — a code the app decided on arrives unaltered, which
/// is what makes the app's table the whole of the policy.
///
/// `shutting-down` is the case worth naming: both reorder ops are in a
/// session's mutating set, so a latched session really does answer it,
/// and `docs/reference/ipc.md` marks it session-socket-only. The app
/// never lets it get this far.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_engine_reports_whatever_code_the_app_decided_on() {
    let mut f = fixture(Socket::Ui);
    let app = stand_in_for_the_app(
        f.ui_rx.take().unwrap(),
        Err(roost_engine::ipc::HostOpFailure::new(
            "host-unavailable",
            "shutting-down: session is shutting down",
        )),
    );

    let error = call(
        &f,
        ops::TAB_REORDER,
        serde_json::json!({"project_id": "h3.4", "tab_ids": ["h3.9"]}),
    )
    .await
    .expect_err("the app refused it");
    assert_eq!(
        server_error(error),
        (
            "host-unavailable".to_string(),
            "shutting-down: session is shutting down".to_string()
        ),
        "the folded code and the session's own words both reach the wire"
    );
    app.await.unwrap();
}
