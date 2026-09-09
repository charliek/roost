#![cfg(feature = "server-vt")]
//! The attach data plane over a real socket (plan 036 C5).
//!
//! Everything here goes through an `IpcServer` bound to a Unix socket
//! and a second connection dialed the way a client dials one, because
//! the parts most likely to break are the ones a `Handler` call cannot
//! see: the first-line sniff that turns a connection binary, the
//! handshake reply, the preamble, and the frame stream after it.
//!
//! The snapshot payload is deliberately NOT decoded here — the records
//! are walked for their tags only. Semantic decode through the plan-034
//! wrapper is the Rust integration client's job (C7/D8); duplicating it
//! would mean two client implementations to keep honest.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use roost_engine::ipc::{
    IpcHandler, SessionInfo, StopHandle, MAX_OUTSTANDING_TOKENS, MAX_TOKENS_PER_CONNECTION,
};
use roost_engine::tab_task::{ServerVtConfig, ServerVtWorkspace};
use roost_engine::{PtySupervisor, Workspace};
use roost_ipc::dataframe::{
    write_data_frame, DataFrame, DataFrameReader, FRAME_ERROR, FRAME_EXIT, FRAME_INPUT, FRAME_PTY,
    FRAME_RESIZE, FRAME_SNAP, MAX_DATA_FRAME_BYTES,
};
use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::messages::{
    ops, AttachAccepted, AttachHandshakeReply, AttachMode, AttachPayloadKind, ResponseError,
    SessionConnectParams, SessionConnectResult, SessionStopParams, SessionStopResult,
    TabAttachParams, TabAttachResult, TabCapturePtyInputParams, TabCapturePtyInputResult,
    TabCloseParams, TabDumpParams, TabDumpResult, TabFeedPtyBytesParams, TabOpenParams,
    TabOpenResult, TabResizeParams, WireTabRef, SESSION_PROTOCOL_VERSION,
};
use roost_ipc::{IpcClient, IpcServer};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

const BUDGET: Duration = Duration::from_secs(20);

/// Record framing from the snapshot format's own table
/// (`snapshot/record.zig`), walked for tags only.
const ENVELOPE_LEN: usize = 10;
const RECORD_HEADER_LEN: usize = 10;
const TAG_READY: u16 = 5;
const TAG_FINISH: u16 = 6;

// ---------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------

struct Harness {
    socket: PathBuf,
    workspace: Arc<Workspace>,
    _dir: TempDir,
}

/// The workspace seam a session hands the tab tasks. The real one is
/// `Workspace` itself; a `Vec` of calls is enough here and keeps the
/// tests off persisted state.
#[derive(Default)]
struct NoopWorkspace;

impl ServerVtWorkspace for NoopWorkspace {
    fn apply_osc(&self, _tab_id: i64, _command: u32, _payload: &str) {}
    fn close_row(&self, _tab_id: i64) {}
    fn tab_effect(&self, _tab_id: i64, _effect: roost_engine::TabEffectKind) {}
}

/// A session advertising both payload kinds, the way a shipped
/// `roost-session` does.
async fn harness() -> Harness {
    harness_advertising(&[AttachPayloadKind::GHOSTTY_SNAPSHOT, AttachPayloadKind::VT]).await
}

/// The same, stating what `session.identify` advertises — the pre-`vt`
/// daemon shape is one entry, and negotiation is defined against the
/// advertisement, not against what the code can encode.
async fn harness_advertising(payload_kinds: &[&str]) -> Harness {
    let payload_kinds: Vec<AttachPayloadKind> = payload_kinds
        .iter()
        .copied()
        .map(AttachPayloadKind::from)
        .collect();
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("roost.sock");
    let workspace = Arc::new(Workspace::open(dir.path().join("state.json")));
    let supervisor = Arc::new(PtySupervisor::new());
    supervisor
        .enable_server_vt(
            ServerVtConfig::new(Arc::new(NoopWorkspace) as Arc<dyn ServerVtWorkspace>)
                .with_input_capture(true),
        )
        .expect("server-vt enables once");

    let handler = IpcHandler::new(
        Arc::clone(&workspace),
        supervisor,
        socket.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    )
    .with_session(
        SessionInfo {
            session_id: "01K3S8TQ4F0Q9YB2K6WZ5D7XN".into(),
            started_at: "2026-08-27T14:03:11Z".into(),
            app_version: "9.9.9".into(),
            payload_kinds,
            libghostty_build: roost_vt::libghostty_build(),
            default_tab_size: (80, 24),
            test_mode: true,
        },
        StopHandle::new(|| async {}),
    );

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
    async fn control(&self) -> IpcClient {
        IpcClient::connect(&self.socket).await.expect("connect")
    }

    /// A connected client holding the lease, plus a live tab parked on
    /// `cat` — a child that keeps its PTY open and echoes nothing on its
    /// own, so every byte on the wire is one the test caused.
    async fn leased_tab(&self) -> (IpcClient, String, i64) {
        self.leased_tab_sized(0, 0).await
    }

    /// The same, at an explicit geometry. Width is what a snapshot's
    /// size follows, so the one test that needs a multi-frame snapshot
    /// asks for a wide tab rather than trying to type its way there.
    async fn leased_tab_sized(&self, cols: u32, rows: u32) -> (IpcClient, String, i64) {
        let mut client = self.control().await;
        let lease: SessionConnectResult = client
            .call(
                ops::SESSION_CONNECT,
                SessionConnectParams {
                    takeover: false,
                    client_label: None,
                },
            )
            .await
            .expect("session.connect");
        let project = self
            .workspace
            .create_project("p", "/tmp")
            .expect("create a project");
        let opened: TabOpenResult = client
            .call(
                ops::TAB_OPEN,
                TabOpenParams {
                    project_id: project.id,
                    cwd: "/tmp".into(),
                    argv: vec!["/bin/sh".into(), "-c".into(), "exec cat".into()],
                    cols,
                    rows,
                    title: String::new(),
                },
            )
            .await
            .expect("tab.open");
        (client, lease.lease, opened.tab.id)
    }

    /// The preamble every "what happens on a live connection" test
    /// shares: a leased client, a tab attached at the default geometry,
    /// and a data connection that has already read its snapshot through
    /// FINISH — so the next frame is whatever the test causes.
    async fn attached(&self) -> (IpcClient, i64, DataClient) {
        let (mut client, lease, tab_id) = self.leased_tab().await;
        let ticket = attach(&mut client, &lease, tab_id).await;
        let (_accepted, mut data) = dial(&self.socket, handshake(&ticket.attach_token))
            .await
            .expect("accepted");
        data.read_snapshot().await;
        (client, tab_id, data)
    }

    /// Attach at an explicit viewport and read the snapshot through, so
    /// the next frame is one the test caused. The accepted reply comes
    /// back with the connection because the geometry tests read it.
    async fn attached_at(
        &self,
        client: &mut IpcClient,
        lease: &str,
        tab_id: i64,
        grid: (u16, u16),
        cell: (u16, u16),
        focus: bool,
    ) -> (AttachAccepted, DataClient) {
        let ticket = attach_with(
            client,
            sized_attach_params(lease, tab_id, grid, cell, focus),
        )
        .await
        .expect("tab.attach");
        let (accepted, mut data) = dial(&self.socket, handshake(&ticket.attach_token))
            .await
            .expect("accepted");
        data.read_snapshot().await;
        (accepted, data)
    }

    /// A tab that has been attached once and has gone quiet again, with
    /// the data connection dropped the way a client's would be. The seq
    /// is the last record that client applied — what it would carry into
    /// `resume_from_seq + 1`.
    async fn caught_up(&self) -> (IpcClient, String, i64, u64) {
        let (mut client, lease, tab_id) = self.leased_tab().await;
        let ticket = attach(&mut client, &lease, tab_id).await;
        let (accepted, mut data) = dial(&self.socket, handshake(&ticket.attach_token))
            .await
            .expect("accepted");
        let (_snapshot, pty) = data.read_snapshot().await;
        // Round-tripped rather than assumed: reading one marker back
        // proves the tab is quiesced at a seq this test knows, instead of
        // guessing that nothing was in flight behind FINISH.
        let after = pty.last().map_or(accepted.seq, |(seq, _)| *seq);
        feed(&mut client, tab_id, b"ROOST_CAUGHT_UP\r\n".to_vec()).await;
        let (applied, _) = data.read_pty_until(after, b"ROOST_CAUGHT_UP").await;
        drop(data);
        (client, lease, tab_id, applied)
    }
}

async fn attach(client: &mut IpcClient, lease: &str, tab_id: i64) -> TabAttachResult {
    attach_with(client, attach_params(lease, tab_id))
        .await
        .expect("tab.attach")
}

fn attach_params(lease: &str, tab_id: i64) -> TabAttachParams {
    TabAttachParams {
        lease: Some(lease.to_string()),
        tab_id,
        kinds: vec![
            AttachPayloadKind::from("sixel-mosaic-v9"),
            AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT),
        ],
        cols: 80,
        rows: 24,
        cell_w_px: 0,
        cell_h_px: 0,
        libghostty_build: roost_vt::libghostty_build(),
        focus: true,
    }
}

/// The same at an explicit viewport. `focus` is the attach-time
/// geometry claim: `true` resizes the tab, `false` attaches at whatever
/// size it already is.
fn sized_attach_params(
    lease: &str,
    tab_id: i64,
    (cols, rows): (u16, u16),
    (cell_w_px, cell_h_px): (u16, u16),
    focus: bool,
) -> TabAttachParams {
    TabAttachParams {
        cols,
        rows,
        cell_w_px,
        cell_h_px,
        focus,
        ..attach_params(lease, tab_id)
    }
}

async fn attach_with(
    client: &mut IpcClient,
    params: TabAttachParams,
) -> Result<TabAttachResult, String> {
    client
        .call(ops::TAB_ATTACH, params)
        .await
        .map_err(|error| match error {
            roost_ipc::ClientError::Server { code, .. } => code,
            other => panic!("tab.attach failed at the transport: {other}"),
        })
}

// ---------------------------------------------------------------------
// The client half of the data plane
// ---------------------------------------------------------------------

/// `Debug` so a rejection can be `expect_err`'d — nothing about a live
/// socket is worth printing, hence the bare name.
struct DataClient {
    reader: DataFrameReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl std::fmt::Debug for DataClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataClient")
    }
}

/// Dial a data connection and run the handshake. `Err` carries the
/// rejection, which is always one JSON line and never a binary frame.
async fn dial(
    socket: &Path,
    handshake: serde_json::Value,
) -> Result<(AttachAccepted, DataClient), ResponseError> {
    let stream = UnixStream::connect(socket).await.expect("dial");
    let (read_half, mut writer) = stream.into_split();
    let body = serde_json::to_vec(&handshake).expect("encode the handshake");
    write_frame(&mut writer, &body).await.expect("write");

    let mut lines = FrameReader::new(read_half);
    let line = timeout(BUDGET, lines.read_line())
        .await
        .expect("a handshake reply in time")
        .expect("read")
        .expect("the server answers every handshake");
    let reply: AttachHandshakeReply = serde_json::from_slice(&line).expect("typed reply");
    let accepted = match reply {
        AttachHandshakeReply::Accepted(accepted) => accepted,
        AttachHandshakeReply::Rejected(error) => return Err(error),
    };

    let (read_half, residue) = lines.into_parts();
    let mut reader = DataFrameReader::new(read_half, residue);
    timeout(BUDGET, reader.read_preamble())
        .await
        .expect("a preamble in time")
        .expect("the preamble follows an accepted handshake");
    Ok((accepted, DataClient { reader, writer }))
}

fn handshake(token: &str) -> serde_json::Value {
    serde_json::json!({"attach": token, "protocol_version": SESSION_PROTOCOL_VERSION})
}

/// The same handshake, plus the resume triple. Every field is spelled
/// out by the caller so a test can lie about exactly one of them.
fn resume_handshake(
    token: &str,
    from_seq: u64,
    server_epoch: u64,
    tab_generation: u64,
) -> serde_json::Value {
    serde_json::json!({
        "attach": token,
        "protocol_version": SESSION_PROTOCOL_VERSION,
        "resume_from_seq": from_seq,
        "server_epoch": server_epoch,
        "tab_generation": tab_generation,
    })
}

impl DataClient {
    async fn next(&mut self) -> Option<DataFrame> {
        timeout(BUDGET, self.reader.next_frame())
            .await
            .expect("a frame in time")
            .expect("frame read")
    }

    async fn frame(&mut self) -> DataFrame {
        self.next().await.expect("the stream is still open")
    }

    async fn send(&mut self, frame_type: u8, payload: &[u8]) {
        let mut bytes = Vec::new();
        write_data_frame(&mut bytes, frame_type, payload)
            .await
            .expect("frame the payload");
        self.raw(&bytes).await;
    }

    async fn raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).await.expect("write");
        self.writer.flush().await.expect("flush");
    }

    /// Read until the snapshot's FINISH record has arrived, returning
    /// the snapshot bytes and every PTY frame that interleaved with
    /// them.
    async fn read_snapshot(&mut self) -> (Vec<u8>, Vec<(u64, Vec<u8>)>) {
        let mut snapshot = Vec::new();
        let mut pty = Vec::new();
        let deadline = Instant::now() + BUDGET;
        while !has_tag(&snapshot, TAG_FINISH) {
            assert!(Instant::now() < deadline, "FINISH never arrived");
            let frame = self.frame().await;
            match frame.frame_type {
                FRAME_SNAP => snapshot.extend_from_slice(&frame.payload),
                FRAME_PTY => pty.push(split_pty(&frame)),
                other => panic!("unexpected frame {other:#04x} during the snapshot"),
            }
        }
        (snapshot, pty)
    }
}

impl DataClient {
    /// Read PTY frames until `marker` has come through, asserting the
    /// stream is contiguous from `after + 1` and carries nothing but PTY
    /// frames — which is also what makes "a resume sends no SNAP"
    /// observable. Returns the last seq seen and the bytes read.
    async fn read_pty_until(&mut self, after: u64, marker: &[u8]) -> (u64, Vec<u8>) {
        let mut next = after + 1;
        let mut text = Vec::new();
        while !contains(&text, marker) {
            let frame = self.frame().await;
            assert_eq!(
                frame.frame_type, FRAME_PTY,
                "expected PTY frames and nothing else"
            );
            let (seq, bytes) = split_pty(&frame);
            assert_eq!(seq, next, "PTY frames must be contiguous");
            next += 1;
            text.extend_from_slice(&bytes);
        }
        (next - 1, text)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn split_pty(frame: &DataFrame) -> (u64, Vec<u8>) {
    assert!(
        frame.payload.len() >= 8,
        "a PTY frame carries a u64 seq and then bytes"
    );
    let seq = u64::from_le_bytes(frame.payload[..8].try_into().unwrap());
    (seq, frame.payload[8..].to_vec())
}

fn error_of(frame: &DataFrame) -> ResponseError {
    assert_eq!(frame.frame_type, FRAME_ERROR, "expected an ERROR frame");
    serde_json::from_slice(&frame.payload).expect("an ERROR frame carries {code, message}")
}

/// Walk the record headers of an encoded snapshot prefix looking for one
/// tag. Framing only — tags, not contents.
fn has_tag(bytes: &[u8], wanted: u16) -> bool {
    let mut at = ENVELOPE_LEN;
    while at + RECORD_HEADER_LEN <= bytes.len() {
        let tag = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let len = u32::from_le_bytes([bytes[at + 2], bytes[at + 3], bytes[at + 4], bytes[at + 5]])
            as usize;
        let end = at + RECORD_HEADER_LEN + len;
        if end > bytes.len() {
            return false;
        }
        if tag == wanted {
            return true;
        }
        at = end;
    }
    false
}

async fn feed(client: &mut IpcClient, tab_id: i64, data: Vec<u8>) {
    client
        .call::<_, serde_json::Value>(
            ops::TAB_FEED_PTY_BYTES,
            TabFeedPtyBytesParams { tab_id, data },
        )
        .await
        .expect("tab.feed_pty_bytes");
}

/// Drain what the tab's PTY writer was handed until every marker has
/// shown up, returning everything read. Test-mode capture, so what this
/// proves is that the bytes crossed the forwarder into the writer.
async fn read_pty_input_until(client: &mut IpcClient, tab_id: i64, markers: &[&[u8]]) -> Vec<u8> {
    let deadline = Instant::now() + BUDGET;
    let mut captured = Vec::new();
    while !markers.iter().all(|marker| contains(&captured, marker)) {
        assert!(Instant::now() < deadline, "the INPUT bytes never arrived");
        let batch: TabCapturePtyInputResult = client
            .call(
                ops::TAB_CAPTURE_PTY_INPUT,
                TabCapturePtyInputParams {
                    tab_id: WireTabRef::Local(tab_id),
                    drain: true,
                },
            )
            .await
            .expect("tab.capture_pty_input");
        captured.extend_from_slice(&batch.data);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    captured
}

async fn dump(client: &mut IpcClient, tab_id: i64) -> TabDumpResult {
    client
        .call(
            ops::TAB_DUMP,
            TabDumpParams {
                tab_id: WireTabRef::Local(tab_id),
                ..Default::default()
            },
        )
        .await
        .expect("tab.dump")
}

/// Poll `predicate` against a fresh dump until it holds.
async fn wait_for_dump(
    client: &mut IpcClient,
    tab_id: i64,
    what: &str,
    mut predicate: impl FnMut(&TabDumpResult) -> bool,
) {
    let deadline = Instant::now() + BUDGET;
    loop {
        let dumped = dump(client, tab_id).await;
        if predicate(&dumped) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------

/// One attach, end to end: the ticket names an identity, the handshake
/// answers with the same one, the snapshot arrives READY-first, and
/// every live PTY frame after it is contiguous from the fence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_snapshot_attach_streams_ready_then_finish_then_live_frames() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;
    let ticket = attach(&mut client, &lease, tab_id).await;
    assert_eq!(
        ticket.kind.as_str(),
        AttachPayloadKind::GHOSTTY_SNAPSHOT,
        "the first kind the server supports wins, unknown ones ignored"
    );
    assert_eq!(ticket.attach_token.len(), 32);

    let (accepted, mut data) = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect("the handshake is accepted");
    assert_eq!(accepted.mode, AttachMode::Snapshot);
    assert_eq!(accepted.server_epoch, ticket.server_epoch);
    assert_eq!(accepted.tab_generation, ticket.tab_generation);
    assert_eq!(accepted.kind.as_str(), AttachPayloadKind::GHOSTTY_SNAPSHOT);

    let (snapshot, _pty) = data.read_snapshot().await;
    // READY has to be reachable in the stream, and the boundary the
    // server streamed at full speed has to be the same one the shared
    // scanner reports.
    assert!(has_tag(&snapshot, TAG_READY), "READY never arrived");
    let boundary = roost_vt::ready_boundary(&snapshot).expect("a complete snapshot has READY");
    assert!(boundary < snapshot.len(), "history must follow READY");

    // Live frames are the tab's own bytes, numbered from the fence.
    // Injected rather than coaxed out of the shell: what is being pinned
    // is the numbering, not a shell's greeting.
    feed(&mut client, tab_id, b"ROOST_LIVE\r\n".to_vec()).await;

    let mut seen = Vec::new();
    let mut text = Vec::new();
    while !text.windows(10).any(|w| w == b"ROOST_LIVE") {
        let frame = data.frame().await;
        assert_eq!(frame.frame_type, FRAME_PTY, "only PTY frames after FINISH");
        let (seq, bytes) = split_pty(&frame);
        seen.push(seq);
        text.extend_from_slice(&bytes);
    }
    let expected: Vec<u64> = (accepted.seq + 1..=accepted.seq + seen.len() as u64).collect();
    assert_eq!(
        seen, expected,
        "PTY frames must be contiguous from the fence"
    );
}

/// The dump the session serves and the terminal the client is attached
/// to are the same terminal, so a RESIZE frame moves both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resize_frame_reaches_the_tabs_terminal() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    // `tab.attach` resizes to the geometry the client asked for before
    // it ever mints a token.
    let mut params = attach_params(&lease, tab_id);
    params.cols = 100;
    params.rows = 30;
    let ticket = attach_with(&mut client, params).await.expect("tab.attach");
    wait_for_dump(&mut client, tab_id, "the attach geometry", |d| {
        (d.cols, d.rows) == (100, 30)
    })
    .await;

    let (_accepted, mut data) = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    data.read_snapshot().await;

    let mut payload = Vec::new();
    for value in [90u16, 20, 9, 18] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    data.send(FRAME_RESIZE, &payload).await;
    wait_for_dump(&mut client, tab_id, "the RESIZE frame to land", |d| {
        (d.cols, d.rows) == (90, 20)
    })
    .await;
}

/// An INPUT frame is the client's keystrokes: it has to reach the PTY
/// writer, in order, byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_input_frame_reaches_the_pty() {
    let h = harness().await;
    let (mut client, tab_id, mut data) = h.attached().await;

    data.send(FRAME_INPUT, b"ROOST_TYPED").await;

    let deadline = Instant::now() + BUDGET;
    let mut captured = Vec::new();
    while !captured.windows(11).any(|window| window == b"ROOST_TYPED") {
        assert!(
            Instant::now() < deadline,
            "the INPUT bytes never reached the PTY"
        );
        let batch: TabCapturePtyInputResult = client
            .call(
                ops::TAB_CAPTURE_PTY_INPUT,
                TabCapturePtyInputParams {
                    tab_id: WireTabRef::Local(tab_id),
                    drain: true,
                },
            )
            .await
            .expect("tab.capture_pty_input");
        captured.extend_from_slice(&batch.data);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// EXIT is the last thing on a data connection, and its `final_seq` is
/// one past the last PTY record — the rule a client uses to know it lost
/// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_is_the_final_frame() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;
    let ticket = attach(&mut client, &lease, tab_id).await;
    let (accepted, mut data) = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    let (_snapshot, pty) = data.read_snapshot().await;

    client
        .call::<_, serde_json::Value>(ops::TAB_CLOSE, TabCloseParams { tab_id })
        .await
        .expect("tab.close");

    let mut last_seq = pty.last().map_or(accepted.seq, |(seq, _)| *seq);
    let exit = loop {
        let frame = data.frame().await;
        match frame.frame_type {
            FRAME_PTY => last_seq = split_pty(&frame).0,
            FRAME_EXIT => break frame,
            other => panic!("unexpected frame {other:#04x} before EXIT"),
        }
    };
    assert_eq!(exit.payload.len(), 12, "u64 final_seq + i32 code");
    let final_seq = u64::from_le_bytes(exit.payload[..8].try_into().unwrap());
    assert_eq!(
        final_seq,
        last_seq + 1,
        "final_seq is one past the last PTY record"
    );
    assert!(
        data.next().await.is_none(),
        "EXIT is always the last frame on the connection"
    );
}

// ---------------------------------------------------------------------
// Refusals — always one JSON line, never a binary frame
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_token_is_refused() {
    let h = harness().await;
    let error = dial(&h.socket, handshake("00000000000000000000000000000000"))
        .await
        .expect_err("an unminted token is not admissible");
    assert_eq!(error.code, "invalid-token");
}

/// Single-use is the whole point of the ticket: the second presentation
/// of a token is indistinguishable from a replayed credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_is_consumed_by_its_first_use() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;
    let ticket = attach(&mut client, &lease, tab_id).await;

    let (_accepted, _data) = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect("the first use is accepted");
    let error = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect_err("the second use is not");
    assert_eq!(error.code, "invalid-token");
}

/// Checked before the token, so a version-skewed client is told what is
/// actually wrong instead of being sent hunting for a bad credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_protocol_mismatch_wins_over_the_token_check() {
    let h = harness().await;
    let error = dial(
        &h.socket,
        serde_json::json!({"attach": "00000000000000000000000000000000", "protocol_version": 1}),
    )
    .await
    .expect_err("a stale protocol is refused");
    assert_eq!(error.code, "protocol-mismatch");
    assert!(
        error
            .message
            .contains(&SESSION_PROTOCOL_VERSION.to_string()),
        "the message names the version this session speaks: {}",
        error.message
    );
}

/// A first line that diverts to the data path but does not decode is
/// answered in the same shape a rejection has — the client never has to
/// guess whether it is reading JSON or binary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_handshake_is_answered_as_a_rejection() {
    let h = harness().await;
    let error = dial(&h.socket, serde_json::json!({"attach": {"nested": true}}))
        .await
        .expect_err("an undecodable handshake is refused");
    assert_eq!(error.code, "parse-error");
}

/// Negotiation is by eligibility, not by a hard-coded kind: the client's
/// list is walked in order and the first entry that is both *servable*
/// (advertised by this session) and *eligible* (its own requirement
/// holds) wins.
///
/// The skew is produced from the client's side, which is the only side a
/// test can move: the session reports the real build, and a client
/// claiming a different one is exactly the upgrade trap R3 exists for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_servable_and_eligible_kind_wins() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;
    let skewed = "ghostty-0000000000000000+snapshot.v1";

    let mut both = attach_params(&lease, tab_id);
    both.kinds = vec![
        AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT),
        AttachPayloadKind::from(AttachPayloadKind::VT),
    ];
    assert_eq!(
        attach_with(&mut client, both.clone())
            .await
            .expect("a matching build serves its first choice")
            .kind
            .as_str(),
        AttachPayloadKind::GHOSTTY_SNAPSHOT,
        "GHOSTSNP is preferred whenever it is eligible"
    );

    let mut both_skewed = both.clone();
    both_skewed.libghostty_build = skewed.into();
    assert_eq!(
        attach_with(&mut client, both_skewed)
            .await
            .expect("the skew falls through to the next offer")
            .kind
            .as_str(),
        AttachPayloadKind::VT,
        "an ineligible first choice must not refuse an eligible second one"
    );

    for build in [roost_vt::libghostty_build(), skewed.to_string()] {
        let mut vt_only = attach_params(&lease, tab_id);
        vt_only.kinds = vec![AttachPayloadKind::from(AttachPayloadKind::VT)];
        vt_only.libghostty_build = build.clone();
        assert_eq!(
            attach_with(&mut client, vt_only)
                .await
                .expect("vt has no build requirement")
                .kind
                .as_str(),
            AttachPayloadKind::VT,
            "vt is a byte stream, so {build:?} is not its business"
        );
    }
}

/// The compatibility promise: a client that has never heard of `vt`
/// negotiates exactly as it did before this session learned to serve it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ghostsnp_only_client_is_unaffected_by_vt() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let mut only = attach_params(&lease, tab_id);
    only.kinds = vec![AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT)];
    assert_eq!(
        attach_with(&mut client, only.clone())
            .await
            .expect("a matching build is served")
            .kind
            .as_str(),
        AttachPayloadKind::GHOSTTY_SNAPSHOT
    );

    let mut skewed = only;
    skewed.libghostty_build = "ghostty-0000000000000000+snapshot.v1".into();
    assert_eq!(
        attach_with(&mut client, skewed).await.unwrap_err(),
        "build-mismatch",
        "a client offering nothing else still has nowhere to fall back to"
    );
}

/// A session that does not advertise `vt` cannot be talked into it, even
/// though this build can encode one: the advertisement is the contract
/// the client negotiated against.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unadvertised_kind_is_not_servable() {
    let h = harness_advertising(&[AttachPayloadKind::GHOSTTY_SNAPSHOT]).await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let mut vt_only = attach_params(&lease, tab_id);
    vt_only.kinds = vec![AttachPayloadKind::from(AttachPayloadKind::VT)];
    assert_eq!(
        attach_with(&mut client, vt_only).await.unwrap_err(),
        "unsupported-kind"
    );
}

/// A kind this session advertised and this build has no rule for is
/// refused outright. Only a misconfigured advertisement can produce it —
/// which is exactly why it must not be waved through: "no requirement"
/// belongs to a kind whose requirement is known to be none, and assuming
/// it for an unknown one serves a build-skewed client GHOSTSNP under
/// another name, the corrupt screen `build-mismatch` exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_advertised_kind_with_no_rule_is_refused() {
    let h = harness_advertising(&["sixel-mosaic-v9"]).await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let mut mystery = attach_params(&lease, tab_id);
    mystery.kinds = vec![AttachPayloadKind::from("sixel-mosaic-v9")];
    assert_eq!(
        attach_with(&mut client, mystery).await.unwrap_err(),
        "internal"
    );
}

/// The client asked for something this session cannot encode, or with a
/// libghostty it cannot exchange snapshots with. Both are refused at
/// `tab.attach`, before any connection is dialed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_control_op_refuses_what_cannot_be_served() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let mut unknown_kind = attach_params(&lease, tab_id);
    unknown_kind.kinds = vec![AttachPayloadKind::from("sixel-mosaic-v9")];
    assert_eq!(
        attach_with(&mut client, unknown_kind).await.unwrap_err(),
        "unsupported-kind"
    );

    let mut wrong_build = attach_params(&lease, tab_id);
    wrong_build.libghostty_build = "ghostty-0000000000000000+snapshot.v1".into();
    assert_eq!(
        attach_with(&mut client, wrong_build).await.unwrap_err(),
        "build-mismatch"
    );

    let mut zero_grid = attach_params(&lease, tab_id);
    zero_grid.rows = 0;
    assert_eq!(
        attach_with(&mut client, zero_grid).await.unwrap_err(),
        "invalid-param"
    );

    let mut missing_tab = attach_params(&lease, tab_id + 9_999);
    missing_tab.kinds = vec![AttachPayloadKind::from("sixel-mosaic-v9")];
    assert_eq!(
        attach_with(&mut client, missing_tab).await.unwrap_err(),
        "not-found",
        "a missing tab wins over an unservable kind"
    );
}

// ---------------------------------------------------------------------
// Ways a live connection ends
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_frame_type_ends_the_connection() {
    let h = harness().await;
    let (_client, _tab_id, mut data) = h.attached().await;

    data.send(0x7E, b"?").await;
    assert_eq!(error_of(&data.frame().await).code, "protocol-error");
    assert!(
        data.next().await.is_none(),
        "the connection closes after it"
    );
}

/// The cap has to be enforced on the header, before the payload is
/// buffered — so the frame is hand-built rather than written through the
/// shared writer, which refuses to emit it at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_frame_ends_the_connection() {
    let h = harness().await;
    let (_client, _tab_id, mut data) = h.attached().await;

    let mut header = u32::try_from(MAX_DATA_FRAME_BYTES + 1)
        .unwrap()
        .to_le_bytes()
        .to_vec();
    header.push(FRAME_INPUT);
    data.raw(&header).await;

    assert_eq!(error_of(&data.frame().await).code, "protocol-error");
    assert!(data.next().await.is_none());
}

/// A tab admits as many data connections as are dialed (plan 057, R15):
/// both see the same PTY output and both can type into it. Nothing is
/// superseded — a second attach used to close the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_attaches_to_one_tab_both_stream_and_both_type() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let first = attach(&mut client, &lease, tab_id).await;
    let (a_accepted, mut a) = dial(&h.socket, handshake(&first.attach_token))
        .await
        .expect("accepted");
    let (_snapshot, pty) = a.read_snapshot().await;
    let a_at = pty.last().map_or(a_accepted.seq, |(seq, _)| *seq);

    let second = attach(&mut client, &lease, tab_id).await;
    let (b_accepted, mut b) = dial(&h.socket, handshake(&second.attach_token))
        .await
        .expect("the first attach is untouched and a second is admitted");
    let (_snapshot, pty) = b.read_snapshot().await;
    let b_at = pty.last().map_or(b_accepted.seq, |(seq, _)| *seq);

    // One tee, two receivers: the same bytes reach both.
    feed(&mut client, tab_id, b"ROOST_BOTH_SEE_THIS\r\n".to_vec()).await;
    a.read_pty_until(a_at, b"ROOST_BOTH_SEE_THIS").await;
    b.read_pty_until(b_at, b"ROOST_BOTH_SEE_THIS").await;

    // And both write: the input side is open to every admitted
    // connection, not to one of them.
    a.send(FRAME_INPUT, b"ROOST_FROM_A").await;
    b.send(FRAME_INPUT, b"ROOST_FROM_B").await;
    let typed =
        read_pty_input_until(&mut client, tab_id, &[b"ROOST_FROM_A", b"ROOST_FROM_B"]).await;
    assert!(contains(&typed, b"ROOST_FROM_A") && contains(&typed, b"ROOST_FROM_B"));
}

/// Dropping one attach leaves the others alone: the registry keeps a
/// list per tab, and a forwarder unwinding removes only its own entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_one_of_several_data_connections_removes_only_its_entry() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let leaving = attach(&mut client, &lease, tab_id).await;
    let (_accepted, mut going) = dial(&h.socket, handshake(&leaving.attach_token))
        .await
        .expect("accepted");
    going.read_snapshot().await;

    let staying = attach(&mut client, &lease, tab_id).await;
    let (accepted, mut data) = dial(&h.socket, handshake(&staying.attach_token))
        .await
        .expect("accepted");
    let (_snapshot, pty) = data.read_snapshot().await;
    let at = pty.last().map_or(accepted.seq, |(seq, _)| *seq);

    drop(going);

    // Still streaming, and a stop still reaches it — which is the half
    // that would break if the departing connection had taken the tab's
    // whole entry with it.
    feed(&mut client, tab_id, b"ROOST_STILL_HERE\r\n".to_vec()).await;
    data.read_pty_until(at, b"ROOST_STILL_HERE").await;

    let _report: SessionStopResult = client
        .call(ops::SESSION_STOP, SessionStopParams {})
        .await
        .expect("session.stop");
    assert_eq!(error_of(&data.frame().await).code, "shutting-down");
}

/// An attach takes no lease, so a session can be serving a data
/// connection having never minted one — and a stop owes that connection
/// the same labeled close as any other. The registry's own walk is what
/// pins this: the closer used to be reachable only through the lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_labels_a_data_connection_on_a_session_that_never_minted_a_lease() {
    let h = harness().await;
    let mut client = h.control().await;
    let project = h
        .workspace
        .create_project("p", "/tmp")
        .expect("create a project");
    let opened: TabOpenResult = client
        .call(
            ops::TAB_OPEN,
            TabOpenParams {
                project_id: project.id,
                cwd: "/tmp".into(),
                argv: vec!["/bin/sh".into(), "-c".into(), "exec cat".into()],
                cols: 0,
                rows: 0,
                title: String::new(),
            },
        )
        .await
        .expect("tab.open");

    let ticket = attach_with(
        &mut client,
        TabAttachParams {
            lease: None,
            ..attach_params("", opened.tab.id)
        },
    )
    .await
    .expect("an attach on a session with no lease at all");
    let (_accepted, mut data) = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    data.read_snapshot().await;

    let _report: SessionStopResult = client
        .call(ops::SESSION_STOP, SessionStopParams {})
        .await
        .expect("session.stop");

    assert_eq!(error_of(&data.frame().await).code, "shutting-down");
    assert!(data.next().await.is_none());
}

/// The lease is accepted and ignored on an attach: absent, empty, and a
/// token this session has already displaced all negotiate a ticket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tab_attach_accepts_no_lease_an_empty_lease_and_a_stale_one() {
    let h = harness().await;
    let (mut client, stale, tab_id) = h.leased_tab().await;
    let mut taker = h.control().await;
    let _taken: SessionConnectResult = taker
        .call(
            ops::SESSION_CONNECT,
            SessionConnectParams {
                takeover: true,
                client_label: None,
            },
        )
        .await
        .expect("session.connect with takeover");

    for lease in [None, Some(String::new()), Some(stale.clone())] {
        let ticket = attach_with(
            &mut client,
            TabAttachParams {
                lease: lease.clone(),
                ..attach_params("", tab_id)
            },
        )
        .await
        .unwrap_or_else(|code| panic!("attach refused {code} for lease={lease:?}"));
        let (_accepted, mut data) = dial(&h.socket, handshake(&ticket.attach_token))
            .await
            .expect("accepted");
        data.read_snapshot().await;
    }
}

// ---------------------------------------------------------------------
// Geometry: the last geometry-bearing interaction sizes the PTY
// ---------------------------------------------------------------------

/// Enable libghostty's mode-2048 in-band size reports on a tab. Every
/// resize then writes one report toward the child, on the very queue
/// keystrokes ride — which is what makes the order between a resize and
/// the input that caused it observable from outside.
async fn watch_size_reports(client: &mut IpcClient, tab_id: i64) {
    feed(client, tab_id, b"\x1b[?2048h".to_vec()).await;
}

/// How many size reports for `grid` a captured stream carries. The
/// report is `CSI 48 ; rows ; cols ; …` — the grid half is all these
/// tests read, so a change in the pixel half cannot break them.
fn size_reports(captured: &[u8], (cols, rows): (u16, u16)) -> usize {
    String::from_utf8_lossy(captured)
        .matches(&format!("48;{rows};{cols}"))
        .count()
}

fn index_of(captured: &[u8], needle: &[u8]) -> usize {
    captured
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap_or_else(|| {
            panic!(
                "{:?} is not in {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(captured)
            )
        })
}

/// Typing is a geometry-bearing interaction: an INPUT frame carries its
/// connection's declared geometry, and the tab takes that size *before*
/// the bytes are written. Both ride one command channel, so the order is
/// the task's receive order and needs no fence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_input_frame_applies_its_connections_geometry_first() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let (_desktop, _a) = h
        .attached_at(&mut client, &lease, tab_id, (100, 30), (8, 16), true)
        .await;
    let (_phone, mut b) = h
        .attached_at(&mut client, &lease, tab_id, (60, 20), (8, 16), false)
        .await;
    wait_for_dump(&mut client, tab_id, "the focused attach's geometry", |d| {
        (d.cols, d.rows) == (100, 30)
    })
    .await;

    watch_size_reports(&mut client, tab_id).await;
    b.send(FRAME_INPUT, b"ROOST_TYPED").await;

    let captured = read_pty_input_until(&mut client, tab_id, &[b"ROOST_TYPED"]).await;
    let resized = index_of(&captured, b"48;20;60");
    let typed = index_of(&captured, b"ROOST_TYPED");
    assert!(
        resized < typed,
        "the resize must precede the bytes it carried: {:?}",
        String::from_utf8_lossy(&captured)
    );
    assert_eq!(
        {
            let d = dump(&mut client, tab_id).await;
            (d.cols, d.rows)
        },
        (60, 20),
        "the PTY follows whoever typed last"
    );
}

/// Geometry is four numbers, not two. A connection at the tab's own grid
/// but with different cell metrics is a different viewport — libghostty's
/// size reports quote the pixel dimensions — so its input resizes the tab
/// even though `tab.dump` cannot tell the difference.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_grid_different_cell_metrics_still_counts_as_a_change() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let (_accepted, mut same) = h
        .attached_at(&mut client, &lease, tab_id, (100, 30), (8, 16), true)
        .await;
    let (_accepted, mut wider_cells) = h
        .attached_at(&mut client, &lease, tab_id, (100, 30), (9, 16), false)
        .await;

    watch_size_reports(&mut client, tab_id).await;
    same.send(FRAME_INPUT, b"ROOST_UNCHANGED").await;
    let captured = read_pty_input_until(&mut client, tab_id, &[b"ROOST_UNCHANGED"]).await;
    assert_eq!(
        size_reports(&captured, (100, 30)),
        0,
        "geometry that has not changed is not re-applied ({:?})",
        String::from_utf8_lossy(&captured)
    );

    wider_cells.send(FRAME_INPUT, b"ROOST_WIDER_CELLS").await;
    let captured = read_pty_input_until(&mut client, tab_id, &[b"ROOST_WIDER_CELLS"]).await;
    assert_eq!(
        size_reports(&captured, (100, 30)),
        1,
        "the same grid at other cell metrics is still a resize ({:?})",
        String::from_utf8_lossy(&captured)
    );
}

/// Two clients typing into one tab do not race for the PTY's size: each
/// INPUT applies its own geometry, and the tab ends up wherever the last
/// command the task *received* asked for — not the last one sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_senders_linearize_in_receive_order() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let (_accepted, mut desktop) = h
        .attached_at(&mut client, &lease, tab_id, (100, 30), (8, 16), true)
        .await;
    let (_accepted, mut phone) = h
        .attached_at(&mut client, &lease, tab_id, (60, 20), (8, 16), false)
        .await;

    // Sequenced, not raced: each marker is read back before the next
    // sender types, so which command the task took first is a fact of
    // the test rather than of the scheduler.
    phone.send(FRAME_INPUT, b"ROOST_PHONE").await;
    read_pty_input_until(&mut client, tab_id, &[b"ROOST_PHONE"]).await;
    let d = dump(&mut client, tab_id).await;
    assert_eq!((d.cols, d.rows), (60, 20));

    desktop.send(FRAME_INPUT, b"ROOST_DESKTOP").await;
    read_pty_input_until(&mut client, tab_id, &[b"ROOST_DESKTOP"]).await;
    let d = dump(&mut client, tab_id).await;
    assert_eq!((d.cols, d.rows), (100, 30));
}

/// `tab.attach` refuses a zero-sized grid, and a RESIZE frame states the
/// same client's geometry, so the two agree: the frame is dropped rather
/// than applied, and the connection carries on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_sized_resize_frame_is_ignored() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;
    let (_accepted, mut data) = h
        .attached_at(&mut client, &lease, tab_id, (100, 30), (8, 16), true)
        .await;

    let mut payload = Vec::new();
    for value in [0u16, 20, 8, 16] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    data.send(FRAME_RESIZE, &payload).await;

    // Read a later frame back, so "nothing happened" is a fact about a
    // tab that has processed the zero-sized one, not about timing.
    data.send(FRAME_INPUT, b"ROOST_AFTER_ZERO").await;
    read_pty_input_until(&mut client, tab_id, &[b"ROOST_AFTER_ZERO"]).await;
    let d = dump(&mut client, tab_id).await;
    assert_eq!(
        (d.cols, d.rows),
        (100, 30),
        "a zero-sized RESIZE leaves the tab alone"
    );
}

/// An unfocused attach claims nothing: the tab keeps the size the
/// focused client gave it, and the accepted handshake reports the
/// geometry its snapshot was actually encoded at — which is what a `vt`
/// client has to build its terminal at.
///
/// A focused attach hears it too. Its resize ran on the control
/// connection before this one was dialed, and raw input is open, so
/// "what I asked for" is not evidence of what the encode composed —
/// `a_focused_attach_reports_the_size_it_was_actually_encoded_at` is the
/// case where the two differ.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unfocused_attach_never_resizes_and_reports_the_snapshot_geometry() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    let (desktop, _a) = h
        .attached_at(&mut client, &lease, tab_id, (100, 30), (8, 16), true)
        .await;
    assert_eq!(
        (desktop.snapshot_cols, desktop.snapshot_rows),
        (Some(100), Some(30)),
        "the size the payload was encoded at, which here is what was asked for"
    );

    let (phone, _b) = h
        .attached_at(&mut client, &lease, tab_id, (60, 20), (8, 16), false)
        .await;
    assert_eq!(
        (phone.snapshot_cols, phone.snapshot_rows),
        (Some(100), Some(30))
    );
    let d = dump(&mut client, tab_id).await;
    assert_eq!(
        (d.cols, d.rows),
        (100, 30),
        "a client that is only watching cannot shrink the one that is typing"
    );
}

/// A focused attach hears the snapshot's real geometry too, and it is
/// the encode's answer rather than the request's (review F7).
///
/// The resize a focused `tab.attach` performs runs on the **control**
/// connection and finishes before the data connection is even dialed.
/// Raw input is open, so anything else may size the tab in that window —
/// here a plain `tab.resize` from a second client, which is the same
/// interaction a phone's first keystroke would be. The payload is then
/// composed at the other client's size, and a `vt` client that hydrated
/// at its own would wrap every line and misplace every absolute cursor
/// move. Suppressing the field on `focus: true` — on the premise that a
/// focused attacher already knows the size — is exactly what R15
/// invalidated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_focused_attach_reports_the_size_it_was_actually_encoded_at() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;

    // The attach negotiates 100x30 and mints a ticket. Nothing has been
    // dialed yet, so nothing has been encoded yet either.
    let ticket = attach_with(
        &mut client,
        sized_attach_params(&lease, tab_id, (100, 30), (8, 16), true),
    )
    .await
    .expect("tab.attach");
    let d = dump(&mut client, tab_id).await;
    assert_eq!((d.cols, d.rows), (100, 30), "the focused attach sized it");

    // Somebody else interacts before the ticket is presented.
    let mut other = h.control().await;
    let _resized: serde_json::Value = other
        .call(
            ops::TAB_RESIZE,
            TabResizeParams {
                tab_id,
                cols: 60,
                rows: 20,
            },
        )
        .await
        .expect("tab.resize");
    wait_for_dump(&mut client, tab_id, "the other client's resize", |d| {
        (d.cols, d.rows) == (60, 20)
    })
    .await;

    let (accepted, mut data) = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    data.read_snapshot().await;
    assert_eq!(
        (accepted.snapshot_cols, accepted.snapshot_rows),
        (Some(60), Some(20)),
        "the reply names the geometry the encode used, not the one asked for"
    );
}

/// Reading is not an interaction. `tab.dump` carries no geometry, so it
/// leaves the tab's size alone however often it is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dump_never_resizes() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;
    let (_accepted, mut data) = h
        .attached_at(&mut client, &lease, tab_id, (100, 30), (8, 16), true)
        .await;

    let d = dump(&mut client, tab_id).await;
    assert_eq!((d.cols, d.rows), (100, 30));

    let mut payload = Vec::new();
    for value in [70u16, 22, 8, 16] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    data.send(FRAME_RESIZE, &payload).await;
    wait_for_dump(&mut client, tab_id, "the RESIZE frame to land", |d| {
        (d.cols, d.rows) == (70, 22)
    })
    .await;

    // A third client reading the tab changes nothing about it.
    let mut reader = h.control().await;
    let d = dump(&mut reader, tab_id).await;
    assert_eq!((d.cols, d.rows), (70, 22));
    let d = dump(&mut client, tab_id).await;
    assert_eq!((d.cols, d.rows), (70, 22));
}

/// A megabyte injected through the session's own test-mode arm has to
/// reach an attached client as ordinary PTY frames. The bytes are
/// chunked to the PTY reader's granularity before they are sequenced,
/// so one big write cannot become one over-cap tee record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_feed_reaches_an_attached_client_as_framed_records() {
    let h = harness().await;
    let (mut client, tab_id, mut data) = h.attached().await;

    let mut payload = vec![b'.'; 1_500_000];
    payload.extend_from_slice(b"\r\nROOST_BIG_DONE\r\n");
    let expected = payload.len();

    // Feed and drain STRICTLY ALTERNATE. FeedBytes bypasses the PTY, so
    // one 1.5 MB op hits the tab task at memory speed and can legally
    // lap the forwarder's tee on a slow machine (Lagged is fatal by
    // contract — that shape is the soak's and CI's macos runner proved
    // it, not this case's). Each ~96 KiB op is ~24 tee records — far
    // inside the 256-event window — and every op's frames are fully
    // read back before the next op exists, so this stays a framing and
    // fidelity claim on every machine speed.
    let mut feeder = IpcClient::connect(&h.socket).await.expect("connect");
    let mut seen = 0usize;
    let mut next_seq = None;
    for piece in payload.chunks(96 * 1024) {
        feed(&mut feeder, tab_id, piece.to_vec()).await;
        let target = seen + piece.len();
        while seen < target {
            let frame = data.frame().await;
            assert_eq!(
                frame.frame_type, FRAME_PTY,
                "a feed produces PTY frames and nothing else"
            );
            let (seq, bytes) = split_pty(&frame);
            if let Some(expected_seq) = next_seq {
                assert_eq!(seq, expected_seq, "PTY frames stay contiguous");
            }
            next_seq = Some(seq + 1);
            seen += bytes.len();
        }
    }
    assert_eq!(seen, expected, "every fed byte arrives exactly once");

    wait_for_dump(&mut client, tab_id, "the fed bytes to render", |d| {
        d.rows_text.iter().any(|row| row.contains("ROOST_BIG_DONE"))
    })
    .await;
}

/// A stop reaches connections that stopped answering requests long ago.
/// EOF is the accepted fallback on an unwritable socket; this peer is
/// reading, so it gets the label.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_stop_labels_a_live_data_connection() {
    let h = harness().await;
    let (mut client, _tab_id, mut data) = h.attached().await;

    let _report: SessionStopResult = client
        .call(ops::SESSION_STOP, SessionStopParams {})
        .await
        .expect("session.stop");

    let frame = data.frame().await;
    assert_eq!(error_of(&frame).code, "shutting-down");
    assert!(data.next().await.is_none());
}

/// A ticket belongs to the connection that minted it, not to a lease
/// (plan 057, R15): a takeover leaves every outstanding one usable, and
/// what reclaims the quota is the minting connection going away.
///
/// The quota is the registry's bound, so it has to be reclaimable
/// without waiting out a TTL — a client that mints its whole share and
/// vanishes must not lock everyone else out for a minute.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_keeps_tokens_and_a_departing_connection_purges_its_own() {
    let h = harness().await;
    let (mut old_client, old_lease, tab_id) = h.leased_tab().await;
    let survivor = attach(&mut old_client, &old_lease, tab_id).await;

    let mut new_client = h.control().await;
    let _taken: SessionConnectResult = new_client
        .call(
            ops::SESSION_CONNECT,
            SessionConnectParams {
                takeover: true,
                client_label: None,
            },
        )
        .await
        .expect("session.connect with takeover");

    // The takeover took the foreground and nothing else: a ticket minted
    // under the displaced lease still admits a data connection.
    let (_accepted, mut data) = dial(&h.socket, handshake(&survivor.attach_token))
        .await
        .expect("a ticket minted before the takeover is still admissible");
    data.read_snapshot().await;

    // The quota is per session, and the displaced client — still
    // attaching on its stale lease, which is accepted and ignored —
    // takes its full share of it. Filling the rest takes a second
    // connection, because no single one may hold the whole pool.
    let mut minted = Vec::new();
    for _ in 0..MAX_TOKENS_PER_CONNECTION {
        minted.push(
            attach(&mut old_client, &old_lease, tab_id)
                .await
                .attach_token,
        );
    }
    let mut filler = h.control().await;
    for _ in 0..(MAX_OUTSTANDING_TOKENS - MAX_TOKENS_PER_CONNECTION) {
        attach_with(&mut filler, attach_params("", tab_id))
            .await
            .expect("a second connection fills the rest of the pool");
    }
    assert_eq!(
        attach_with(&mut new_client, attach_params("", tab_id))
            .await
            .unwrap_err(),
        "too-many-tokens",
        "the quota is what bounds the registry"
    );

    // The connection that minted them goes away, and its tickets go with
    // it — immediately, with no expiry to wait out.
    drop(old_client);
    let deadline = Instant::now() + BUDGET;
    let ticket = loop {
        match attach_with(&mut new_client, attach_params("", tab_id)).await {
            Ok(ticket) => break ticket,
            Err(code) => {
                assert_eq!(code, "too-many-tokens");
                assert!(
                    Instant::now() < deadline,
                    "the minting connection's tickets were never reclaimed"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    let error = dial(&h.socket, handshake(&minted[0]))
        .await
        .expect_err("a purged ticket is not admissible");
    assert_eq!(error.code, "invalid-token");

    let (_accepted, mut data) = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect("the freshly minted ticket is accepted");
    data.read_snapshot().await;
}

/// One connection cannot hold the whole ticket pool (review F2).
///
/// Before R15 minting required the lease, so only the foreground could
/// reach the session-wide quota at all. Raw input is open now: any
/// same-UID process can loop `tab.attach` without ever dialing, and
/// without a per-connection share one buggy script would answer the real
/// UI's attach with `too-many-tokens` for a whole TTL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_connection_cannot_mint_away_everybody_elses_attach() {
    let h = harness().await;
    let (mut hog, lease, tab_id) = h.leased_tab().await;

    for _ in 0..MAX_TOKENS_PER_CONNECTION {
        attach(&mut hog, &lease, tab_id).await;
    }
    assert_eq!(
        attach_with(&mut hog, attach_params(&lease, tab_id))
            .await
            .unwrap_err(),
        "too-many-tokens",
        "its own share is spent"
    );

    // And the session is not: another client — leaseless, as R15 allows
    // — still gets a ticket, and a usable one.
    let mut other = h.control().await;
    let ticket = attach_with(&mut other, attach_params("", tab_id))
        .await
        .expect("a second connection still has room in the pool");
    let (_accepted, mut data) = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    data.read_snapshot().await;
}

/// After the latch there is nothing left to attach to: a ticket minted
/// then would be authority over a session that has already reaped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_after_the_stop_latch_is_refused() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;
    let ticket = attach(&mut client, &lease, tab_id).await;

    let _report: SessionStopResult = client
        .call(ops::SESSION_STOP, SessionStopParams {})
        .await
        .expect("session.stop");

    // Both halves: the control op, and a token minted before the stop.
    // On a fresh connection because the stop closed every one the lease
    // holder had — which is itself the point of registering them.
    let mut after = h.control().await;
    assert_eq!(
        attach_with(&mut after, attach_params(&lease, tab_id))
            .await
            .unwrap_err(),
        "shutting-down"
    );
    let error = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect_err("a pre-stop token is not a way past the latch");
    assert_eq!(error.code, "shutting-down");

    // But the latch does not swallow the token check: a credential this
    // session never issued is broken whether or not it is stopping, and
    // telling its holder `shutting-down` would send it reconnecting with
    // the same bad token.
    let unknown = dial(&h.socket, handshake("00000000000000000000000000000000"))
        .await
        .expect_err("an unminted token is not admissible");
    assert_eq!(unknown.code, "invalid-token");
}

// ---------------------------------------------------------------------
// Resume — the ring instead of a snapshot, and every way it falls back
// ---------------------------------------------------------------------

/// The hit: a client that was away for a few records gets exactly those
/// records back, as ordinary PTY frames, with no snapshot in sight — and
/// the live tee continues from them without a seam.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_replays_the_ring_and_sends_no_snapshot() {
    let h = harness().await;
    let (mut client, lease, tab_id, applied) = h.caught_up().await;

    // What the client misses while it is away. Dumped first so the ring
    // provably holds it before the handoff runs.
    let missed = b"ROOST_MISSED_ONE\r\nROOST_MISSED_TWO\r\n";
    feed(&mut client, tab_id, missed.to_vec()).await;
    wait_for_dump(&mut client, tab_id, "the missed bytes to land", |d| {
        d.rows_text
            .iter()
            .any(|row| row.contains("ROOST_MISSED_TWO"))
    })
    .await;

    let ticket = attach(&mut client, &lease, tab_id).await;
    let (accepted, mut data) = dial(
        &h.socket,
        resume_handshake(
            &ticket.attach_token,
            applied + 1,
            ticket.server_epoch,
            ticket.tab_generation,
        ),
    )
    .await
    .expect("accepted");
    assert_eq!(accepted.mode, AttachMode::Resume);
    assert_eq!(accepted.seq, applied, "the fence is resume_from_seq - 1");
    assert_eq!(accepted.server_epoch, ticket.server_epoch);
    assert_eq!(accepted.tab_generation, ticket.tab_generation);

    let (last, replayed) = data.read_pty_until(applied, b"ROOST_MISSED_TWO").await;
    assert_eq!(
        replayed, missed,
        "the ring hands back the missed bytes, exactly and only them"
    );

    // The subscription came out of the same handoff, so the live stream
    // continues from the replay with no gap and no duplicate.
    feed(&mut client, tab_id, b"ROOST_LIVE_AGAIN\r\n".to_vec()).await;
    let (mut last, live) = data.read_pty_until(last, b"ROOST_LIVE_AGAIN").await;
    assert_eq!(live, b"ROOST_LIVE_AGAIN\r\n");

    // Drained to the end of the connection rather than stopping at the
    // last marker: "no SNAP frames" is a claim about the whole resumed
    // stream, and a scheduling floor that fired late would show up here.
    // Closing the tab is what makes the drain terminate without a sleep.
    client
        .call::<_, serde_json::Value>(ops::TAB_CLOSE, TabCloseParams { tab_id })
        .await
        .expect("tab.close");
    loop {
        let frame = data.frame().await;
        match frame.frame_type {
            FRAME_PTY => {
                let (seq, _) = split_pty(&frame);
                assert_eq!(seq, last + 1, "PTY frames stay contiguous to the end");
                last = seq;
            }
            FRAME_EXIT => break,
            other => panic!("a resumed stream carries no frame {other:#04x}"),
        }
    }
    assert!(data.next().await.is_none());
}

/// `last_assigned + 1` is a hit, not a miss: the client missed nothing,
/// and an empty slice is the honest answer. It must not be turned into a
/// snapshot the client already has.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_slice_resume_is_a_hit() {
    let h = harness().await;
    let (mut client, lease, tab_id, applied) = h.caught_up().await;

    let ticket = attach(&mut client, &lease, tab_id).await;
    let (accepted, mut data) = dial(
        &h.socket,
        resume_handshake(
            &ticket.attach_token,
            applied + 1,
            ticket.server_epoch,
            ticket.tab_generation,
        ),
    )
    .await
    .expect("accepted");
    assert_eq!(accepted.mode, AttachMode::Resume);
    assert_eq!(accepted.seq, applied);

    feed(&mut client, tab_id, b"ROOST_AFTER_NOTHING\r\n".to_vec()).await;
    let (_, text) = data.read_pty_until(applied, b"ROOST_AFTER_NOTHING").await;
    assert_eq!(
        text, b"ROOST_AFTER_NOTHING\r\n",
        "nothing was missed, so the first frame is a live one"
    );
}

/// The eligibility rules, one dial each. None of these is an error and
/// none of them is a refusal: an unhonorable resume triple is served as
/// a full attach, and `mode` is how the client finds out.
///
/// Not covered here, because it is not reachable through the wire: the
/// resume path also checks the live generation against the one the
/// *token* was minted for. Reaching it needs a respawn of the same
/// `tab_id` between `tab.attach` and the handshake, and ids are never
/// reused — the same guard on the snapshot path
/// (`the_control_op_refuses_what_cannot_be_served`) is equally
/// unreachable from a test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unhonorable_resume_triple_falls_back_to_a_snapshot() {
    let h = harness().await;
    let (mut client, lease, tab_id, applied) = h.caught_up().await;

    // Seq 0: a client holding nothing is asking for everything.
    let ticket = attach(&mut client, &lease, tab_id).await;
    falls_back(
        &h.socket,
        resume_handshake(
            &ticket.attach_token,
            0,
            ticket.server_epoch,
            ticket.tab_generation,
        ),
        "seq 0",
    )
    .await;

    // The triple is all-or-nothing: a seq with no identity beside it
    // names a stream on no particular server.
    let ticket = attach(&mut client, &lease, tab_id).await;
    falls_back(
        &h.socket,
        serde_json::json!({
            "attach": ticket.attach_token,
            "protocol_version": SESSION_PROTOCOL_VERSION,
            "resume_from_seq": applied + 1,
        }),
        "a resume with no identity",
    )
    .await;

    // A seq the tab has not reached yet: the client claims to hold
    // records that do not exist, which is the one direction a replay
    // could never fix.
    let ticket = attach(&mut client, &lease, tab_id).await;
    falls_back(
        &h.socket,
        resume_handshake(
            &ticket.attach_token,
            applied + 1_000_000,
            ticket.server_epoch,
            ticket.tab_generation,
        ),
        "a seq past the tab's own",
    )
    .await;
}

/// Dial with a handshake that must not be honored as a resume, and prove
/// what came back is a real full attach rather than just a label.
async fn falls_back(socket: &Path, handshake: serde_json::Value, what: &str) {
    let (accepted, mut data) = dial(socket, handshake)
        .await
        .unwrap_or_else(|error| panic!("{what} must be served, not refused: {}", error.code));
    assert_eq!(
        accepted.mode,
        AttachMode::Snapshot,
        "{what} must fall back to a snapshot"
    );
    let (snapshot, _pty) = data.read_snapshot().await;
    assert!(has_tag(&snapshot, TAG_READY), "{what} must get a snapshot");
}

/// A generation that is not the tab's current one is a different
/// terminal with a different seq space. Falling back is the contract —
/// the client is served, and told by `mode` what it got.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generation_mismatch_falls_back_to_a_snapshot() {
    let h = harness().await;
    let (mut client, lease, tab_id, applied) = h.caught_up().await;

    let ticket = attach(&mut client, &lease, tab_id).await;
    let (accepted, mut data) = dial(
        &h.socket,
        resume_handshake(
            &ticket.attach_token,
            applied + 1,
            ticket.server_epoch,
            ticket.tab_generation + 1,
        ),
    )
    .await
    .expect("a stale resume is served, not refused");
    assert_eq!(accepted.mode, AttachMode::Snapshot);
    assert_eq!(
        accepted.tab_generation, ticket.tab_generation,
        "the reply carries the tab's real identity, not the claim"
    );
    // Really a full attach, not just a label.
    let (snapshot, _pty) = data.read_snapshot().await;
    assert!(has_tag(&snapshot, TAG_READY));
}

/// The epoch is what makes a resume across a daemon restart impossible.
/// A client claiming the wrong one is exactly that case, and gets the
/// same fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_epoch_mismatch_falls_back_to_a_snapshot() {
    let h = harness().await;
    let (mut client, lease, tab_id, applied) = h.caught_up().await;

    let ticket = attach(&mut client, &lease, tab_id).await;
    let (accepted, mut data) = dial(
        &h.socket,
        resume_handshake(
            &ticket.attach_token,
            applied + 1,
            ticket.server_epoch ^ 1,
            ticket.tab_generation,
        ),
    )
    .await
    .expect("a stale resume is served, not refused");
    assert_eq!(accepted.mode, AttachMode::Snapshot);
    assert_eq!(accepted.server_epoch, ticket.server_epoch);
    data.read_snapshot().await;
}

/// A client away long enough for its seq to fall out of the 2 MiB ring
/// cannot be replayed — the records are gone. It pays for a snapshot
/// rather than being told off.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seq_the_ring_no_longer_covers_falls_back_to_a_snapshot() {
    let h = harness().await;
    let (mut client, lease, tab_id, applied) = h.caught_up().await;

    let mut flood = vec![b'.'; 3 * 1024 * 1024];
    flood.extend_from_slice(b"\r\nROOST_FLOOD_DONE\r\n");
    feed(&mut client, tab_id, flood).await;
    wait_for_dump(&mut client, tab_id, "the ring to be overrun", |d| {
        d.rows_text
            .iter()
            .any(|row| row.contains("ROOST_FLOOD_DONE"))
    })
    .await;

    let ticket = attach(&mut client, &lease, tab_id).await;
    let (accepted, mut data) = dial(
        &h.socket,
        resume_handshake(
            &ticket.attach_token,
            applied + 1,
            ticket.server_epoch,
            ticket.tab_generation,
        ),
    )
    .await
    .expect("an evicted resume is served, not refused");
    assert_eq!(accepted.mode, AttachMode::Snapshot);
    assert!(
        accepted.seq > applied,
        "the snapshot fences at the tab's current seq, far past the evicted one"
    );
    data.read_snapshot().await;
}

/// EXIT is the last frame on a resumed connection too: the whole ring
/// slice goes out ahead of it, never interleaved with it and never lost
/// to it.
///
/// The tab is killed before a single frame is read, so the replay and
/// the exit are both waiting on the pump when it starts — the pump
/// writes its framed batch (step 3) before it will write EXIT (step 4),
/// which is what this asserts. Which pass EXIT lands in is scheduling
/// and not pinned here; the ordering is.
///
/// The remaining shape — a tab that died *before* the handshake, so the
/// handoff carries `stored_exit` and the pump absorbs it right behind
/// the slice — takes this same code path but is not reachable from a
/// test: the supervisor drops a dead tab's task handle *before* the exit
/// is published, so a resume that arrives after the death finds no
/// pipeline and falls back (and its `tab.attach` would already have been
/// refused). Only the reap's own race window can produce it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_connection_replays_its_slice_then_exits() {
    let h = harness().await;
    let (mut client, lease, tab_id, applied) = h.caught_up().await;

    let missed = b"ROOST_LAST_WORDS\r\n";
    feed(&mut client, tab_id, missed.to_vec()).await;
    wait_for_dump(&mut client, tab_id, "the missed bytes to land", |d| {
        d.rows_text
            .iter()
            .any(|row| row.contains("ROOST_LAST_WORDS"))
    })
    .await;

    let ticket = attach(&mut client, &lease, tab_id).await;
    let (accepted, mut data) = dial(
        &h.socket,
        resume_handshake(
            &ticket.attach_token,
            applied + 1,
            ticket.server_epoch,
            ticket.tab_generation,
        ),
    )
    .await
    .expect("accepted");
    assert_eq!(accepted.mode, AttachMode::Resume);

    // Before any frame is read: the slice is already in the pump's hands
    // (the handoff ran during the handshake) and the exit arrives on the
    // subscription that came with it.
    client
        .call::<_, serde_json::Value>(ops::TAB_CLOSE, TabCloseParams { tab_id })
        .await
        .expect("tab.close");

    let mut last_seq = applied;
    let mut replayed = Vec::new();
    let exit = loop {
        let frame = data.frame().await;
        match frame.frame_type {
            FRAME_PTY => {
                let (seq, bytes) = split_pty(&frame);
                assert_eq!(seq, last_seq + 1, "PTY frames stay contiguous");
                last_seq = seq;
                replayed.extend_from_slice(&bytes);
            }
            FRAME_EXIT => break frame,
            other => panic!("a resumed stream carries no frame {other:#04x} before EXIT"),
        }
    };
    assert_eq!(
        replayed, missed,
        "every record the client missed precedes EXIT"
    );
    let final_seq = u64::from_le_bytes(exit.payload[..8].try_into().unwrap());
    assert_eq!(final_seq, last_seq + 1);
    assert!(data.next().await.is_none(), "EXIT is the last frame");
}
