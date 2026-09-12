//! The client registry: who a session is tracking, and what a stop owes
//! each of them.
//!
//! Driven through the [`Handler`] trait rather than a socket: what these
//! cases are about is *which connection* a record belongs to, and
//! connection identity is exactly what the trait carries. The
//! wire-visible half — a closed connection's final labeled frame — is
//! pinned in `events_push_test`, where there is a socket to see it on.

use std::sync::Arc;

use roost_engine::ipc::{AgentHooksHandle, AgentHooksRequest, IpcHandler, SessionInfo, StopHandle};
use roost_engine::{PtySupervisor, Workspace};
use roost_ipc::messages::{
    ops, AgentHooksMode, AgentHooksSkipped, SessionConnectResult, SessionSetAgentHooksResult,
};
use roost_ipc::{
    CloseReason, ConnAction, ConnCloseWatch, ConnCtx, Handler, HandlerOutcome, PushSource,
};
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
/// the opposite of what most of these cases are checking.
async fn subscribe(f: &Fixture, c: &Conn) -> Result<PushSource, String> {
    match f
        .handler
        .handle(&c.ctx, ops::EVENTS_SUBSCRIBE, serde_json::json!({}))
        .await
    {
        Ok(HandlerOutcome::ReplyThen {
            then: ConnAction::StartPush(source),
            ..
        }) => Ok(source),
        Ok(HandlerOutcome::ReplyThen { then, .. }) => {
            panic!("subscribe must start a push, not {then:?}")
        }
        Ok(HandlerOutcome::Reply(value)) => panic!("subscribe must flip to push mode: {value}"),
        Err(e) => Err(e.code),
    }
}

async fn next_frame(source: &mut PushSource) -> Option<serde_json::Value> {
    tokio::time::timeout(std::time::Duration::from_secs(5), source.next())
        .await
        .expect("a stream must answer within its budget")
}

/// What a lease-gated op used to answer a client that had none, or had
/// lost it. Nothing on this socket may answer any of them again.
const RETIRED_AUTHORITY_CODES: [&str; 3] = ["connect-required", "taken-over", "already-connected"];

/// **Nothing deposes anybody.** A second client arriving takes no
/// authority away from the first: every op the first can run before the
/// second connects, it can still run afterwards, and its stream keeps
/// delivering.
///
/// The list is the whole of what used to be gated — theme, focus, agent
/// hooks, put_file — so a gate reinstated on any of them reds this
/// rather than being discovered by a user whose window went read-only.
/// The last two are asserted on the *code* rather than on success: this
/// fixture wires neither a file store nor a server terminal, and which
/// of the two wiring refusals they give depends on the build's features
/// — what must never come back is an authority refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_connect_gates_nothing_and_deposes_nobody() {
    let (handle, _seen) = recording_backend();
    let f = fixture_with(Some(handle));
    let project = f.workspace.create_project("p", "/tmp").unwrap();
    let tab = f.workspace.open_tab(project.id, "/tmp", "sh").unwrap().id;

    let first = conn(1);
    connect(&f, &first).await.expect("the first connect");
    let stream = conn(2);
    let mut push = subscribe(&f, &stream).await.expect("subscribe");

    // The second client. Under the lease this was a takeover.
    connect(&f, &conn(3)).await.expect("a second connect");

    assert_eq!(
        first.watch.reason(),
        None,
        "a second client must close nobody's connection"
    );
    assert_eq!(stream.watch.reason(), None);

    f.handler
        .handle(
            &first.ctx,
            ops::SESSION_SET_FOCUS,
            // `lease` is still a required field on the wire; it is read
            // by nothing and retired at protocol 5.
            serde_json::json!({"lease": "", "focused_tab_id": tab.to_string()}),
        )
        .await
        .expect("session.set_focus");
    f.handler
        .handle(
            &first.ctx,
            ops::SESSION_SET_AGENT_HOOKS,
            serde_json::json!({"lease": "", "mode": "auto", "skip": [], "client": "charlie-mbp"}),
        )
        .await
        .expect("session.set_agent_hooks");
    for (op, params) in [
        (
            ops::SESSION_SET_THEME,
            serde_json::json!({"lease": "", "osc_colors": {"palette": vec!["#000000"; 256], "foreground": "#ffffff", "background": "#000000", "cursor": "#ffffff"}}),
        ),
        (
            ops::SESSION_PUT_FILE,
            serde_json::json!({"lease": "", "name": "note.txt", "data": ""}),
        ),
    ] {
        let refusal = f
            .handler
            .handle(&first.ctx, op, params)
            .await
            .expect_err("this fixture wires neither a server VT nor a file store");
        assert!(
            !RETIRED_AUTHORITY_CODES.contains(&refusal.code.as_str()),
            "{op} was refused for authority rather than for wiring: {refusal:?}"
        );
    }

    // And the stream the first client opened before the second arrived
    // is still a stream.
    f.workspace.create_project("after", "/tmp").unwrap();
    let batch = next_frame(&mut push).await.expect("the stream keeps going");
    assert!(batch.get("revision").is_some(), "expected a batch: {batch}");
}

/// A stop closes every stream, and a subscribe that raced the sweep is
/// refused rather than registered into a list nobody will read again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_closes_every_stream() {
    let f = fixture();
    let first = conn(1);
    let second = conn(2);
    let _a = subscribe(&f, &first).await.expect("a stream");
    let _b = subscribe(&f, &second).await.expect("another stream");

    f.handler
        .handle(&conn(3).ctx, ops::SESSION_STOP, serde_json::json!({}))
        .await
        .expect("session.stop");

    assert_eq!(first.watch.reason(), Some(CloseReason::ShuttingDown));
    assert_eq!(second.watch.reason(), Some(CloseReason::ShuttingDown));
    assert_eq!(
        subscribe(&f, &conn(4)).await.err(),
        Some("shutting-down".to_string()),
        "a subscribe that raced the sweep is refused rather than orphaned"
    );
}

/// `tab_id_filter` is refused before anything is spawned or registered:
/// an unimplemented param is a client bug worth naming on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unimplemented_filter_is_refused_before_a_stream_opens() {
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

/// A stop closes both kinds of record a client holds — its control
/// connection and its stream — with the reason that tells a client not
/// to reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_closes_a_control_connection_and_its_stream() {
    let f = fixture();
    let control = conn(1);
    f.handler
        .handle(&control.ctx, ops::SESSION_IDENTIFY, serde_json::json!({}))
        .await
        .expect("an op on the control connection");
    let stream = conn(2);
    let _push = subscribe(&f, &stream).await.expect("subscribe");

    f.handler
        .handle(&conn(3).ctx, ops::SESSION_STOP, serde_json::json!({}))
        .await
        .expect("session.stop");

    assert_eq!(control.watch.reason(), Some(CloseReason::ShuttingDown));
    assert_eq!(stream.watch.reason(), Some(CloseReason::ShuttingDown));
}

/// Three clients, three labeled goodbyes.
///
/// Which is why the registry tracks control connections in their own
/// right: every connection that sends a single op is in there, so a stop
/// can hand each of them the labeled close rather than a bare EOF — and
/// a client that distinguishes "the session stopped" from "the wire
/// died" would re-dial a socket being unlinked on the latter.
///
/// The second half is the other edge: a connection that has already
/// ended must be *gone*, not merely closed twice. A probe that dials,
/// asks one question and hangs up would otherwise accumulate in the
/// registry for the life of the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_labels_every_control_connection_and_forgets_the_ended_one() {
    let f = fixture();
    let (a, b, c) = (conn(1), conn(2), conn(3));
    for connection in [&a, &b, &c] {
        f.handler
            .handle(
                &connection.ctx,
                ops::SESSION_IDENTIFY,
                serde_json::json!({}),
            )
            .await
            .expect("an op registers the connection");
    }

    let probe = conn(4);
    f.handler
        .handle(&probe.ctx, ops::SESSION_IDENTIFY, serde_json::json!({}))
        .await
        .expect("an op");
    f.handler.connection_ended(4);

    f.handler
        .handle(&conn(5).ctx, ops::SESSION_STOP, serde_json::json!({}))
        .await
        .expect("session.stop");

    for (who, connection) in [("a", &a), ("b", &b), ("c", &c)] {
        assert_eq!(
            connection.watch.reason(),
            Some(CloseReason::ShuttingDown),
            "{who}'s control connection was never told the session stopped"
        );
    }
    assert_eq!(
        probe.watch.reason(),
        None,
        "a connection that already ended was forgotten, not kept for the sweep"
    );
}

/// A UI socket has no session, so it does not serve `session.connect`.
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
    mode: &str,
) -> Result<SessionSetAgentHooksResult, String> {
    let params = serde_json::json!({
        "lease": "",
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
        Err(e) => Err(e.code),
    }
}

/// What gets through carries the client's own values verbatim —
/// including `off`, which on a host means *remove*. Two clients stating
/// opposite policies are last-writer-wins (plan 046 §3.4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_agent_hooks_hands_the_client_values_on() {
    let (handle, seen) = recording_backend();
    let f = fixture_with(Some(handle));
    let asking = conn(1);

    let result = set_agent_hooks(&f, &asking, "auto")
        .await
        .expect("any same-UID client may wire the host");
    assert_eq!(result.wired, vec!["claude".to_string()]);
    assert_eq!(result.skipped[0].agent, "cursor");

    set_agent_hooks(&f, &conn(2), "off")
        .await
        .expect("off is a value of the same op, from whichever client sends it");

    let asked = seen.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].mode, AgentHooksMode::Auto);
    assert_eq!(asked[0].skip, vec!["cursor".to_string()]);
    assert_eq!(asked[0].client, "charlie-mbp");
    assert_eq!(asked[1].mode, AgentHooksMode::Off);
}
