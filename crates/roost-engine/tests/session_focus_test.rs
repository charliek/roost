//! `session.set_focus` — what a session mutes, and for whom — plus the
//! `session.set_agent_hooks` install backend's non-authority answers.
//!
//! Driven through the [`Handler`] trait rather than a socket: a focus is
//! a statement *by a connection*, and connection identity is exactly what
//! the trait carries and a socket harness would have to fake.

use std::sync::Arc;

use roost_engine::ipc::{
    AgentHooksError, AgentHooksHandle, AgentHooksRequest, IpcHandler, SessionInfo, StopHandle,
};
use roost_engine::{AttentionSource, PtySupervisor, Workspace};
use roost_ipc::messages::{ops, SessionConnectResult, SessionSetAgentHooksResult};
use roost_ipc::{ConnAction, ConnCloseWatch, ConnCtx, Handler, HandlerOutcome, PushSource};
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

async fn connect(f: &Fixture, c: &Conn) -> Result<SessionConnectResult, String> {
    match f
        .handler
        .handle(&c.ctx, ops::SESSION_CONNECT, serde_json::json!({}))
        .await
    {
        Ok(outcome) => Ok(serde_json::from_value(reply(outcome)).expect("typed connect result")),
        Err(e) => Err(e.code),
    }
}

/// Subscribe and keep the push source alive: dropping it would end the
/// relay, and a dead connection is pruned out of the registry — which is
/// the opposite of what the cases below are checking.
async fn subscribe(f: &Fixture, c: &Conn, lease: &str) -> PushSource {
    match f
        .handler
        .handle(
            &c.ctx,
            ops::EVENTS_SUBSCRIBE,
            serde_json::json!({"lease": lease}),
        )
        .await
    {
        Ok(HandlerOutcome::ReplyThen {
            then: ConnAction::StartPush(source),
            ..
        }) => source,
        Ok(other) => panic!("subscribe must start a push: {:?}", reply(other)),
        Err(e) => panic!("subscribe was refused: {}", e.code),
    }
}

/// One tab in a fresh project.
fn a_tab(f: &Fixture) -> i64 {
    let project = f.workspace.create_project("p", "/tmp").unwrap().id;
    f.workspace.open_tab(project, "/tmp", "sh").unwrap().id
}

/// Another tab beside `tab`, in the same project.
fn sibling_tab(f: &Fixture, tab: i64) -> i64 {
    let project = f.workspace.tab(tab).unwrap().project_id;
    f.workspace.open_tab(project, "/tmp", "sh").unwrap().id
}

async fn set_focus(f: &Fixture, c: &Conn, lease: &str, tab: Option<i64>) -> Result<(), String> {
    let params = serde_json::json!({
        "lease": lease,
        "focused_tab_id": tab.map(|id| id.to_string()),
    });
    match f
        .handler
        .handle(&c.ctx, ops::SESSION_SET_FOCUS, params)
        .await
    {
        Ok(outcome) => {
            assert_eq!(reply(outcome), serde_json::json!({}));
            Ok(())
        }
        Err(e) => {
            assert!(
                !e.message.contains(lease) || lease.is_empty(),
                "a lease is a credential and must not be echoed back: {}",
                e.message
            );
            Err(e.code)
        }
    }
}

/// Whether a structured notification for `tab` gets through — the only
/// externally visible reading of the session's focus state.
fn attention_fires(f: &Fixture, tab: i64) -> bool {
    f.workspace
        .raise_attention(tab, "Roost", "body", AttentionSource::Structured)
        .expect("the tab exists")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_focus_needs_the_lease_and_a_tab_that_exists() {
    let f = fixture();
    let tab = a_tab(&f);
    let holder = conn(1);

    assert_eq!(
        set_focus(&f, &conn(2), &"0".repeat(32), Some(tab))
            .await
            .unwrap_err(),
        "connect-required",
    );

    let lease = connect(&f, &holder).await.expect("connect").lease;
    assert_eq!(
        set_focus(&f, &holder, &lease, Some(tab + 999))
            .await
            .unwrap_err(),
        "not-found"
    );
    set_focus(&f, &holder, &lease, Some(tab))
        .await
        .expect("the client states what it is looking at");
    assert!(!attention_fires(&f, tab), "the viewed tab is suppressed");
}

/// Two clients looking at two tabs mute both. Symmetry's load-bearing
/// half: the second claim may not un-mute the first's tab, and each
/// connection's claim ends only with that connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_connections_viewing_different_tabs_mute_both() {
    let f = fixture();
    let first = a_tab(&f);
    let second = sibling_tab(&f, first);
    let loud = sibling_tab(&f, first);
    let a = conn(1);
    let b = conn(2);
    let lease = connect(&f, &a).await.expect("connect").lease;

    set_focus(&f, &a, &lease, Some(first)).await.expect("focus");
    set_focus(&f, &b, &lease, Some(second))
        .await
        .expect("focus");
    assert!(!attention_fires(&f, first));
    assert!(!attention_fires(&f, second));
    assert!(
        attention_fires(&f, loud),
        "a tab nobody is looking at still raises"
    );

    // A withdrawal is only the withdrawing connection's.
    set_focus(&f, &a, &lease, None).await.expect("nothing here");
    assert!(attention_fires(&f, first));
    assert!(!attention_fires(&f, second));

    // As is a close.
    set_focus(&f, &a, &lease, Some(first)).await.expect("focus");
    assert!(!attention_fires(&f, first));
    f.handler.connection_ended(a.ctx.conn_id);
    assert!(attention_fires(&f, first));
    assert!(!attention_fires(&f, second));

    // And a refused claim applies nothing at all.
    let c = conn(3);
    assert_eq!(
        set_focus(&f, &c, &lease, Some(second + 999))
            .await
            .unwrap_err(),
        "not-found"
    );
    assert!(attention_fires(&f, first));
    assert!(!attention_fires(&f, second));
}

/// A focus is a statement about one window, so it ends with the
/// connection that made it — and with nothing else. The selection never
/// enters into it: `session.set_focus` does not move one, and a
/// reconnect must find the tab the departed client left selected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_focus_ends_with_its_connection_and_the_selection_never_moves() {
    let f = fixture();
    let first = a_tab(&f);
    let second = sibling_tab(&f, first);
    f.workspace.focus_tab(first).unwrap();
    let control = conn(1);
    let lease = connect(&f, &control).await.expect("connect").lease;
    let stream = conn(2);
    let _push = subscribe(&f, &stream, &lease).await;
    set_focus(&f, &control, &lease, Some(second))
        .await
        .expect("focus");
    assert_eq!(
        f.workspace.active().1,
        first,
        "stating a focus is not selecting a tab"
    );

    // The subscriber going away is not the connection that stated the
    // focus: the client is still there, still looking.
    f.handler.connection_ended(stream.ctx.conn_id);
    assert!(!attention_fires(&f, second));

    f.handler.connection_ended(control.ctx.conn_id);
    assert!(attention_fires(&f, second), "nobody is looking any more");
    assert_eq!(
        f.workspace.active().1,
        first,
        "the selection stays where it was"
    );

    // And a client coming back re-states it, which is what a
    // reconnecting UI does the moment it reaches Connected.
    let back = conn(3);
    set_focus(&f, &back, &lease, Some(second))
        .await
        .expect("the same lease still works");
    assert!(!attention_fires(&f, second));
}

/// And it must not depend on the order the server notices things. A
/// client re-dialing can register before the departed one's close is
/// processed; counting live connections alone would then leave the gone
/// client's focus standing, which is the mute all over again. The focus
/// is retired by its *author* going away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_focus_dies_with_its_author_even_when_someone_registered_first() {
    let f = fixture();
    let tab = a_tab(&f);
    let author = conn(1);
    let lease = connect(&f, &author).await.expect("connect").lease;
    set_focus(&f, &author, &lease, Some(tab))
        .await
        .expect("focus");
    assert!(!attention_fires(&f, tab));

    let late = conn(2);
    let _push = subscribe(&f, &late, &lease).await;
    f.handler.connection_ended(author.ctx.conn_id);

    assert!(
        attention_fires(&f, tab),
        "the connection that stated the focus is gone"
    );
}

/// A null focus is nobody's claim: the connection that sent it closing
/// leaves nothing to retire, and the tab is already raising.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_null_focus_leaves_no_claim_behind() {
    let f = fixture();
    let tab = a_tab(&f);
    let holder = conn(1);
    let lease = connect(&f, &holder).await.expect("connect").lease;
    set_focus(&f, &holder, &lease, Some(tab))
        .await
        .expect("focus");
    set_focus(&f, &holder, &lease, None)
        .await
        .expect("and then nothing");

    let other = conn(2);
    let _push = subscribe(&f, &other, &lease).await;
    f.handler.connection_ended(holder.ctx.conn_id);
    assert!(attention_fires(&f, tab));
}

/// A connection ending on a socket with no session touches nothing — the
/// hook is a session's business alone, and a UI reports its own focus
/// through `set_window_focused`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ui_sockets_connection_ending_changes_nothing() {
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
    let tab = workspace.open_tab(project, "/tmp", "sh").unwrap().id;
    workspace.focus_tab(tab).unwrap();

    handler.connection_ended(1);

    assert!(
        !workspace
            .raise_attention(tab, "Roost", "body", AttentionSource::Structured)
            .unwrap(),
        "a UI's own focus is the UI's to report, not this hook's to clear"
    );
}

// ---------------------------------------------------------------------------
// session.set_agent_hooks — what the install backend's answers become
// ---------------------------------------------------------------------------

async fn set_agent_hooks(
    f: &Fixture,
    c: &Conn,
    lease: &str,
    mode: &str,
) -> Result<SessionSetAgentHooksResult, String> {
    let params = serde_json::json!({
        "lease": lease,
        "mode": mode,
        "skip": ["cursor"],
        "client": "charlie-mbp",
    });
    match f
        .handler
        .handle(&c.ctx, ops::SESSION_SET_AGENT_HOOKS, params)
        .await
    {
        Ok(outcome) => Ok(serde_json::from_value(reply(outcome)).expect("typed result")),
        Err(e) => {
            assert!(
                !e.message.contains(lease) || lease.is_empty(),
                "a lease is a credential and must not be echoed back: {}",
                e.message
            );
            Err(e.code)
        }
    }
}

/// A session built without an install backend answers honestly rather
/// than reporting an empty success — the same posture a UI socket takes
/// by not serving `session.*` at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_without_an_install_backend_says_not_supported() {
    let f = fixture();
    let holder = conn(1);
    let lease = connect(&f, &holder).await.expect("connect").lease;
    assert_eq!(
        set_agent_hooks(&f, &holder, &lease, "auto").await,
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
    let holder = conn(1);
    let lease = connect(&f, &holder).await.expect("connect").lease;
    assert_eq!(
        set_agent_hooks(&f, &holder, &lease, "auto").await,
        Err("internal".into())
    );
}
