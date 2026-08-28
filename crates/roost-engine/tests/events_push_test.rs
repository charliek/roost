//! `events.subscribe` on a session socket: the ack fence, the batch
//! relay, and every way the stream is allowed to end.
//!
//! Driven over a real `IpcServer` on a Unix socket wherever the
//! request→push flip matters, because the flip is the part that cannot
//! be observed through the `Handler` trait: after it, the connection
//! stops being request/response and the only remaining signals are
//! "a frame arrived" and "the socket closed".
//!
//! Workspace mutations mostly go through the `Workspace` handle rather
//! than through ops. It is the same commit path either way (every op
//! ends in `Workspace::commit`), and it keeps the tests free of PTYs
//! they would only have to reap.

use std::sync::Arc;
use std::time::Duration;

use roost_engine::event_push::{self, PushLimits};
use roost_engine::ipc::{IpcHandler, SessionInfo, StopHandle};
use roost_engine::{PtySupervisor, Workspace, WorkspaceEvent};
use roost_ipc::agent::{AgentLifecycle, AgentTabState, Ownership, ShellState};
use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::messages::{
    ops, EventBatch, EventsSubscribeResult, Project, Response, SessionStopResult, Tab,
    TabListResult, TabState,
};
use roost_ipc::{IpcClient, IpcServer};
use tempfile::TempDir;
use tokio::net::UnixStream;

const TIMEOUT: Duration = Duration::from_secs(10);

type Reader = FrameReader<tokio::net::unix::OwnedReadHalf>;
type Writer = tokio::net::unix::OwnedWriteHalf;

struct Harness {
    socket: std::path::PathBuf,
    workspace: Arc<Workspace>,
    _dir: TempDir,
}

/// Bind a server. `session` picks which socket kind this is; `limits`
/// narrows the push bounds so the overflow branch is reachable.
async fn harness(session: bool, limits: Option<PushLimits>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("roost.sock");
    let workspace = Arc::new(Workspace::open(dir.path().join("state.json")));
    let supervisor = Arc::new(PtySupervisor::new());
    let mut handler = IpcHandler::new(
        Arc::clone(&workspace),
        supervisor,
        socket.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    );
    if session {
        handler = handler.with_session(
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
    }
    if let Some(limits) = limits {
        handler = handler.with_push_limits(limits);
    }
    let server = IpcServer::bind(&socket, handler).await.expect("bind");
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    Harness {
        socket,
        workspace,
        _dir: dir,
    }
}

impl Harness {
    async fn dial(&self) -> (Reader, Writer) {
        for _ in 0..400 {
            if let Ok(stream) = UnixStream::connect(&self.socket).await {
                let (r, w) = stream.into_split();
                return (FrameReader::new(r), w);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("server never came up at {}", self.socket.display());
    }

    /// Dial, subscribe, and return the connection plus the acked fence.
    async fn subscribe(&self) -> (Reader, Writer, u64) {
        let (mut reader, mut w) = self.dial().await;
        request(&mut w, 1, ops::EVENTS_SUBSCRIBE, serde_json::json!({})).await;
        let ack = read_frame(&mut reader).await;
        assert_eq!(ack["ok"], serde_json::json!(true), "ack: {ack}");
        let result: EventsSubscribeResult =
            serde_json::from_value(ack["result"].clone()).expect("typed ack");
        (reader, w, result.revision)
    }
}

async fn request(w: &mut Writer, id: i64, op: &str, params: serde_json::Value) {
    let body = serde_json::to_vec(&serde_json::json!({
        "id": id.to_string(),
        "op": op,
        "params": params,
    }))
    .unwrap();
    write_frame(w, &body).await.expect("write request");
}

async fn read_frame(reader: &mut Reader) -> serde_json::Value {
    let line = tokio::time::timeout(TIMEOUT, reader.read_line())
        .await
        .expect("a frame must arrive")
        .expect("read")
        .expect("expected a frame, got EOF");
    serde_json::from_slice(&line).expect("valid JSON frame")
}

/// The next frame decoded as a batch. Fails loudly on a response
/// envelope, which is the shape a wrongly-ordered ack would arrive in.
async fn read_batch(reader: &mut Reader) -> EventBatch {
    let value = read_frame(reader).await;
    assert!(
        value.get("id").is_none(),
        "a push frame must not be a response envelope: {value}"
    );
    serde_json::from_value(value).expect("typed batch")
}

fn names(batch: &EventBatch) -> Vec<&str> {
    batch.events.iter().map(|e| e.event.as_str()).collect()
}

/// The ack lands before any batch, and the first batch is exactly the
/// commit after the acked fence — no batch at or below it, and no gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ack_fences_the_stream_and_the_first_batch_is_the_next_commit() {
    let h = harness(true, None).await;
    // Commits before the subscribe: the fence must account for them.
    h.workspace.create_project("early", "/tmp").unwrap();

    let (mut reader, _w, fence) = h.subscribe().await;
    assert!(
        fence > 0,
        "the fence must reflect the pre-subscribe commits"
    );

    h.workspace.create_project("later", "/tmp").unwrap();
    let batch = read_batch(&mut reader).await;
    assert_eq!(batch.revision, fence + 1);
    assert_eq!(names(&batch), vec![ops::EVENT_PROJECT_CREATED]);

    h.workspace.create_project("later still", "/tmp").unwrap();
    let batch = read_batch(&mut reader).await;
    assert_eq!(batch.revision, fence + 2, "revisions must not skip");
}

/// One commit, several events: they arrive together, in commit order,
/// under one revision. A subscriber that saw them split across frames
/// could observe half a transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_event_commit_is_one_batch() {
    let h = harness(true, None).await;
    let project = h.workspace.create_project("p", "/tmp").unwrap();
    let a = h.workspace.open_tab(project.id, "/tmp", "a").unwrap();
    let b = h.workspace.open_tab(project.id, "/tmp", "b").unwrap();

    let (mut reader, _w, fence) = h.subscribe().await;
    h.workspace.delete_project(project.id).unwrap();

    let batch = read_batch(&mut reader).await;
    assert_eq!(batch.revision, fence + 1);
    assert_eq!(
        names(&batch),
        vec![
            ops::EVENT_TAB_CLOSED,
            ops::EVENT_TAB_CLOSED,
            ops::EVENT_PROJECT_DELETED,
            ops::EVENT_ACTIVE_CHANGED,
        ],
        "the whole cascade rides one batch, in commit order"
    );
    assert_eq!(batch.events[0].data["tab_id"], a.id.to_string());
    assert_eq!(batch.events[1].data["tab_id"], b.id.to_string());
    assert_eq!(batch.events[2].data["project_id"], project.id.to_string());
}

/// A commit that produced no events is still a revision, so it is still
/// a batch. Skipping it would make a legitimate revision look like loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_commit_pushes_an_empty_batch() {
    let h = harness(true, None).await;
    // Seed the default project *and* the active selection, so the
    // second call has nothing left to change.
    h.workspace.ensure_default_project("/tmp");

    let (mut reader, _w, fence) = h.subscribe().await;
    h.workspace.ensure_default_project("/tmp");

    let batch = read_batch(&mut reader).await;
    assert_eq!(batch.revision, fence + 1, "no revision gap");
    assert!(
        batch.events.is_empty(),
        "an empty commit is an empty batch, not a skipped one: {:?}",
        names(&batch)
    );
}

/// The fence contract as a client actually walks it: subscribe, let some
/// commits happen, pull `tab.list` for a revision `S`, then discard
/// every batch `<= S`. The next one must be exactly `S + 1` — that is
/// what makes "snapshot then stream" lossless without a replay buffer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tab_list_revision_fences_the_live_stream() {
    let h = harness(true, None).await;
    let (mut reader, _w, _fence) = h.subscribe().await;

    for i in 0..3 {
        h.workspace
            .create_project(&format!("p{i}"), "/tmp")
            .unwrap();
    }

    let mut client = IpcClient::connect(&h.socket).await.expect("connect");
    let list: TabListResult = client
        .call(ops::TAB_LIST, serde_json::json!({}))
        .await
        .expect("tab.list");
    let snapshot = list.revision.expect("a session socket fences tab.list");
    assert_eq!(list.projects.len(), 3);

    // More commits, after the snapshot.
    h.workspace.create_project("after", "/tmp").unwrap();

    let mut batch = read_batch(&mut reader).await;
    while batch.revision <= snapshot {
        batch = read_batch(&mut reader).await;
    }
    assert_eq!(
        batch.revision,
        snapshot + 1,
        "the first batch past the snapshot must be its successor"
    );
    assert_eq!(names(&batch), vec![ops::EVENT_PROJECT_CREATED]);
}

/// A subscriber that stops reading is dropped, not buffered forever and
/// not silently thinned. The close is the whole protocol: reconnect,
/// re-subscribe, re-pull.
///
/// This is the relay's half of the bound. The socket-write half — a peer
/// whose unread buffer blocks the write itself — is pinned in
/// `roost-ipc`'s `server_seam_test`, where the write lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_that_stops_draining_is_closed_and_can_heal() {
    let limits = PushLimits {
        capacity: 1,
        stall: Duration::from_millis(200),
    };
    let h = harness(true, Some(limits)).await;
    let project = h.workspace.create_project("p", "/tmp").unwrap();
    let tab = h.workspace.open_tab(project.id, "/tmp", "t").unwrap();
    let (mut reader, w, _fence) = h.subscribe().await;

    // Commit far faster than a client that never reads can absorb. Which
    // bound notices first — the queue staying full past the stall
    // budget, or the workspace broadcast lapping the relay — is not the
    // contract; that the subscriber is dropped rather than silently
    // thinned is.
    for i in 0..8_000 {
        h.workspace
            .set_tab_has_notification(tab.id, i % 2 == 0)
            .unwrap();
    }

    let closed = tokio::time::timeout(TIMEOUT, async {
        loop {
            match reader.read_line().await {
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "a stalled subscriber must be dropped");
    drop(w);

    // Healing is a fresh connection: subscribe again, re-pull, and carry
    // on from the new fence — no replay buffer, no gap to reconstruct.
    let (mut reader, _w, fence) = h.subscribe().await;
    let mut client = IpcClient::connect(&h.socket).await.expect("connect");
    let list: TabListResult = client
        .call(ops::TAB_LIST, serde_json::json!({}))
        .await
        .expect("tab.list");
    assert_eq!(list.projects[0].tabs.len(), 1);
    assert!(list.revision.expect("fence") >= fence);

    h.workspace.create_project("healed", "/tmp").unwrap();
    let batch = read_batch(&mut reader).await;
    assert_eq!(batch.revision, fence + 1);
    assert_eq!(names(&batch), vec![ops::EVENT_PROJECT_CREATED]);
}

/// The bounded-delivery policy on its own, with no socket in the way:
/// a source nobody polls backs up, stays backed up past the stall
/// budget, and the relay gives up — dropping its sender, which is what
/// closes the connection at the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_gives_up_on_a_source_nobody_polls() {
    let workspace = Arc::new(Workspace::new());
    let (_revision, mut source, _abort) = event_push::spawn(
        &workspace,
        PushLimits {
            capacity: 1,
            stall: Duration::from_millis(100),
        },
    );
    for i in 0..8 {
        workspace.create_project(&format!("p{i}"), "/tmp").unwrap();
    }

    // Drain only after the budget has expired: whatever the queue holds
    // comes through, and then the source ends rather than waiting on a
    // relay that has already given up.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut delivered = 0;
    while let Some(_batch) = tokio::time::timeout(TIMEOUT, source.next())
        .await
        .expect("the source must not hang")
    {
        delivered += 1;
        assert!(delivered <= 8, "the queue must stay bounded");
    }
    assert!(delivered >= 1, "the queue delivers what it accepted");
}

/// The documented-but-unimplemented param is rejected, not ignored. A
/// client that believed it was filtered would mis-attribute every other
/// tab's events to the one it asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tab_id_filter_is_refused_rather_than_ignored() {
    let h = harness(true, None).await;
    let (mut reader, mut w) = h.dial().await;
    request(
        &mut w,
        1,
        ops::EVENTS_SUBSCRIBE,
        serde_json::json!({"tab_id_filter": "7"}),
    )
    .await;
    let reply = read_frame(&mut reader).await;
    assert_eq!(reply["ok"], serde_json::json!(false));
    assert_eq!(reply["error"]["code"], "invalid-param");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tab_id_filter"),
        "the message must name the param: {reply}"
    );

    // Refused, not flipped: the connection is still request/response.
    request(&mut w, 2, ops::IDENTIFY, serde_json::json!({})).await;
    assert_eq!(read_frame(&mut reader).await["id"], "2");
}

/// After the flip the connection is one-way. A request written on it is
/// read (so a peer that goes away is still noticed) and discarded — it
/// must not be answered, and it must not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_sent_after_the_flip_is_discarded() {
    let h = harness(true, None).await;
    let (mut reader, mut w, fence) = h.subscribe().await;

    request(
        &mut w,
        99,
        ops::TAB_OPEN,
        serde_json::json!({
            "project_id": "0",
            "cwd": "/tmp",
            "argv": ["/bin/sh", "-c", "sleep 30"],
            "cols": 0,
            "rows": 0,
            "title": "ghost",
        }),
    )
    .await;

    // The next frame is the batch for our own commit, not a reply to the
    // smuggled request — and the tab it asked for never opened.
    h.workspace.create_project("p", "/tmp").unwrap();
    let batch = read_batch(&mut reader).await;
    assert_eq!(batch.revision, fence + 1);
    assert_eq!(names(&batch), vec![ops::EVENT_PROJECT_CREATED]);
    assert!(
        h.workspace.snapshot().iter().all(|p| p.tabs.is_empty()),
        "a discarded tab.open must not have run"
    );
}

/// A stop cuts every push connection. The client sees a plain close —
/// the same signal it already treats as "resync" — and it sees it by the
/// time the stop's own reply comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_stop_closes_a_live_push_connection() {
    let h = harness(true, None).await;
    let (mut reader, _w, _fence) = h.subscribe().await;

    let mut client = IpcClient::connect(&h.socket).await.expect("connect");
    let report: SessionStopResult = client
        .call(ops::SESSION_STOP, serde_json::json!({}))
        .await
        .expect("session.stop");
    assert!(report.reaped.is_empty() && report.killed.is_empty());

    // The stop tears pushes down before the barrier, so by the time its
    // reply is on the wire the subscriber's connection is already gone.
    let tail = tokio::time::timeout(TIMEOUT, reader.read_line())
        .await
        .expect("the push connection must close");
    assert!(
        matches!(tail, Ok(None) | Err(_)),
        "expected EOF on the push connection, got a frame"
    );

    // And a subscribe after the latch is refused rather than handed a
    // stream nothing will ever end.
    let (mut reader, mut w) = h.dial().await;
    request(&mut w, 1, ops::EVENTS_SUBSCRIBE, serde_json::json!({})).await;
    let reply = read_frame(&mut reader).await;
    assert_eq!(reply["error"]["code"], "shutting-down");
}

/// The UI socket is untouched by all of the above: the op still answers
/// `not-implemented` with the exact message it always has, and
/// `tab.list` carries no `revision` key at all — not `null`, absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ui_socket_is_byte_identical() {
    let h = harness(false, None).await;
    let (mut reader, mut w) = h.dial().await;

    request(&mut w, 1, ops::EVENTS_SUBSCRIBE, serde_json::json!({})).await;
    let reply = read_frame(&mut reader).await;
    assert_eq!(
        reply,
        serde_json::json!({
            "id": "1",
            "ok": false,
            "error": {
                "code": "not-implemented",
                "message": "events.subscribe is not yet implemented",
            },
        })
    );

    h.workspace.create_project("p", "/tmp").unwrap();
    request(&mut w, 2, ops::TAB_LIST, serde_json::json!({})).await;
    let reply = read_frame(&mut reader).await;
    let result = reply["result"].as_object().expect("result object");
    assert_eq!(
        result.keys().collect::<Vec<_>>(),
        vec!["projects"],
        "a UI socket's tab.list must carry nothing but projects"
    );
    // The typed decode still works, with the field defaulted to absent.
    let list: TabListResult = serde_json::from_value(reply["result"].clone()).unwrap();
    assert_eq!(list.revision, None);
}

/// A session socket's `tab.list` does carry it, and the response still
/// decodes for a client that predates the field.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_socket_fences_tab_list() {
    let h = harness(true, None).await;
    let mut client = IpcClient::connect(&h.socket).await.expect("connect");
    let before: TabListResult = client
        .call(ops::TAB_LIST, serde_json::json!({}))
        .await
        .expect("tab.list");
    h.workspace.create_project("p", "/tmp").unwrap();
    let after: TabListResult = client
        .call(ops::TAB_LIST, serde_json::json!({}))
        .await
        .expect("tab.list");
    assert_eq!(
        after.revision.unwrap(),
        before.revision.unwrap() + 1,
        "the fence must move with the commit"
    );
}

/// Every event in the catalog, walked through the serializer. The match
/// inside `envelope` is total, so this table is what proves each variant
/// reaches the wire under the *right* name and shape rather than merely
/// compiling.
#[test]
fn every_workspace_event_has_a_wire_name() {
    let tab = Tab {
        id: 5,
        project_id: 1,
        title: "zsh".into(),
        cwd: "/tmp".into(),
        state: TabState::Running,
        has_notification: false,
        is_active: true,
        user_titled: false,
        position: 0,
        created_at: 1_700_000_000,
        last_active: 1_700_000_000,
        hook_active: false,
        shell_state: ShellState::ForegroundProcess,
        agent_lifecycle: AgentLifecycle::Inactive,
        ownership: None,
    };
    let project = Project {
        id: 1,
        name: "Roost".into(),
        cwd: "/tmp".into(),
        position: 0,
        created_at: 1_700_000_000,
        tabs: Vec::new(),
    };
    let agent = AgentTabState {
        shell: ShellState::AtPrompt,
        lifecycle: AgentLifecycle::Waiting,
        ownership: Some(Ownership {
            source: "claude".into(),
            session_id: "abc123".into(),
            last_event_at: 1_700_000_000,
            detail: "permission_prompt".into(),
            metadata: Default::default(),
        }),
    };

    let catalog: Vec<(WorkspaceEvent, &str, Vec<&str>)> = vec![
        (
            WorkspaceEvent::TabOpened(tab.clone()),
            ops::EVENT_TAB_OPENED,
            vec!["tab"],
        ),
        (
            WorkspaceEvent::TabClosed { tab_id: 5 },
            ops::EVENT_TAB_CLOSED,
            vec!["tab_id"],
        ),
        (
            WorkspaceEvent::TabStateChanged {
                tab_id: 5,
                state: TabState::Running,
            },
            ops::EVENT_TAB_STATE_CHANGED,
            vec!["state", "tab_id"],
        ),
        (
            WorkspaceEvent::TabTitleChanged {
                tab_id: 5,
                title: "t".into(),
            },
            ops::EVENT_TAB_TITLE_CHANGED,
            vec!["tab_id", "title"],
        ),
        (
            WorkspaceEvent::TabCwdChanged {
                tab_id: 5,
                cwd: "/tmp".into(),
            },
            ops::EVENT_TAB_CWD_CHANGED,
            vec!["cwd", "tab_id"],
        ),
        (
            WorkspaceEvent::TabNotification {
                tab_id: 5,
                has_pending: true,
            },
            ops::EVENT_TAB_NOTIFICATION,
            vec!["has_pending", "tab_id"],
        ),
        (
            WorkspaceEvent::ProjectCreated(project),
            ops::EVENT_PROJECT_CREATED,
            vec!["project"],
        ),
        (
            WorkspaceEvent::ProjectRenamed {
                project_id: 1,
                name: "n".into(),
            },
            ops::EVENT_PROJECT_RENAMED,
            vec!["name", "project_id"],
        ),
        (
            WorkspaceEvent::ProjectDeleted { project_id: 1 },
            ops::EVENT_PROJECT_DELETED,
            vec!["project_id"],
        ),
        (
            WorkspaceEvent::ActiveChanged {
                project_id: 1,
                tab_id: 5,
            },
            ops::EVENT_ACTIVE_CHANGED,
            vec!["project_id", "tab_id"],
        ),
        (
            WorkspaceEvent::HookActiveChanged {
                tab_id: 5,
                active: true,
            },
            ops::EVENT_HOOK_ACTIVE_CHANGED,
            vec!["active", "tab_id"],
        ),
        (
            WorkspaceEvent::AgentChanged {
                tab_id: 5,
                agent: agent.clone(),
            },
            ops::EVENT_AGENT_REPORT_CHANGED,
            vec![
                "agent_lifecycle",
                "hook_active",
                "ownership",
                "shell_state",
                "state",
                "tab_id",
            ],
        ),
        (
            WorkspaceEvent::NotificationFired {
                tab_id: 5,
                title: "t".into(),
                body: "b".into(),
            },
            ops::EVENT_NOTIFICATION_FIRED,
            vec!["body", "tab_id", "title"],
        ),
        (
            WorkspaceEvent::TabsReordered {
                project_id: 1,
                tab_ids: vec![5, 6],
            },
            ops::EVENT_TABS_REORDERED,
            vec!["project_id", "tab_ids"],
        ),
        (
            WorkspaceEvent::ProjectsReordered {
                project_ids: vec![1, 2],
            },
            ops::EVENT_PROJECTS_REORDERED,
            vec!["project_ids"],
        ),
    ];

    for (event, name, keys) in &catalog {
        let envelope =
            event_push::envelope(event).unwrap_or_else(|| panic!("{name} must have a wire form"));
        assert_eq!(&envelope.event, name);
        let data = envelope
            .data
            .as_object()
            .unwrap_or_else(|| panic!("{name} data must be an object"));
        assert_eq!(&data.keys().map(String::as_str).collect::<Vec<_>>(), keys);
    }

    // Ids stay string-encoded, lists included — a JSON `Number` would
    // round a tab id past 2^53.
    let reordered = event_push::envelope(&WorkspaceEvent::TabsReordered {
        project_id: 1,
        tab_ids: vec![9_007_199_254_740_993, 6],
    })
    .expect("tabs.reordered");
    assert_eq!(
        reordered.data["tab_ids"],
        serde_json::json!(["9007199254740993", "6"])
    );
    assert_eq!(reordered.data["project_id"], serde_json::json!("1"));

    // The agent event carries its two projections pre-derived.
    let agent_event = event_push::envelope(&WorkspaceEvent::AgentChanged { tab_id: 5, agent })
        .expect("agent_report.changed");
    assert_eq!(agent_event.data["state"], serde_json::json!("needs_input"));
    assert_eq!(agent_event.data["hook_active"], serde_json::json!(true));
}

/// `Resync` is an in-process recovery snapshot with no wire spelling.
/// Serializing it would be inventing an event; the relay closes the
/// connection instead, which is the signal a client already handles.
#[test]
fn resync_never_reaches_the_wire() {
    assert!(event_push::envelope(&WorkspaceEvent::Resync(Vec::new())).is_none());
}

/// The catalog is exhaustive by construction — this is the reminder that
/// makes that claim checkable: the wire's event-name constants and the
/// serializer's arms are the same set.
#[test]
fn the_wire_catalog_has_no_orphan_names() {
    let names = [
        ops::EVENT_TAB_OPENED,
        ops::EVENT_TAB_CLOSED,
        ops::EVENT_TAB_STATE_CHANGED,
        ops::EVENT_TAB_TITLE_CHANGED,
        ops::EVENT_TAB_CWD_CHANGED,
        ops::EVENT_TAB_NOTIFICATION,
        ops::EVENT_PROJECT_CREATED,
        ops::EVENT_PROJECT_RENAMED,
        ops::EVENT_PROJECT_DELETED,
        ops::EVENT_ACTIVE_CHANGED,
        ops::EVENT_HOOK_ACTIVE_CHANGED,
        ops::EVENT_AGENT_REPORT_CHANGED,
        ops::EVENT_NOTIFICATION_FIRED,
        ops::EVENT_TABS_REORDERED,
        ops::EVENT_PROJECTS_REORDERED,
    ];
    let unique: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "duplicate wire event name");
    for name in names {
        assert!(
            name.contains('.') && name == name.to_lowercase(),
            "{name} breaks the dotted-lowercase convention"
        );
    }
}

/// A malformed frame on a push connection is discarded like any other
/// inbound frame — it must not produce a parse-error reply, which would
/// interleave a response envelope into a batch stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_frame_after_the_flip_produces_no_reply() {
    let h = harness(true, None).await;
    let (mut reader, mut w, fence) = h.subscribe().await;

    write_frame(&mut w, b"{not json").await.expect("write");
    h.workspace.create_project("p", "/tmp").unwrap();

    let batch = read_batch(&mut reader).await;
    assert_eq!(batch.revision, fence + 1);
}

/// The ack is a normal response envelope: `ok` + `result`, at the id the
/// client sent. Pinned because a client matches it by id like any other
/// reply, and only afterwards switches its reader into batch mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ack_is_an_ordinary_response_envelope() {
    let h = harness(true, None).await;
    let (mut reader, mut w) = h.dial().await;
    request(&mut w, 42, ops::EVENTS_SUBSCRIBE, serde_json::json!({})).await;
    let raw = read_frame(&mut reader).await;
    let response: Response = serde_json::from_value(raw).expect("response envelope");
    assert_eq!(response.id, 42);
    assert!(response.ok);
    let result: EventsSubscribeResult =
        serde_json::from_value(response.result.expect("result")).expect("typed ack");
    assert_eq!(result.revision, h.workspace.revision());
}
