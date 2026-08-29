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

use roost_engine::ipc::{IpcHandler, SessionInfo, StopHandle};
use roost_engine::{PtySupervisor, Workspace};
use roost_ipc::messages::{ops, SessionConnectResult};
use roost_ipc::{CloseReason, ConnAction, ConnCloseWatch, ConnCtx, Handler, HandlerOutcome};
use tempfile::TempDir;

struct Fixture {
    handler: IpcHandler,
    workspace: Arc<Workspace>,
    _dir: TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(Workspace::open(dir.path().join("state.json")));
    let handler = IpcHandler::new(
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
        },
        StopHandle::new(|| async {}),
    );
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
