//! What a session does with a notification — which since #474 is: fire
//! it, every time, and let each client answer for its own screen — plus
//! the `session.set_agent_hooks` install backend's non-authority
//! answers.
//!
//! Driven through the [`Handler`] trait rather than a socket, because
//! connection identity is what the trait carries and a socket harness
//! would have to fake it.

use std::sync::Arc;

use roost_engine::ipc::{
    AgentHooksError, AgentHooksHandle, AgentHooksRequest, IpcHandler, SessionInfo, StopHandle,
};
use roost_engine::{AttentionSource, PtySupervisor, Workspace, WorkspaceEvent};
use roost_ipc::messages::{ops, AgentHooksOutcome};
use roost_ipc::{ConnCloseWatch, ConnCtx, Handler, HandlerOutcome};
use tempfile::TempDir;

struct Fixture {
    handler: IpcHandler,
    workspace: Arc<Workspace>,
    _dir: TempDir,
}

fn fixture() -> Fixture {
    fixture_with(None)
}

fn fixture_with(agent_hooks: Option<AgentHooksHandle>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(Workspace::open(dir.path().join("state.json")));
    let mut handler = IpcHandler::new(
        Arc::clone(&workspace),
        Arc::new(PtySupervisor::new()),
        dir.path().join("roost.sock"),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    )
    .with_session(
        SessionInfo {
            session_id: "01K3S8TQ4F0Q9YB2K6WZ5D7XN".into(),
            started_at: "2026-08-27T14:03:11Z".into(),
            app_version: "9.9.9".into(),
            payload_kinds: Vec::new(),
            libghostty_build: String::new(),
            default_tab_size: (120, 40),
            test_mode: false,
        },
        StopHandle::new(|| async {}),
    );
    if let Some(handle) = agent_hooks {
        handler = handler.with_agent_hooks(handle);
    }
    Fixture {
        handler,
        workspace,
        _dir: dir,
    }
}

/// One client's connection: the context the handler sees plus the watch
/// its connection task would be selecting on.
struct Conn {
    ctx: ConnCtx,
    /// Held, never read: dropping it marks the connection closed, and a
    /// closed peer is pruned out of the registry on the next walk.
    _watch: ConnCloseWatch,
}

fn conn(id: u64) -> Conn {
    let (ctx, _watch) = ConnCtx::new(id);
    Conn { ctx, _watch }
}

fn reply(outcome: HandlerOutcome) -> serde_json::Value {
    match outcome {
        HandlerOutcome::Reply(value) => value,
        HandlerOutcome::ReplyThen { reply, .. } => reply,
    }
}

/// One tab in a fresh project.
fn a_tab(f: &Fixture) -> i64 {
    let project = f.workspace.create_project("p", "/tmp").unwrap().id;
    f.workspace
        .open_tab(project, "/tmp", "sh", true)
        .unwrap()
        .id
}

/// Another tab beside `tab`, in the same project.
fn sibling_tab(f: &Fixture, tab: i64) -> i64 {
    let project = f.workspace.tab(tab).unwrap().project_id;
    f.workspace
        .open_tab(project, "/tmp", "sh", true)
        .unwrap()
        .id
}

/// Whether a structured notification for `tab` gets through.
fn attention_fires(f: &Fixture, tab: i64) -> bool {
    f.workspace
        .raise_attention(tab, "Roost", "body", AttentionSource::Structured)
        .expect("the tab exists")
}

/// Raise on `tab` and return the generation the wire carried, which is
/// all an acknowledging client ever knows.
fn fire(f: &Fixture, tab: i64) -> u64 {
    let mut rx = f.workspace.subscribe();
    assert!(attention_fires(f, tab), "a session suppresses nothing");
    std::iter::from_fn(|| rx.try_recv().ok())
        .find_map(|event| match event {
            WorkspaceEvent::NotificationFired {
                tab_id, generation, ..
            } if tab_id == tab => Some(generation),
            _ => None,
        })
        .expect("a raise that returned true fired an event")
}

/// `tab.clear_notification` through the op, answering `cleared`.
async fn clear(f: &Fixture, c: &Conn, tab: i64, generation: Option<u64>) -> bool {
    let mut params = serde_json::json!({"tab_id": tab.to_string()});
    if let Some(generation) = generation {
        params["generation"] = generation.into();
    }
    let answer = reply(
        f.handler
            .handle(&c.ctx, ops::TAB_CLEAR_NOTIFICATION, params)
            .await
            .expect("a clear on a live tab"),
    );
    answer["cleared"]
        .as_bool()
        .unwrap_or_else(|| panic!("the reply must carry `cleared`: {answer}"))
}

/// The op a session used to be told what to mute with is gone, and with
/// it the muting (#474). A session has no window, so the tab its
/// headless workspace happens to have selected raises like any other —
/// which is what stops a phone left open on a tab from silencing a
/// laptop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_takes_no_ones_focus_and_suppresses_nothing() {
    let f = fixture();
    let tab = a_tab(&f);
    let sibling = sibling_tab(&f, tab);
    f.workspace.focus_tab(tab).unwrap();
    let viewer = conn(1);

    // Spelled out rather than taken from a constant: the constant is
    // deleted, and what this pins is the name on the wire.
    let refused = f
        .handler
        .handle(
            &viewer.ctx,
            "session.set_focus",
            serde_json::json!({"focused_tab_id": tab.to_string()}),
        )
        .await
        .expect_err("session.set_focus is not served any more");
    assert_eq!(refused.code, "unknown-op");

    assert!(attention_fires(&f, tab), "even the selected tab");
    assert!(attention_fires(&f, sibling));
}

/// The acknowledgement, in the shape the viewing client sends it: it
/// names the raise it read, and the session agrees it is current.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_acknowledgement_that_names_the_current_raise_clears_it() {
    let f = fixture();
    let tab = a_tab(&f);
    let viewer = conn(1);

    let generation = fire(&f, tab);
    assert!(clear(&f, &viewer, tab, Some(generation)).await);
    assert!(!f.workspace.tab(tab).unwrap().has_notification);

    // A second client looking at the same tab acknowledges the same
    // raise. It is current too, so this is not a refusal — it is simply
    // told it arrived second.
    let second = conn(2);
    assert!(!clear(&f, &second, tab, Some(generation)).await);
}

/// The race the generation exists for: A fires, the viewing client
/// acknowledges A, B fires, and A's acknowledgement lands afterwards.
/// B must survive it — nobody has seen B.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_acknowledgement_cannot_erase_the_raise_that_replaced_it() {
    let f = fixture();
    let tab = a_tab(&f);
    let viewer = conn(1);

    let stale = fire(&f, tab);
    assert!(clear(&f, &viewer, tab, Some(stale)).await);
    let current = fire(&f, tab);
    assert_ne!(current, stale, "a second raise mints a second generation");

    assert!(!clear(&f, &viewer, tab, Some(stale)).await);
    assert!(
        f.workspace.tab(tab).unwrap().has_notification,
        "the notification nobody has seen is still standing"
    );
    assert!(clear(&f, &viewer, tab, Some(current)).await);
}

/// Omitting the generation is the other caller — a click, `roostctl` —
/// and is unconditional, which is what it has always been.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clear_with_no_generation_answers_the_whole_tab() {
    let f = fixture();
    let tab = a_tab(&f);
    let clicking = conn(1);

    fire(&f, tab);
    fire(&f, tab);
    assert!(clear(&f, &clicking, tab, None).await);
    assert!(!f.workspace.tab(tab).unwrap().has_notification);
}

/// A connection ending touches no notification state on either kind of
/// socket. It used to retire that connection's focus; there is no such
/// thing now, and a UI's own focus is the UI's to report through
/// `set_window_focused`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_ending_retires_no_ones_attention() {
    let f = fixture();
    let tab = a_tab(&f);
    let gone = conn(1);
    fire(&f, tab);

    f.handler.connection_ended(gone.ctx.conn_id);
    assert!(
        f.workspace.tab(tab).unwrap().has_notification,
        "a departing client does not answer the tab on everyone's behalf"
    );

    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(Workspace::open(dir.path().join("state.json")));
    workspace.set_window_focused(true);
    let handler = IpcHandler::new(
        Arc::clone(&workspace),
        Arc::new(PtySupervisor::new()),
        dir.path().join("roost.sock"),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );
    let project = workspace.create_project("p", "/tmp").unwrap().id;
    let ui_tab = workspace.open_tab(project, "/tmp", "sh", true).unwrap().id;
    workspace.focus_tab(ui_tab).unwrap();

    handler.connection_ended(1);

    assert!(
        !workspace
            .raise_attention(ui_tab, "Roost", "body", AttentionSource::Structured)
            .unwrap(),
        "a UI's own focus is the UI's to report, not this hook's to clear"
    );
}

// ---------------------------------------------------------------------------
// session.set_agent_hooks — what the install backend's answers become
// ---------------------------------------------------------------------------

async fn set_agent_hooks(f: &Fixture, c: &Conn) -> Result<AgentHooksOutcome, String> {
    let params = serde_json::json!({
        "agents": ["claude"],
        "client": "charlie-mbp",
    });
    match f
        .handler
        .handle(&c.ctx, ops::SESSION_SET_AGENT_HOOKS, params)
        .await
    {
        Ok(outcome) => Ok(serde_json::from_value(reply(outcome)).expect("typed result")),
        Err(e) => Err(e.code),
    }
}

/// A session built without an install backend answers honestly rather
/// than reporting an empty success — the same posture a UI socket takes
/// by not serving `session.*` at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_without_an_install_backend_says_not_supported() {
    let f = fixture();
    let asking = conn(1);
    assert_eq!(
        set_agent_hooks(&f, &asking).await,
        Err("not-supported".into())
    );
}

/// The backend's own failure is an error frame with a code the client can
/// act on, not a panic and not a silent empty reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_install_backend_surfaces_as_internal() {
    let handle = AgentHooksHandle::new(|_: AgentHooksRequest| async {
        Err(AgentHooksError::Failed(
            "no HOME in this session's environment".to_string(),
        ))
    });
    let f = fixture_with(Some(handle));
    let asking = conn(1);
    assert_eq!(set_agent_hooks(&f, &asking).await, Err("internal".into()));
}
