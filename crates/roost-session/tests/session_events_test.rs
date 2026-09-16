//! `events.subscribe` against the real daemon serve path.
//!
//! The mechanics — the fence, batching, every close condition — are
//! pinned in `roost-engine`'s `events_push_test`. What only this level
//! can show is that the daemon's own wiring serves it: a session started
//! by `serve` pushes real workspace commits, its `tab.list` carries the
//! matching fence, and its stop takes the stream down with it.

mod support;

use std::path::Path;
use std::time::Duration;

use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::messages::{
    ops, EventBatch, EventsSubscribeResult, Response, SESSION_STOPPING_EVENT,
};
use tokio::net::UnixStream;

type Reader = FrameReader<tokio::net::unix::OwnedReadHalf>;
type Writer = tokio::net::unix::OwnedWriteHalf;

/// Dial the session socket at the frame level. The typed `IpcClient`
/// cannot follow this connection past the ack — after the flip there are
/// no response envelopes left to correlate.
///
/// The write half comes back with it and must be held: the server reads
/// the push connection only to notice a peer that went away, so dropping
/// it is indistinguishable from hanging up.
async fn subscribe(socket_path: &Path) -> (Reader, Writer, u64) {
    let stream = UnixStream::connect(socket_path)
        .await
        .expect("dial the session socket");
    let (r, mut w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "1",
        "op": ops::EVENTS_SUBSCRIBE,
        "params": {},
    }))
    .unwrap();
    write_frame(&mut w, &body).await.expect("write subscribe");

    let line = tokio::time::timeout(support::scaled(Duration::from_secs(10)), reader.read_line())
        .await
        .expect("the ack must arrive")
        .expect("read")
        .expect("expected the ack frame");
    let response: Response = serde_json::from_slice(&line).expect("response envelope");
    assert!(response.ok, "subscribe failed: {response:?}");
    let ack: EventsSubscribeResult =
        serde_json::from_value(response.result.expect("result")).expect("typed ack");
    (reader, w, ack.revision)
}

async fn next_batch(reader: &mut Reader) -> EventBatch {
    let line = tokio::time::timeout(support::scaled(Duration::from_secs(10)), reader.read_line())
        .await
        .expect("a batch must arrive")
        .expect("read")
        .expect("expected a batch frame");
    serde_json::from_slice(&line).expect("typed batch")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_pushes_its_commits_and_cuts_the_stream_on_stop() {
    let layout = support::Layout::new();
    let served = layout.spawn();
    let socket_path = layout.socket_path();

    let mut client = support::connect(&socket_path).await;
    let seeded = support::tabs(&mut client).await;
    let project_id = seeded[0].project_id;

    // Two subscribers, and they get the same stream: the daemon serves
    // every connection, it does not merely tolerate a second one.
    let (mut watching, _watch_w, watch_fence) = subscribe(&socket_path).await;

    let (mut reader, _w, fence) = subscribe(&socket_path).await;
    assert!(fence > 0, "hydration alone commits");

    // A real op, on another connection, through the whole daemon.
    let cwd = layout.subdir("watched");
    let tab = support::open_tab(
        &mut client,
        project_id,
        &cwd,
        "watched",
        &["/bin/sh", "-c", "sleep 30"],
    )
    .await;

    // The tab's own commit, plus whatever the drain does behind it —
    // walk until we see the open. Revisions must stay contiguous.
    let mut expected = fence + 1;
    let opened = loop {
        let batch = next_batch(&mut reader).await;
        assert_eq!(batch.revision, expected, "the revision stream has a gap");
        expected += 1;
        if let Some(event) = batch
            .events
            .iter()
            .find(|e| e.event == ops::EVENT_TAB_OPENED)
        {
            break event.clone();
        }
    };
    assert_eq!(opened.data["tab"]["id"], tab.id.to_string());
    assert_eq!(opened.data["tab"]["title"], "watched");

    // The second stream saw the same open, on the same contiguous
    // sequence.
    let mut expected_watch = watch_fence + 1;
    loop {
        let batch = next_batch(&mut watching).await;
        assert_eq!(
            batch.revision, expected_watch,
            "the observer's revision stream has a gap"
        );
        expected_watch += 1;
        if batch
            .events
            .iter()
            .any(|e| e.event == ops::EVENT_TAB_OPENED)
        {
            break;
        }
    }

    // The fence a client would snapshot at is the same counter.
    let list = support::tab_list(&mut client).await;
    assert!(
        list.revision.expect("a session socket fences tab.list") >= expected - 1,
        "the snapshot fence must not trail the pushed batches"
    );

    // Stopping the session takes the stream down with it, and says so.
    // Reading to the close rather than asserting on the next frame:
    // batches committed before the cut are legitimately still in flight,
    // so what is pinned is that the LAST frame is the label.
    let _ = support::session_stop(&mut client).await;
    let mut last = None;
    let ended = tokio::time::timeout(support::scaled(Duration::from_secs(10)), async {
        loop {
            match reader.read_line().await {
                Ok(Some(line)) => last = serde_json::from_slice::<serde_json::Value>(&line).ok(),
                Ok(None) | Err(_) => return,
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the push connection must close on stop");
    let last = last.expect("the stream must end with a labeled frame, not a bare EOF");
    assert_eq!(last["event"], SESSION_STOPPING_EVENT, "last frame: {last}");
    assert_eq!(last["data"]["reason"], "stop");

    served.await.expect("join").expect("serve");
}

/// A directory whose mode is restored when the test ends, however it
/// ends — a `0o500` directory left behind survives `TempDir`'s own
/// cleanup and breaks whatever runs next.
struct ReadOnlyDir {
    path: std::path::PathBuf,
    restore: u32,
}

impl ReadOnlyDir {
    fn seal(path: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let restore = std::fs::metadata(path).unwrap().permissions().mode();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500)).unwrap();
        Self {
            path: path.to_path_buf(),
            restore,
        }
    }

    fn unseal(self) {}
}

impl Drop for ReadOnlyDir {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(self.restore));
    }
}

/// #481 end to end on the real daemon: a session whose state directory
/// goes read-only answers every op, reports the failure on
/// `session.identify`, and puts exactly one `workspace.durability_changed`
/// on the stream however many commits fail — then one more, the other
/// way, when a write lands again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_that_cannot_write_its_layout_says_so_once_and_keeps_serving() {
    let layout = support::Layout::new();
    let state_dir = layout.subdir("state");
    let config = roost_session::SessionConfig {
        state_path: state_dir.join("state.json"),
        ..layout.config()
    };
    let served = layout.spawn_config(config);
    let socket_path = layout.socket_path();

    let mut client = support::connect(&socket_path).await;
    let seeded = support::tabs(&mut client).await;
    let project_id = seeded[0].project_id;
    assert_eq!(
        support::session_identify(&mut client).await.persist_error,
        None,
        "the hydrating session wrote fine"
    );

    let (mut reader, _w, _fence) = subscribe(&socket_path).await;
    let sealed = ReadOnlyDir::seal(&state_dir);

    // Two failing commits. The op still answers: a workspace nobody can
    // save is still a workspace, and refusing here would block opening a
    // tab on a full disk.
    let cwd = layout.subdir("sealed");
    let tab = support::open_tab(
        &mut client,
        project_id,
        &cwd,
        "one",
        &["/bin/sh", "-c", "sleep 30"],
    )
    .await;
    support::set_tab_title(&mut client, tab.id, "two").await;

    let failure = support::session_identify(&mut client)
        .await
        .persist_error
        .expect("the session must report the write it could not make");

    sealed.unseal();
    // The recovery is what terminates the walk below, so the count is
    // exact rather than "however many had arrived by now".
    support::set_tab_title(&mut client, tab.id, "three").await;

    let mut announced: Vec<serde_json::Value> = vec![];
    tokio::time::timeout(support::scaled(Duration::from_secs(10)), async {
        loop {
            let batch = next_batch(&mut reader).await;
            for event in batch.events {
                if event.event == ops::EVENT_WORKSPACE_DURABILITY_CHANGED {
                    let recovered = event.data["error"].is_null();
                    announced.push(event.data);
                    if recovered {
                        return;
                    }
                }
            }
        }
    })
    .await
    .expect("the recovery must reach the stream");

    assert_eq!(
        announced.len(),
        2,
        "two failing commits are one announcement, and the recovery is the other: {announced:?}"
    );
    assert_eq!(announced[0]["error"], serde_json::json!(failure));
    assert_eq!(announced[1]["error"], serde_json::json!(null));
    assert_eq!(
        support::session_identify(&mut client).await.persist_error,
        None,
        "and the standing value a resyncing client reads is cleared too"
    );

    let _ = support::session_stop(&mut client).await;
    served.await.expect("join").expect("serve");
}
