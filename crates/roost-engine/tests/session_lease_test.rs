//! `session.connect`, the client registry, and the lease gate on
//! `events.subscribe`.
//!
//! Driven through the [`Handler`] trait rather than a socket: what these
//! cases are about is *which connection* holds authority and what happens
//! to the others, and connection identity is exactly what the trait
//! carries. The wire-visible half — a closed connection's final labeled
//! frame — is pinned in `events_push_test`, where there is a socket to
//! see it on.

use std::sync::Arc;

use roost_engine::ipc::{
    AgentHooksError, AgentHooksHandle, AgentHooksRequest, IpcHandler, SessionInfo, StopHandle,
};
use roost_engine::{AttentionSource, PtySupervisor, Workspace};
use roost_ipc::messages::{
    ops, AgentHooksMode, AgentHooksSkipped, SessionConnectResult, SessionSetAgentHooksResult,
};
use roost_ipc::{CloseReason, ConnAction, ConnCloseWatch, ConnCtx, Handler, HandlerOutcome};
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
    watch: ConnCloseWatch,
}

fn conn(id: u64) -> Conn {
    let (ctx, watch) = ConnCtx::new(id);
    Conn { ctx, watch }
}

fn reply(outcome: HandlerOutcome) -> serde_json::Value {
    match outcome {
        HandlerOutcome::Reply(value) => value,
        HandlerOutcome::ReplyThen { reply, .. } => reply,
    }
}

async fn connect(f: &Fixture, c: &Conn, takeover: bool) -> Result<SessionConnectResult, String> {
    match f
        .handler
        .handle(
            &c.ctx,
            ops::SESSION_CONNECT,
            serde_json::json!({"takeover": takeover}),
        )
        .await
    {
        Ok(outcome) => Ok(serde_json::from_value(reply(outcome)).expect("typed connect result")),
        Err(e) => Err(e.code),
    }
}

/// Subscribe and keep the push source alive: dropping it would end the
/// relay, and a dead connection is pruned out of the registry — which is
/// the opposite of what most of these cases are checking.
async fn subscribe(f: &Fixture, c: &Conn, lease: &str) -> Result<ConnAction, String> {
    match f
        .handler
        .handle(
            &c.ctx,
            ops::EVENTS_SUBSCRIBE,
            serde_json::json!({"lease": lease}),
        )
        .await
    {
        Ok(HandlerOutcome::ReplyThen { then, .. }) => Ok(then),
        Ok(HandlerOutcome::Reply(value)) => panic!("subscribe must flip to push mode: {value}"),
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

fn is_hex_32(s: &str) -> bool {
    s.len() == 32
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_issues_a_fresh_lease_and_the_workspace_fence() {
    let f = fixture();
    f.workspace.create_project("p", "/tmp").unwrap();

    let a = conn(1);
    let first = connect(&f, &a, false).await.expect("session.connect");
    // Assertions here never format a token: leases are bearer
    // credentials and a failure dump is a log.
    assert!(is_hex_32(&first.lease), "lease is not 32 lowercase hex");
    assert_eq!(first.revision, f.workspace.revision());

    // A second session hands out a different one — the token is minted,
    // never derived from anything a client could predict.
    let g = fixture();
    let second = connect(&g, &conn(1), false).await.expect("session.connect");
    assert!(
        first.lease != second.lease,
        "two sessions minted the same lease"
    );
}

/// Reconnect is always takeover, holder included: a client that lost
/// track of its own lease must re-establish it deliberately rather than
/// have the server guess that the second connect meant the same client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_lease_refuses_a_plain_connect_from_anyone() {
    let f = fixture();
    let holder = conn(1);
    connect(&f, &holder, false).await.expect("session.connect");

    assert_eq!(
        connect(&f, &conn(2), false).await.unwrap_err(),
        "already-connected"
    );
    assert_eq!(
        connect(&f, &holder, false).await.unwrap_err(),
        "already-connected",
        "the holder is refused too — reconnect is always takeover"
    );
}

/// A lease outlives the connection that took it. HS-2's reconnect is a
/// takeover precisely because of this: a dropped socket must not silently
/// hand the session to whoever dials next.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_survives_its_connection_going_away() {
    let f = fixture();
    let gone = conn(1);
    connect(&f, &gone, false).await.expect("session.connect");
    drop(gone);

    assert_eq!(
        connect(&f, &conn(2), false).await.unwrap_err(),
        "already-connected"
    );
    connect(&f, &conn(2), true)
        .await
        .expect("takeover is how a client comes back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_replaces_the_lease_and_tombstones_the_old_one() {
    let f = fixture();
    let first = conn(1);
    let old = connect(&f, &first, false).await.expect("connect").lease;

    let second = conn(2);
    let new = connect(&f, &second, true).await.expect("takeover").lease;
    assert!(old != new, "takeover re-issued the displaced lease");

    // The tombstone is what turns a stale lease into an instruction:
    // "you lost it", not "you never had one".
    assert_eq!(
        subscribe(&f, &conn(3), &old).await.unwrap_err(),
        "taken-over"
    );
    subscribe(&f, &second, &new)
        .await
        .expect("the new lease works");
}

/// The takeover's whole job: every connection the previous holder had —
/// its control connection and any stream it opened — is closed with the
/// reason that says why, and the requester's own connection is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_closes_the_previous_holders_connections_but_never_the_requesters() {
    let f = fixture();
    let control = conn(1);
    let lease = connect(&f, &control, false).await.expect("connect").lease;
    let stream = conn(2);
    let _push = subscribe(&f, &stream, &lease)
        .await
        .expect("subscribe under the live lease");

    let taker = conn(3);
    connect(&f, &taker, true).await.expect("takeover");

    assert_eq!(control.watch.reason(), Some(CloseReason::TakenOver));
    assert_eq!(
        stream.watch.reason(),
        Some(CloseReason::TakenOver),
        "a subscribe registers the connection, which is what makes it reachable by a takeover"
    );
    assert_eq!(
        taker.watch.reason(),
        None,
        "the requesting connection is the one being answered on"
    );
}

/// A client that already holds a connection under the lease and takes it
/// over again (HS-2's reconnect shape) keeps that connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registered_connection_taking_over_does_not_close_itself() {
    let f = fixture();
    let first = conn(1);
    let lease = connect(&f, &first, false).await.expect("connect").lease;
    let second = conn(2);
    let _push = subscribe(&f, &second, &lease).await.expect("subscribe");

    connect(&f, &second, true).await.expect("takeover");

    assert_eq!(second.watch.reason(), None);
    assert_eq!(first.watch.reason(), Some(CloseReason::TakenOver));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_without_a_usable_lease_names_the_missing_step() {
    let f = fixture();

    // Absent, empty, and simply wrong all land on the same instruction:
    // there is no lease here, go get one.
    let missing = f
        .handler
        .handle(&conn(1).ctx, ops::EVENTS_SUBSCRIBE, serde_json::json!({}))
        .await
        .expect_err("a leaseless subscribe must be refused");
    assert_eq!(missing.code, "connect-required");
    assert!(
        missing.message.contains("session.connect"),
        "the error must name the step that was skipped: {}",
        missing.message
    );

    assert_eq!(
        subscribe(&f, &conn(2), "").await.unwrap_err(),
        "connect-required"
    );
    connect(&f, &conn(3), false).await.expect("connect");
    assert_eq!(
        subscribe(&f, &conn(4), "00000000000000000000000000000000")
            .await
            .unwrap_err(),
        "connect-required",
        "a lease this session never issued is not a tombstone"
    );
}

/// `tab_id_filter` is refused before the lease is even looked at: an
/// unimplemented param is a client bug worth naming on its own, and
/// answering `connect-required` first would hide it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unimplemented_filter_still_wins_over_the_lease_gate() {
    let f = fixture();
    let err = f
        .handler
        .handle(
            &conn(1).ctx,
            ops::EVENTS_SUBSCRIBE,
            serde_json::json!({"tab_id_filter": "7"}),
        )
        .await
        .expect_err("a filtered subscribe must be refused");
    assert_eq!(err.code, "invalid-param");
}

/// Authority handed out after the latch would be authority over a session
/// that has already flushed and reaped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_racing_the_stop_latch_is_refused() {
    let f = fixture();
    f.handler
        .handle(&conn(1).ctx, ops::SESSION_STOP, serde_json::json!({}))
        .await
        .expect("session.stop");

    assert_eq!(
        connect(&f, &conn(2), false).await.unwrap_err(),
        "shutting-down"
    );
    assert_eq!(
        connect(&f, &conn(2), true).await.unwrap_err(),
        "shutting-down",
        "takeover is not a way around the latch"
    );
}

/// A stop closes every connection the lease holder had, with the reason
/// that tells a client not to reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_closes_the_lease_holders_connections() {
    let f = fixture();
    let control = conn(1);
    let lease = connect(&f, &control, false).await.expect("connect").lease;
    let stream = conn(2);
    let _push = subscribe(&f, &stream, &lease).await.expect("subscribe");

    f.handler
        .handle(&conn(3).ctx, ops::SESSION_STOP, serde_json::json!({}))
        .await
        .expect("session.stop");

    assert_eq!(control.watch.reason(), Some(CloseReason::ShuttingDown));
    assert_eq!(stream.watch.reason(), Some(CloseReason::ShuttingDown));
}

/// A UI socket has no session, so it has no lease to hand out either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ui_socket_does_not_know_session_connect() {
    let dir = tempfile::tempdir().unwrap();
    let handler = IpcHandler::new(
        Arc::new(Workspace::open(dir.path().join("state.json"))),
        Arc::new(PtySupervisor::new()),
        dir.path().join("roost.sock"),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );
    let err = handler
        .handle(&conn(1).ctx, ops::SESSION_CONNECT, serde_json::json!({}))
        .await
        .expect_err("a UI socket must not serve session.connect");
    assert_eq!(err.code, "unknown-op");
}

// ---------------------------------------------------------------------------
// session.set_focus, and the two edges that forget it (plan 038 C6)
// ---------------------------------------------------------------------------

/// One tab in a fresh project.
fn a_tab(f: &Fixture) -> i64 {
    let project = f.workspace.create_project("p", "/tmp").unwrap().id;
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
/// externally visible reading of the session's focus state, and the one
/// the whole op exists to move.
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
        "focus is the driving client's to state"
    );

    let lease = connect(&f, &holder, false).await.expect("connect").lease;
    assert_eq!(
        set_focus(&f, &holder, &lease, Some(tab + 999))
            .await
            .unwrap_err(),
        "not-found"
    );
    set_focus(&f, &holder, &lease, Some(tab))
        .await
        .expect("the client states what it is looking at");
    assert!(!attention_fires(&f, tab), "the focused tab is suppressed");
}

/// The lease turning over forgets the focus the previous holder
/// reported: it was a statement about *its* window, and carried over it
/// would mute a tab for a client that never said anything — the exact
/// headless-default bug this op exists to fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_lease_forgets_the_previous_clients_focus() {
    let f = fixture();
    let tab = a_tab(&f);
    let first = conn(1);
    let lease = connect(&f, &first, false).await.expect("connect").lease;
    set_focus(&f, &first, &lease, Some(tab))
        .await
        .expect("focus");
    assert!(!attention_fires(&f, tab));

    connect(&f, &conn(2), true).await.expect("takeover");
    assert!(
        attention_fires(&f, tab),
        "a taken-over session must not keep muting the displaced client's tab"
    );
}

/// The other edge: the lease outlives its connections by design, but a
/// focus does not. When the last connection under the live lease goes
/// away nobody is looking at this session, so the flag drops — while the
/// selection it moved stays put.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_last_connection_closing_forgets_the_focus_but_not_the_selection() {
    let f = fixture();
    let tab = a_tab(&f);
    let control = conn(1);
    let lease = connect(&f, &control, false).await.expect("connect").lease;
    let stream = conn(2);
    let _push = subscribe(&f, &stream, &lease).await.expect("subscribe");
    set_focus(&f, &control, &lease, Some(tab))
        .await
        .expect("focus");

    // The subscriber going away is neither the last connection nor the
    // one that stated the focus: the client is still there, still
    // looking.
    f.handler.connection_ended(stream.ctx.conn_id);
    assert!(
        !attention_fires(&f, tab),
        "the client still holds the connection it spoke on"
    );

    f.handler.connection_ended(control.ctx.conn_id);
    assert!(attention_fires(&f, tab), "nobody is attached any more");
    assert_eq!(
        f.workspace.active().1,
        tab,
        "the selection stays where the departed client left it"
    );

    // The lease itself survives — a client can come back on it and
    // re-assert, which is what a reconnecting UI does at `Connected`.
    let back = conn(3);
    set_focus(&f, &back, &lease, Some(tab))
        .await
        .expect("the same lease still works");
    assert!(!attention_fires(&f, tab));
}

/// And it must not depend on the order the server notices things. A
/// client re-dialing on the same lease can register before the departed
/// one's close is processed; counting live connections alone would then
/// leave the gone client's focus standing, which is the mute all over
/// again. The focus is retired by its *author* going away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_focus_dies_with_its_author_even_when_someone_registered_first() {
    let f = fixture();
    let tab = a_tab(&f);
    let author = conn(1);
    let lease = connect(&f, &author, false).await.expect("connect").lease;
    set_focus(&f, &author, &lease, Some(tab))
        .await
        .expect("focus");
    assert!(!attention_fires(&f, tab));

    let late = conn(2);
    let _push = subscribe(&f, &late, &lease).await.expect("subscribe");
    f.handler.connection_ended(author.ctx.conn_id);

    assert!(
        attention_fires(&f, tab),
        "the client that stated the focus is gone"
    );
}

/// A null focus is nobody's claim: the connection that sent it closing
/// leaves nothing to retire, and the flag is already down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_null_focus_leaves_no_claim_behind() {
    let f = fixture();
    let tab = a_tab(&f);
    let holder = conn(1);
    let lease = connect(&f, &holder, false).await.expect("connect").lease;
    set_focus(&f, &holder, &lease, Some(tab))
        .await
        .expect("focus");
    set_focus(&f, &holder, &lease, None)
        .await
        .expect("and then nothing");

    let other = conn(2);
    let _push = subscribe(&f, &other, &lease).await.expect("subscribe");
    f.handler.connection_ended(holder.ctx.conn_id);
    assert!(attention_fires(&f, tab));
}

/// A connection ending on a socket with no session touches nothing — the
/// hook is a session's business alone, and a UI reports its own focus.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ui_sockets_connection_ending_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(Workspace::open(dir.path().join("state.json")));
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
// session.set_agent_hooks (plan 046 C8)
// ---------------------------------------------------------------------------

/// An install backend that records what it was asked for and answers a
/// fixed result. What the engine owes this op is admission and decoding,
/// so a recorder is the whole of the far side.
fn recording_backend() -> (
    AgentHooksHandle,
    Arc<std::sync::Mutex<Vec<AgentHooksRequest>>>,
) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let handle = AgentHooksHandle::new(move |request: AgentHooksRequest| {
        sink.lock().unwrap().push(request);
        async {
            Ok(SessionSetAgentHooksResult {
                wired: vec!["claude".into()],
                skipped: vec![AgentHooksSkipped {
                    agent: "cursor".into(),
                    reason: "skip-list".into(),
                }],
                ..SessionSetAgentHooksResult::default()
            })
        }
    });
    (handle, seen)
}

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

/// The gate is the lease, and what gets through carries the client's own
/// values verbatim — including `off`, which on a host means *remove*.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_agent_hooks_needs_the_lease_and_hands_the_client_values_on() {
    let (handle, seen) = recording_backend();
    let f = fixture_with(Some(handle));
    let holder = conn(1);

    assert_eq!(
        set_agent_hooks(&f, &holder, "not-a-lease", "auto").await,
        Err("connect-required".into()),
        "writing the host's dotfiles is not something an unleased client does"
    );
    assert!(seen.lock().unwrap().is_empty(), "a refusal must not run it");

    let lease = connect(&f, &holder, false).await.expect("connect").lease;
    let result = set_agent_hooks(&f, &holder, &lease, "auto")
        .await
        .expect("the lease holder may wire the host");
    assert_eq!(result.wired, vec!["claude".to_string()]);
    assert_eq!(result.skipped[0].agent, "cursor");

    set_agent_hooks(&f, &holder, &lease, "off")
        .await
        .expect("off is a value of the same op");

    {
        let asked = seen.lock().unwrap();
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[0].mode, AgentHooksMode::Auto);
        assert_eq!(asked[0].skip, vec!["cursor".to_string()]);
        assert_eq!(asked[0].client, "charlie-mbp");
        assert_eq!(asked[1].mode, AgentHooksMode::Off);
    }

    // A second client taking the lease over is the only holder left, and
    // the displaced one may no longer rewrite the host's files.
    let usurper = conn(2);
    connect(&f, &usurper, true).await.expect("takeover");
    assert_eq!(
        set_agent_hooks(&f, &holder, &lease, "auto").await,
        Err("taken-over".into())
    );
}

/// A session built without an install backend answers honestly rather
/// than reporting an empty success — the same posture a UI socket takes
/// by not serving `session.*` at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_without_an_install_backend_says_not_supported() {
    let f = fixture();
    let holder = conn(1);
    let lease = connect(&f, &holder, false).await.expect("connect").lease;
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
    let lease = connect(&f, &holder, false).await.expect("connect").lease;
    assert_eq!(
        set_agent_hooks(&f, &holder, &lease, "auto").await,
        Err("internal".into())
    );
}

/// A backend that found its caller's authority gone answers `taken-over`,
/// not `internal`: the two instruct differently, and only one of them
/// means "stop driving this session".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_that_lost_its_authority_answers_taken_over() {
    let handle =
        AgentHooksHandle::new(|_: AgentHooksRequest| async { Err(AgentHooksError::Unauthorized) });
    let f = fixture_with(Some(handle));
    let holder = conn(1);
    let lease = connect(&f, &holder, false).await.expect("connect").lease;
    assert_eq!(
        set_agent_hooks(&f, &holder, &lease, "auto").await,
        Err("taken-over".into())
    );
}

/// **The door is not the point of effect.** The lease is validated when
/// the op is admitted, and then the install runs — on a per-home `flock`,
/// on a `$HOME` that may be network-mounted, for as long as that takes.
/// Closing the asking connection does not cancel the running handler and
/// neither does the client's own 15 s timeout, so a request admitted
/// under a lease that has since been taken over would otherwise go on to
/// rewrite the host's files and overwrite the state record — undoing the
/// policy of the client that displaced it.
///
/// So the credential travels with the request and is re-asked where it
/// matters. This drives the shape exactly: a backend parked mid-run, a
/// takeover while it is parked, and the authority it is holding answering
/// `false` afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_install_that_outlives_its_lease_is_told_so_at_the_point_of_effect() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let answered = Arc::new(std::sync::Mutex::new(Vec::<bool>::new()));

    let handle = {
        let (entered, release, answered) = (
            Arc::clone(&entered),
            Arc::clone(&release),
            Arc::clone(&answered),
        );
        AgentHooksHandle::new(move |request: AgentHooksRequest| {
            let (entered, release, answered) = (
                Arc::clone(&entered),
                Arc::clone(&release),
                Arc::clone(&answered),
            );
            async move {
                // The install engine asks this once it owns the lock;
                // parking here is that wait, made deterministic.
                entered.notify_one();
                release.notified().await;
                answered.lock().unwrap().push(request.authority.holds());
                Ok(SessionSetAgentHooksResult::default())
            }
        })
    };

    let f = fixture_with(Some(handle));
    let holder = conn(1);
    let lease = connect(&f, &holder, false).await.expect("connect").lease;

    // Uncontested first: the authority is real, and says yes.
    release.notify_one();
    set_agent_hooks(&f, &holder, &lease, "auto")
        .await
        .expect("the lease holder may wire the host");
    assert_eq!(*answered.lock().unwrap(), vec![true]);

    // Now the race the fix exists for. The second op is parked inside the
    // backend while another client takes the session over.
    let usurper = conn(2);
    let (_, ()) = tokio::join!(set_agent_hooks(&f, &holder, &lease, "auto"), async {
        entered.notified().await;
        connect(&f, &usurper, true).await.expect("takeover");
        release.notify_one();
    });
    assert_eq!(
        *answered.lock().unwrap(),
        vec![true, false],
        "a displaced client's install must not still be authorised to write"
    );
}
