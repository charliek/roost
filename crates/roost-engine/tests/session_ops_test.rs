//! `session.identify` / `session.stop` and the stop latch.
//!
//! The dispatcher is driven directly through the [`Handler`] trait: the
//! reply-before-finalizer ordering is a server property and is pinned in
//! `roost-ipc`'s `server_seam_test.rs`, so what matters here is which
//! ops answer, with what, and in which order the shutdown steps run.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use roost_engine::ipc::{IpcHandler, SessionInfo, StopHandle};
use roost_engine::{PtyOutputEvent, PtySupervisor, Workspace};
use roost_ipc::messages::{ops, SessionIdentify, SessionStopResult, TabOpenResult};
use roost_ipc::{ConnAction, Handler, HandlerError, HandlerOutcome};
use tempfile::TempDir;

const SESSION_ID: &str = "01K3S8TQ4F0Q9YB2K6WZ5D7XN";
const STARTED_AT: &str = "2026-08-27T14:03:11Z";

struct Fixture {
    handler: IpcHandler,
    workspace: Arc<Workspace>,
    supervisor: Arc<PtySupervisor>,
    /// Set when the `StopHandle` the session was built with is invoked.
    stop_calls: Arc<AtomicUsize>,
    _dir: TempDir,
}

fn session_info() -> SessionInfo {
    SessionInfo {
        session_id: SESSION_ID.into(),
        started_at: STARTED_AT.into(),
        app_version: "9.9.9".into(),
        payload_kinds: Vec::new(),
        libghostty_build: String::new(),
        default_tab_size: (120, 40),
    }
}

fn fixture(with_session: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("roost.sock");
    let workspace = Arc::new(Workspace::open(dir.path().join("state.json")));
    let supervisor = Arc::new(PtySupervisor::new());
    let stop_calls = Arc::new(AtomicUsize::new(0));
    let mut handler = IpcHandler::new(
        workspace.clone(),
        supervisor.clone(),
        socket_path,
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );
    if with_session {
        let calls = stop_calls.clone();
        handler = handler.with_session(
            session_info(),
            StopHandle::new(move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                }
            }),
        );
    }
    Fixture {
        handler,
        workspace,
        supervisor,
        stop_calls,
        _dir: dir,
    }
}

async fn call(
    handler: &IpcHandler,
    op: &str,
    params: serde_json::Value,
) -> Result<HandlerOutcome, HandlerError> {
    handler.handle(op, params).await
}

fn reply(outcome: HandlerOutcome) -> serde_json::Value {
    match outcome {
        HandlerOutcome::Reply(value) => value,
        HandlerOutcome::ReplyThen { reply, .. } => reply,
    }
}

fn tab_open_params(project_id: i64, argv: &[&str], size: Option<(u32, u32)>) -> serde_json::Value {
    let (cols, rows) = size.unwrap_or((0, 0));
    serde_json::json!({
        "project_id": project_id.to_string(),
        "cwd": "/tmp",
        "argv": argv,
        "cols": cols,
        "rows": rows,
        "title": "",
    })
}

/// Every tab the report accounts for, in bucket order. The three lists
/// partition the live set, so a tab appearing twice here is a bug.
fn accounted(report: &SessionStopResult) -> Vec<i64> {
    report
        .reaped
        .iter()
        .chain(&report.killed)
        .chain(&report.abandoned)
        .copied()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ui_socket_does_not_know_the_session_ops() {
    let f = fixture(false);

    for op in [ops::SESSION_IDENTIFY, ops::SESSION_STOP] {
        let err = call(&f.handler, op, serde_json::json!({}))
            .await
            .expect_err("a UI socket must not serve session ops");
        assert_eq!(err.code, "unknown-op", "{op}");
        assert_eq!(err.message, format!("no such op: {op}"), "{op}");
    }

    // And nothing about the UI socket's other answers moved: the size
    // fallback is still 80x24.
    assert_eq!(open_tab_reporting_size(&f, None).await, (80, 24));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_identify_reports_the_installed_identity() {
    let f = fixture(true);
    let value = reply(
        call(&f.handler, ops::SESSION_IDENTIFY, serde_json::json!({}))
            .await
            .expect("session.identify"),
    );
    let identify: SessionIdentify = serde_json::from_value(value).expect("typed decode");

    assert_eq!(identify.session_id, SESSION_ID);
    assert_eq!(identify.started_at, STARTED_AT);
    assert_eq!(identify.app_version, "9.9.9");
    assert_eq!(
        identify.session_protocol,
        roost_ipc::messages::SESSION_PROTOCOL_VERSION
    );
    // Honest "attach unavailable" until HS-1b fills these in.
    assert!(identify.payload_kinds.is_empty());
    assert_eq!(identify.libghostty_build, "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tab_open_without_a_size_uses_the_session_default() {
    let f = fixture(true);
    assert_eq!(open_tab_reporting_size(&f, None).await, (120, 40));
    // An explicit size still wins.
    assert_eq!(open_tab_reporting_size(&f, Some((72, 19))).await, (72, 19));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_stop_reaps_latches_and_finalizes() {
    let f = fixture(true);

    let project = f.workspace.create_project("p", "/tmp").unwrap();
    let value = reply(
        call(
            &f.handler,
            ops::TAB_OPEN,
            tab_open_params(project.id, &["/bin/sh", "-c", "sleep 30"], None),
        )
        .await
        .expect("tab.open"),
    );
    let opened: TabOpenResult = serde_json::from_value(value).unwrap();

    let outcome = call(&f.handler, ops::SESSION_STOP, serde_json::json!({}))
        .await
        .expect("session.stop");
    let HandlerOutcome::ReplyThen { reply, then } = outcome else {
        panic!("session.stop must reply then finalize");
    };
    let report: SessionStopResult = serde_json::from_value(reply).expect("typed decode");
    assert_eq!(
        accounted(&report),
        vec![opened.tab.id],
        "the live tab must appear in exactly one bucket"
    );
    assert!(!f.supervisor.has(opened.tab.id));

    // Post-latch: mutating ops are refused, reads still answer.
    let err = call(
        &f.handler,
        ops::TAB_OPEN,
        tab_open_params(project.id, &["/bin/sh", "-c", "true"], None),
    )
    .await
    .expect_err("tab.open after stop");
    assert_eq!(err.code, "shutting-down");

    let err = call(
        &f.handler,
        ops::PROJECT_CREATE,
        serde_json::json!({"name": "late", "cwd": "/tmp"}),
    )
    .await
    .expect_err("project.create after stop");
    assert_eq!(err.code, "shutting-down");

    // Idempotent-reject: a second stop gets the same answer, and never
    // runs the shutdown steps twice.
    let err = call(&f.handler, ops::SESSION_STOP, serde_json::json!({}))
        .await
        .expect_err("second session.stop");
    assert_eq!(err.code, "shutting-down");

    for op in [ops::TAB_LIST, ops::IDENTIFY, ops::SESSION_IDENTIFY] {
        call(&f.handler, op, serde_json::json!({}))
            .await
            .unwrap_or_else(|e| panic!("read op {op} must still answer, got {e}"));
    }

    // The finalizer is the daemon's tail and only runs when the server
    // hands it back the connection.
    assert_eq!(f.stop_calls.load(Ordering::SeqCst), 0);
    match then {
        ConnAction::FinalizeStop(finalizer) => finalizer.run().await,
        ConnAction::StartPush(_) => panic!("expected FinalizeStop"),
    }
    assert_eq!(f.stop_calls.load(Ordering::SeqCst), 1);
}

/// The barrier's invariant, raced deliberately: a `tab.open` concurrent
/// with `session.stop` either loses the latch and is refused, or wins it
/// and is waited for — in which case its tab is in the report. What must
/// never happen is a tab that spawned and was not accounted for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mutation_racing_stop_is_refused_or_reaped() {
    let f = Arc::new(fixture(true));
    let project = f.workspace.create_project("p", "/tmp").unwrap();

    let opener = {
        let f = f.clone();
        let project_id = project.id;
        tokio::spawn(async move {
            call(
                &f.handler,
                ops::TAB_OPEN,
                tab_open_params(project_id, &["/bin/sh", "-c", "sleep 30"], None),
            )
            .await
        })
    };
    let stopper = {
        let f = f.clone();
        tokio::spawn(
            async move { call(&f.handler, ops::SESSION_STOP, serde_json::json!({})).await },
        )
    };

    let opened = opener.await.unwrap();
    let report: SessionStopResult =
        serde_json::from_value(reply(stopper.await.unwrap().expect("session.stop"))).unwrap();
    let ids = accounted(&report);

    match opened {
        Err(err) => assert_eq!(err.code, "shutting-down"),
        Ok(outcome) => {
            let opened: TabOpenResult = serde_json::from_value(reply(outcome)).unwrap();
            assert!(
                ids.contains(&opened.tab.id),
                "a tab that got past the latch must be reaped, got {ids:?}"
            );
        }
    }
}

/// Open a tab whose shell prints its winsize, and read it back off the
/// PTY. This is the only honest check that the default reached the
/// terminal rather than just the workspace record.
async fn open_tab_reporting_size(f: &Fixture, size: Option<(u32, u32)>) -> (u16, u16) {
    let project = f.workspace.ensure_default_project("/tmp");
    let value = reply(
        call(
            &f.handler,
            ops::TAB_OPEN,
            tab_open_params(project, &["/bin/sh", "-c", "stty size"], size),
        )
        .await
        .expect("tab.open"),
    );
    let opened: TabOpenResult = serde_json::from_value(value).unwrap();

    let mut output = f
        .supervisor
        .take_initial_receiver(opened.tab.id)
        .expect("initial receiver");
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "stty never reported a size");
        match tokio::time::timeout(remaining, output.recv()).await {
            Ok(Ok(PtyOutputEvent::Bytes { data, .. })) => collected.extend_from_slice(&data),
            Ok(Ok(PtyOutputEvent::Exit { .. })) => break,
            Ok(Err(e)) => panic!("output channel: {e}"),
            Err(_) => panic!("stty never reported a size"),
        }
    }
    let text = String::from_utf8_lossy(&collected);
    // `stty size` prints "<rows> <cols>".
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| {
            l.split_whitespace().count() == 2 && l.chars().all(|c| c.is_ascii_digit() || c == ' ')
        })
        .unwrap_or_else(|| panic!("no winsize line in {text:?}"));
    let mut parts = line.split_whitespace();
    let rows: u16 = parts.next().unwrap().parse().unwrap();
    let cols: u16 = parts.next().unwrap().parse().unwrap();
    (cols, rows)
}

/// The latch is what gates; nothing else about the handler is stateful,
/// so a session that never stops behaves exactly like a UI socket for
/// every op other than `session.*`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unstopped_session_serves_mutating_ops_normally() {
    let f = fixture(true);
    let value = reply(
        call(
            &f.handler,
            ops::PROJECT_CREATE,
            serde_json::json!({"name": "p", "cwd": "/tmp"}),
        )
        .await
        .expect("project.create"),
    );
    assert_eq!(value["project"]["name"], "p");
    assert_eq!(f.stop_calls.load(Ordering::SeqCst), 0);
}
