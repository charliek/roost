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
async fn subscribe(socket_path: &Path, lease: &str) -> (Reader, Writer, u64) {
    let stream = UnixStream::connect(socket_path)
        .await
        .expect("dial the session socket");
    let (r, mut w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "1",
        "op": ops::EVENTS_SUBSCRIBE,
        "params": {"lease": lease},
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

/// The refusal a subscribe gets, on a connection that is then dropped.
/// Separate from [`subscribe`] because a refused subscribe never flips
/// the connection — there is no stream to hand back.
async fn subscribe_error(socket_path: &Path, lease: &str) -> roost_ipc::messages::ResponseError {
    let stream = UnixStream::connect(socket_path)
        .await
        .expect("dial the session socket");
    let (r, mut w) = stream.into_split();
    let mut reader = FrameReader::new(r);
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "1",
        "op": ops::EVENTS_SUBSCRIBE,
        "params": {"lease": lease},
    }))
    .unwrap();
    write_frame(&mut w, &body).await.expect("write subscribe");
    let line = tokio::time::timeout(support::scaled(Duration::from_secs(10)), reader.read_line())
        .await
        .expect("the reply must arrive")
        .expect("read")
        .expect("expected a reply frame");
    let response: Response = serde_json::from_slice(&line).expect("response envelope");
    assert!(!response.ok, "subscribe must not succeed: {response:?}");
    response.error.expect("an error body")
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
    let launch_cwd = layout.launch_cwd.clone();
    let served = layout.spawn(&launch_cwd);
    let socket_path = layout.socket_path();

    let mut client = support::connect(&socket_path).await;
    let seeded = support::tabs(&mut client).await;
    let project_id = seeded[0].project_id;

    // Lease first: the stream is interactive authority, and the daemon
    // refuses to hand it out to a client that never connected.
    let leaseless = subscribe_error(&socket_path, "").await;
    assert_eq!(leaseless.code, "connect-required", "{leaseless:?}");
    let lease = support::session_connect(&mut client).await.lease;

    let (mut reader, _w, fence) = subscribe(&socket_path, &lease).await;
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
