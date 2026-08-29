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

use roost_engine::ipc::{IpcHandler, SessionInfo, StopHandle, MAX_OUTSTANDING_TOKENS};
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
    TabOpenResult, SESSION_PROTOCOL_VERSION,
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
}

async fn harness() -> Harness {
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
            payload_kinds: vec![AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT)],
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
                SessionConnectParams { takeover: false },
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
        lease: lease.to_string(),
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

async fn dump(client: &mut IpcClient, tab_id: i64) -> TabDumpResult {
    client
        .call(ops::TAB_DUMP, TabDumpParams { tab_id })
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
                    tab_id,
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

    let mut no_lease = attach_params("", tab_id);
    no_lease.tab_id = tab_id;
    assert_eq!(
        attach_with(&mut client, no_lease).await.unwrap_err(),
        "connect-required",
        "the lease is checked before anything else"
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

/// A second admitted handshake for the same tab takes it over; the first
/// forwarder is told why rather than just going quiet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_attach_supersedes_the_first() {
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab().await;
    let first = attach(&mut client, &lease, tab_id).await;
    let (_accepted, mut old) = dial(&h.socket, handshake(&first.attach_token))
        .await
        .expect("accepted");
    old.read_snapshot().await;

    let second = attach(&mut client, &lease, tab_id).await;
    let (_accepted, mut new) = dial(&h.socket, handshake(&second.attach_token))
        .await
        .expect("accepted");

    assert_eq!(error_of(&old.frame().await).code, "superseded");
    assert!(old.next().await.is_none());
    // The replacement is untouched and still streaming.
    new.read_snapshot().await;
}

/// A supersede that lands mid-snapshot has to end the stream where it
/// is. The close watch is checked at the top of every pump pass, so the
/// displaced client stops receiving within one pass rather than riding
/// the rest of a multi-megabyte catch-up to EXIT.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_supersede_mid_snapshot_ends_the_stream_with_an_error() {
    // Wide and full: the encoded snapshot's size follows the characters
    // the terminal holds, and this one has to be too big to hand over
    // before the supersede lands.
    const COLS: u32 = 1_000;
    let h = harness().await;
    let (mut client, lease, tab_id) = h.leased_tab_sized(COLS, 24).await;

    let mut history = Vec::new();
    for row in 0..2_100u32 {
        history.extend_from_slice(format!("{row:.<998}\r\n").as_bytes());
    }
    history.extend_from_slice(b"ROOST_HISTORY_DONE\r\n");
    feed(&mut client, tab_id, history).await;
    wait_for_dump(&mut client, tab_id, "the scrollback to land", |d| {
        d.rows_text
            .iter()
            .any(|row| row.contains("ROOST_HISTORY_DONE"))
    })
    .await;

    let mut wide = attach_params(&lease, tab_id);
    wide.cols = u16::try_from(COLS).unwrap();
    let first = attach_with(&mut client, wide.clone())
        .await
        .expect("tab.attach");
    let (_accepted, mut old) = dial(&h.socket, handshake(&first.attach_token))
        .await
        .expect("accepted");
    // One frame only: the snapshot is deliberately left unfinished.
    let mut seen = Vec::new();
    let frame = old.frame().await;
    assert_eq!(frame.frame_type, FRAME_SNAP);
    seen.extend_from_slice(&frame.payload);

    let second = attach_with(&mut client, wide).await.expect("tab.attach");
    let (_accepted, mut new) = dial(&h.socket, handshake(&second.attach_token))
        .await
        .expect("accepted");

    // Drain the displaced connection to its end. Whatever was already
    // in flight may still arrive; what must not is EXIT, which would
    // mean the forwarder kept working for a client that lost the tab.
    let error = loop {
        let frame = old.frame().await;
        match frame.frame_type {
            FRAME_SNAP => seen.extend_from_slice(&frame.payload),
            FRAME_PTY => continue,
            FRAME_ERROR => break error_of(&frame),
            other => panic!("a superseded stream must not carry frame {other:#04x}"),
        }
    };
    assert_eq!(error.code, "superseded");
    assert!(
        !has_tag(&seen, TAG_FINISH),
        "the close has to land mid-snapshot or this test proves nothing"
    );
    assert!(old.next().await.is_none(), "the connection closes after it");
    // The replacement is untouched and still streaming.
    new.read_snapshot().await;
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

    // Fed from a second connection so this test keeps reading while the
    // bytes flow: a client that stops reading during a burst this size
    // is a different test (the forwarder's overflow rule).
    let socket = h.socket.clone();
    let feeder = tokio::spawn(async move {
        let mut feeder = IpcClient::connect(&socket).await.expect("connect");
        feed(&mut feeder, tab_id, payload).await;
    });

    let mut seen = 0usize;
    let mut next_seq = None;
    while seen < expected {
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
    assert_eq!(seen, expected, "every fed byte arrives exactly once");
    timeout(BUDGET, feeder)
        .await
        .expect("the feed finishes")
        .ok();

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

/// A takeover invalidates the displaced lease's tickets, so it has to
/// drop them too: they are refused at admission anyway, and leaving them
/// in the registry would let a dead client's full quota lock the new
/// holder out for a whole TTL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_purges_the_displaced_leases_attach_tokens() {
    let h = harness().await;
    let (mut old_client, old_lease, tab_id) = h.leased_tab().await;

    let mut minted = Vec::new();
    for _ in 0..MAX_OUTSTANDING_TOKENS {
        minted.push(
            attach(&mut old_client, &old_lease, tab_id)
                .await
                .attach_token,
        );
    }
    assert_eq!(
        attach_with(&mut old_client, attach_params(&old_lease, tab_id))
            .await
            .unwrap_err(),
        "too-many-tokens",
        "the quota is what bounds the registry"
    );

    let mut new_client = h.control().await;
    let taken: SessionConnectResult = new_client
        .call(
            ops::SESSION_CONNECT,
            SessionConnectParams { takeover: true },
        )
        .await
        .expect("session.connect with takeover");

    // Immediately, with no expiry to wait out: the point of the purge.
    let ticket = attach(&mut new_client, &taken.lease, tab_id).await;

    // The displaced lease itself still answers `taken-over` — the
    // instruction its holder can act on.
    let mut stale = h.control().await;
    assert_eq!(
        attach_with(&mut stale, attach_params(&old_lease, tab_id))
            .await
            .unwrap_err(),
        "taken-over"
    );
    // Its tickets are gone rather than merely unusable, so the handshake
    // no longer recognizes them: `invalid-token` sends the client back
    // for a new one, which is where it learns it was taken over.
    let error = dial(&h.socket, handshake(&minted[0]))
        .await
        .expect_err("a purged ticket is not admissible");
    assert_eq!(error.code, "invalid-token");

    let (_accepted, mut data) = dial(&h.socket, handshake(&ticket.attach_token))
        .await
        .expect("the new lease's own ticket is accepted");
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
