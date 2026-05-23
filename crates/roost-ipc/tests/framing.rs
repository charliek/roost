//! Integration tests for the framing layer over a real `UnixStream`
//! pair. The in-crate unit tests in `src/framing.rs` cover edge
//! cases against `&[u8]`; this file exercises the same flow over the
//! transport `IpcServer` and `IpcClient` actually use.

use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::MAX_FRAME_BYTES;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

async fn pair() -> (UnixStream, UnixStream) {
    UnixStream::pair().expect("UnixStream::pair")
}

#[tokio::test]
async fn round_trip_one_frame_over_uds() {
    let (mut a, b) = pair().await;
    let mut reader = FrameReader::new(b);
    let payload = serde_json::to_vec(&serde_json::json!({
        "id": "1",
        "op": "identify",
        "params": {"client_name": "roostctl"}
    }))
    .unwrap();
    write_frame(&mut a, &payload).await.unwrap();
    a.shutdown().await.unwrap();
    let line = reader.read_line().await.unwrap().unwrap();
    assert_eq!(line, payload);
    assert!(reader.read_line().await.unwrap().is_none());
}

// Production JSON payloads never embed a raw `\n` because serde_json
// escapes newlines inside string values as `\\n`. The framer's
// contract is that any literal `\n` byte in the input stream is a
// delimiter — the test below pins that for callers that might write
// non-JSON payloads to the same stream.

#[tokio::test]
async fn write_frame_rejects_embedded_newline() {
    // M3b CR follow-up: write_frame validates that the payload
    // does not contain a literal `\n`. Production callers feed
    // `serde_json::to_vec` output (which escapes newlines), so
    // this rejection only catches buggy callers that would
    // otherwise emit multiple logical frames in one write.
    let (mut a, _b) = pair().await;
    match write_frame(&mut a, b"hello\nworld").await {
        Err(roost_ipc::Error::EmbeddedNewline) => {}
        other => panic!("expected EmbeddedNewline, got {other:?}"),
    }
}

#[tokio::test]
async fn many_small_frames_back_to_back() {
    let (mut a, b) = pair().await;
    let mut reader = FrameReader::new(b);

    let writer = tokio::spawn(async move {
        for i in 0..1000usize {
            let payload = format!("frame-{i}");
            write_frame(&mut a, payload.as_bytes()).await.unwrap();
        }
        a.shutdown().await.unwrap();
    });

    for i in 0..1000usize {
        let line = reader.read_line().await.unwrap().unwrap();
        assert_eq!(line, format!("frame-{i}").into_bytes());
    }
    assert!(reader.read_line().await.unwrap().is_none());
    writer.await.unwrap();
}

#[tokio::test]
async fn large_one_mib_frame_round_trips() {
    let (a, b) = pair().await;
    let mut reader = FrameReader::new(b);
    let payload = vec![b'x'; 1024 * 1024];
    let writer_payload = payload.clone();
    let writer = tokio::spawn(async move {
        let mut a = a;
        write_frame(&mut a, &writer_payload).await.unwrap();
        a.shutdown().await.unwrap();
    });
    let line = reader.read_line().await.unwrap().unwrap();
    assert_eq!(line.len(), payload.len());
    assert_eq!(line, payload);
    writer.await.unwrap();
}

#[tokio::test]
async fn max_frame_minus_one_succeeds() {
    let (a, b) = pair().await;
    let mut reader = FrameReader::new(b);
    let payload = vec![b'a'; MAX_FRAME_BYTES - 1];
    let writer_payload = payload.clone();
    let writer = tokio::spawn(async move {
        let mut a = a;
        write_frame(&mut a, &writer_payload)
            .await
            .map_err(|e| format!("write_frame: {e}"))?;
        a.shutdown().await.map_err(|e| format!("shutdown: {e}"))?;
        Ok::<_, String>(())
    });
    let line = reader.read_line().await.unwrap().unwrap();
    // Compare full byte contents, not just length — a wrong byte
    // at the boundary would otherwise slip through silently.
    assert_eq!(line, payload, "boundary frame bytes diverged");
    // Surface writer-side errors; previously the JoinHandle was
    // dropped and any write failure would have been silent.
    writer.await.expect("writer join").expect("writer io");
}
