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

use roost_engine::ipc::{IpcHandler, SessionInfo, StopHandle, MAX_DATA_CONNS_PER_SESSION};
use roost_engine::tab_task::{AttachPause, AttachReleases, ServerVtConfig, ServerVtWorkspace};
use roost_engine::{PtySupervisor, Workspace};
use roost_ipc::dataframe::{
    write_data_frame, DataFrame, DataFrameReader, FRAME_ERROR, FRAME_EXIT, FRAME_INPUT, FRAME_PTY,
    FRAME_RESIZE, FRAME_SNAP, MAX_DATA_FRAME_BYTES,
};
use roost_ipc::framing::{write_frame, FrameReader};
use roost_ipc::messages::{
    ops, AttachAccepted, AttachHandshakeReply, AttachMode, AttachPayloadKind, ResponseError,
    SessionStopParams, SessionStopResult, TabCapturePtyInputParams, TabCapturePtyInputResult,
    TabCloseParams, TabDumpParams, TabDumpResult, TabFeedPtyBytesParams, TabOpenParams,
    TabOpenResult, TabResizeParams, TabWriteParams, WireTabRef, SESSION_PROTOCOL_VERSION,
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
    /// The session's own identity, which every handshake has to
    /// name, and the registry the release-on-reject cases read.
    session_id: String,
    handler: Arc<IpcHandler>,
    /// The tabs' own supervisor, so the respawn tests can replace a
    /// tab's terminal the way a reap plus a fresh spawn does — there is
    /// no op that reuses a tab id.
    supervisor: Arc<PtySupervisor>,
    serving: tokio::task::AbortHandle,
    dir: TempDir,
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

const SESSION_ID: &str = "01K3S8TQ4F0Q9YB2K6WZ5D7XN";

/// What a shipped `roost-session` advertises.
const BOTH_KINDS: [&str; 2] = [AttachPayloadKind::GHOSTTY_SNAPSHOT, AttachPayloadKind::VT];

/// A session advertising both payload kinds.
async fn harness() -> Harness {
    harness_advertising(&BOTH_KINDS).await
}

/// The same, stating what `session.identify` advertises — the pre-`vt`
/// daemon shape is one entry, and negotiation is defined against the
/// advertisement, not against what the code can encode.
async fn harness_advertising(payload_kinds: &[&str]) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    harness_in(dir, SESSION_ID, payload_kinds).await
}

/// A session whose tab tasks park at every snapshot and resume hand-off
/// until this test lets them through — the only way to stand inside the
/// window between the forwarder's lookup and the task's own turn.
async fn harness_pausing_handoffs() -> (Harness, AttachReleases) {
    let (pause, releases) = AttachPause::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let h = harness_with(dir, SESSION_ID, &BOTH_KINDS, Some(Seam::Handoff(pause))).await;
    (h, releases)
}

/// The same, parking each admitted data connection between the
/// registration that captured its tab's identity and the resize that
/// acts on it.
async fn harness_pausing_admission() -> (Harness, AttachReleases) {
    let (pause, releases) = AttachPause::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let h = harness_with(dir, SESSION_ID, &BOTH_KINDS, Some(Seam::Admission(pause))).await;
    (h, releases)
}

/// Which window a test has asked to stand inside.
enum Seam {
    Admission(AttachPause),
    Handoff(AttachPause),
}

/// Bind a session into an existing directory under a stated identity —
/// what a restart at the same socket path looks like from a client's
/// side.
async fn harness_in(dir: TempDir, session_id: &str, payload_kinds: &[&str]) -> Harness {
    harness_with(dir, session_id, payload_kinds, None).await
}

async fn harness_with(
    dir: TempDir,
    session_id: &str,
    payload_kinds: &[&str],
    seam: Option<Seam>,
) -> Harness {
    let payload_kinds: Vec<AttachPayloadKind> = payload_kinds
        .iter()
        .copied()
        .map(AttachPayloadKind::from)
        .collect();
    let socket = dir.path().join("roost.sock");
    let workspace = Arc::new(Workspace::open(dir.path().join("state.json")));
    let supervisor = Arc::new(PtySupervisor::new());
    let mut config = ServerVtConfig::new(Arc::new(NoopWorkspace) as Arc<dyn ServerVtWorkspace>)
        .with_input_capture(true);
    match seam {
        Some(Seam::Admission(pause)) => config = config.with_admission_pause(pause),
        Some(Seam::Handoff(pause)) => config = config.with_handoff_pause(pause),
        None => {}
    }
    supervisor
        .enable_server_vt(config)
        .expect("server-vt enables once");

    let handler = IpcHandler::new(
        Arc::clone(&workspace),
        Arc::clone(&supervisor),
        socket.clone(),
        "Roost-test",
        "ai.stridelabs.Roost.test",
    )
    .with_session(
        SessionInfo {
            session_id: session_id.into(),
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
    let handler = Arc::clone(server.handler());
    let serving = tokio::spawn(async move {
        let _ = server.run().await;
    })
    .abort_handle();
    Harness {
        socket,
        workspace,
        session_id: session_id.to_string(),
        handler,
        supervisor,
        serving,
        dir,
    }
}

impl Harness {
    async fn control(&self) -> IpcClient {
        IpcClient::connect(&self.socket).await.expect("connect")
    }

    /// The handshake this session would accept for `tab_id`, spelled
    /// out as JSON so a test can lie about exactly one term.
    fn handshake(&self, tab_id: i64) -> serde_json::Value {
        self.sized_handshake(tab_id, (80, 24), (0, 0), true)
    }

    /// The same at an explicit viewport. `focus` is the attach-time
    /// geometry claim: `true` resizes the tab, `false` attaches at
    /// whatever size it already is.
    fn sized_handshake(
        &self,
        tab_id: i64,
        (cols, rows): (u16, u16),
        (cell_w_px, cell_h_px): (u16, u16),
        focus: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "attach": tab_id.to_string(),
            "protocol_version": SESSION_PROTOCOL_VERSION,
            "session_id": self.session_id,
            "kinds": [AttachPayloadKind::GHOSTTY_SNAPSHOT],
            "cols": cols,
            "rows": rows,
            "cell_w_px": cell_w_px,
            "cell_h_px": cell_h_px,
            "libghostty_build": roost_vt::libghostty_build(),
            "focus": focus,
        })
    }

    /// The same handshake, plus the resume triple. Every field is
    /// spelled out by the caller so a test can lie about exactly one of
    /// them.
    fn resume_handshake(
        &self,
        tab_id: i64,
        from_seq: u64,
        server_epoch: u64,
        tab_generation: u64,
    ) -> serde_json::Value {
        let mut line = self.handshake(tab_id);
        line["resume_from_seq"] = serde_json::json!(from_seq);
        line["server_epoch"] = serde_json::json!(server_epoch);
        line["tab_generation"] = serde_json::json!(tab_generation);
        line
    }

    /// Stop serving and bind a replacement session at the same socket
    /// path — a restart, from a client's side.
    async fn restart(self) -> Harness {
        let Harness { serving, dir, .. } = self;
        serving.abort();
        // The abort leaves the socket file behind; `bind` probes it,
        // finds nothing listening and unlinks it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        harness_in(dir, "01K9RESTARTED0000000000000", &BOTH_KINDS).await
    }

    /// Replace this tab's terminal with a second one under the same id:
    /// the reap-then-respawn `pty.rs` supports before an old waiter has
    /// finished. The forwarder's cloned command sender keeps the old
    /// task alive and serving, which is the whole point.
    fn respawn(&self, tab_id: i64) {
        self.supervisor.close(tab_id);
        self.supervisor
            .spawn(
                tab_id,
                "/tmp",
                &["/bin/sh".into(), "-c".into(), "exec cat".into()],
                80,
                24,
                &self.socket,
            )
            .expect("the tab id is free once close() has taken the slot");
    }

    /// Poll the session's data-connection registry until it reads
    /// `want`. A count is the server's own bookkeeping and moves on the
    /// forwarder's task, so it is waited for rather than asserted at an
    /// instant the test cannot pin.
    async fn wait_for_data_conns(&self, want: usize) {
        let deadline = Instant::now() + BUDGET;
        loop {
            let live = self.handler.data_conn_count();
            if live == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the session held {live} data connections, not {want}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A control connection plus a tab running a real shell at an
    /// explicit geometry — the one child that can be asked what size it
    /// thinks it is.
    async fn shell_tab(&self, cols: u32, rows: u32) -> (IpcClient, i64) {
        self.open_tab(cols, rows, vec!["/bin/sh".into()]).await
    }

    /// A control connection plus a live tab parked on `cat` — a child
    /// that keeps its PTY open and echoes nothing on its own, so every
    /// byte on the wire is one the test caused.
    async fn live_tab(&self) -> (IpcClient, i64) {
        self.live_tab_sized(0, 0).await
    }

    /// The same, at an explicit geometry. Width is what a snapshot's
    /// size follows, so the one test that needs a multi-frame snapshot
    /// asks for a wide tab rather than trying to type its way there.
    async fn live_tab_sized(&self, cols: u32, rows: u32) -> (IpcClient, i64) {
        self.open_tab(
            cols,
            rows,
            vec!["/bin/sh".into(), "-c".into(), "exec cat".into()],
        )
        .await
    }

    async fn open_tab(&self, cols: u32, rows: u32, argv: Vec<String>) -> (IpcClient, i64) {
        let mut client = self.control().await;
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
                    argv,
                    cols,
                    rows,
                    title: String::new(),
                    activate: None,
                    cwd_from_tab: None,
                },
            )
            .await
            .expect("tab.open");
        (client, opened.tab.id)
    }

    /// The preamble every "what happens on a live connection" test
    /// shares: a control connection, a tab attached at the default
    /// geometry, and a data connection that has already read its
    /// snapshot through FINISH — so the next frame is whatever the test
    /// causes.
    async fn attached(&self) -> (IpcClient, i64, DataClient) {
        let (client, tab_id) = self.live_tab().await;
        let (_accepted, mut data) = dial(&self.socket, self.handshake(tab_id))
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
        tab_id: i64,
        grid: (u16, u16),
        cell: (u16, u16),
        focus: bool,
    ) -> (AttachAccepted, DataClient) {
        let (accepted, mut data) = dial(
            &self.socket,
            self.sized_handshake(tab_id, grid, cell, focus),
        )
        .await
        .expect("accepted");
        data.read_snapshot().await;
        (accepted, data)
    }

    /// A tab that has been attached once and has gone quiet again, with
    /// the data connection dropped the way a client's would be. The seq
    /// is the last record that client applied — what it would carry into
    /// `resume_from_seq + 1` — and the accepted reply is where the
    /// identity a resume hands back comes from, now that there is no
    /// control leg to learn it on.
    async fn caught_up(&self) -> (IpcClient, i64, u64, AttachAccepted) {
        let (mut client, tab_id) = self.live_tab().await;
        let (accepted, mut data) = dial(&self.socket, self.handshake(tab_id))
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
        (client, tab_id, applied, accepted)
    }
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

/// Wait until a tab task has reached a hand-off. It stays parked there
/// until the returned sender is used or dropped, which is what lets a
/// test respawn the tab from inside the window.
async fn parked(releases: &mut AttachReleases) -> tokio::sync::oneshot::Sender<()> {
    timeout(BUDGET, releases.recv())
        .await
        .expect("a tab task reaches its hand-off in time")
        .expect("the seam outlives the tasks parked on it")
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

/// Bytes into the tab from the CONTROL plane, which states no geometry
/// of its own — the only way to reach a child without also sizing it.
async fn write_tab(client: &mut IpcClient, tab_id: i64, data: &[u8]) {
    client
        .call::<_, serde_json::Value>(
            ops::TAB_WRITE,
            TabWriteParams {
                tab_id,
                data: data.to_vec(),
            },
        )
        .await
        .expect("tab.write");
}

async fn resize(client: &mut IpcClient, tab_id: i64, cols: u32, rows: u32) {
    client
        .call::<_, serde_json::Value>(ops::TAB_RESIZE, TabResizeParams { tab_id, cols, rows })
        .await
        .expect("tab.resize");
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

/// One attach, end to end: the handshake names the tab and the terms,
/// the accepted line names the identity every seq is scoped to, the
/// snapshot arrives READY-first, and every live PTY frame after it is
/// contiguous from the fence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_snapshot_attach_streams_ready_then_finish_then_live_frames() {
    let h = harness().await;
    let (mut client, tab_id) = h.live_tab().await;

    let (accepted, mut data) = dial(&h.socket, h.handshake(tab_id))
        .await
        .expect("the handshake is accepted");
    assert_eq!(accepted.mode, AttachMode::Snapshot);
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
    let (mut client, tab_id) = h.live_tab().await;

    // A focused attach resizes the tab to the geometry the handshake
    // asked for, before anything is encoded from it.
    let (_accepted, mut data) = dial(
        &h.socket,
        h.sized_handshake(tab_id, (100, 30), (0, 0), true),
    )
    .await
    .expect("accepted");
    wait_for_dump(&mut client, tab_id, "the attach geometry", |d| {
        (d.cols, d.rows) == (100, 30)
    })
    .await;
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
    let (mut client, tab_id) = h.live_tab().await;
    let (accepted, mut data) = dial(&h.socket, h.handshake(tab_id))
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
    let (_client, tab_id) = h.live_tab().await;
    let skewed = "ghostty-0000000000000000+snapshot.v1";

    let both = |build: &str| {
        let mut line = h.handshake(tab_id);
        line["kinds"] =
            serde_json::json!([AttachPayloadKind::GHOSTTY_SNAPSHOT, AttachPayloadKind::VT]);
        line["libghostty_build"] = serde_json::json!(build);
        line
    };

    let (accepted, _data) = dial(&h.socket, both(&roost_vt::libghostty_build()))
        .await
        .expect("a matching build serves its first choice");
    assert_eq!(
        accepted.kind.as_str(),
        AttachPayloadKind::GHOSTTY_SNAPSHOT,
        "GHOSTSNP is preferred whenever it is eligible"
    );

    let (accepted, _data) = dial(&h.socket, both(skewed))
        .await
        .expect("the skew falls through to the next offer");
    assert_eq!(
        accepted.kind.as_str(),
        AttachPayloadKind::VT,
        "an ineligible first choice must not refuse an eligible second one"
    );

    for build in [roost_vt::libghostty_build(), skewed.to_string()] {
        let mut vt_only = h.handshake(tab_id);
        vt_only["kinds"] = serde_json::json!([AttachPayloadKind::VT]);
        vt_only["libghostty_build"] = serde_json::json!(build);
        let (accepted, _data) = dial(&h.socket, vt_only)
            .await
            .expect("vt has no build requirement");
        assert_eq!(
            accepted.kind.as_str(),
            AttachPayloadKind::VT,
            "vt is a byte stream, so {build:?} is not its business"
        );
    }

    // And a client that has never heard of `vt` still has nowhere to
    // fall back to when the builds disagree.
    let mut ghostsnp_only = h.handshake(tab_id);
    ghostsnp_only["libghostty_build"] = serde_json::json!(skewed);
    assert_eq!(
        dial(&h.socket, ghostsnp_only)
            .await
            .expect_err("nowhere to fall back to")
            .code,
        "build-mismatch"
    );
}

/// A session that does not advertise `vt` cannot be talked into it, even
/// though this build can encode one: the advertisement is the contract
/// the client negotiated against.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unadvertised_kind_is_not_servable() {
    let h = harness_advertising(&[AttachPayloadKind::GHOSTTY_SNAPSHOT]).await;
    let (_client, tab_id) = h.live_tab().await;

    let mut vt_only = h.handshake(tab_id);
    vt_only["kinds"] = serde_json::json!([AttachPayloadKind::VT]);
    assert_eq!(
        dial(&h.socket, vt_only).await.unwrap_err().code,
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
    let (_client, tab_id) = h.live_tab().await;

    let mut mystery = h.handshake(tab_id);
    mystery["kinds"] = serde_json::json!(["sixel-mosaic-v9"]);
    assert_eq!(dial(&h.socket, mystery).await.unwrap_err().code, "internal");
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
    let (mut client, tab_id) = h.live_tab().await;

    let (a_accepted, mut a) = dial(&h.socket, h.handshake(tab_id))
        .await
        .expect("accepted");
    let (_snapshot, pty) = a.read_snapshot().await;
    let a_at = pty.last().map_or(a_accepted.seq, |(seq, _)| *seq);

    let (b_accepted, mut b) = dial(&h.socket, h.handshake(tab_id))
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
    let (mut client, tab_id) = h.live_tab().await;

    let (_accepted, mut going) = dial(&h.socket, h.handshake(tab_id))
        .await
        .expect("accepted");
    going.read_snapshot().await;

    let (accepted, mut data) = dial(&h.socket, h.handshake(tab_id))
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

/// A stop owes a data connection the same labeled close as a control
/// one. The registry's own walk is what pins it: this control
/// connection ran nothing but `tab.open`, and the data connection ran
/// no op at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_labels_a_data_connection_that_only_ever_attached() {
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
                activate: None,
                cwd_from_tab: None,
            },
        )
        .await
        .expect("tab.open");

    let (_accepted, mut data) = dial(&h.socket, h.handshake(opened.tab.id))
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

/// The pixel half of every size report for `grid` a captured stream
/// carries, as `(width_px, height_px)`. The whole report is
/// `CSI 48 ; rows ; cols ; height_px ; width_px t`, so a test that cares
/// what libghostty holds for the cell metrics has to read past where
/// [`size_reports`] stops.
fn size_report_pixels(captured: &[u8], (cols, rows): (u16, u16)) -> Vec<(u32, u32)> {
    let text = String::from_utf8_lossy(captured).into_owned();
    let head = format!("\x1b[48;{rows};{cols};");
    text.match_indices(&head)
        .filter_map(|(at, _)| {
            let (params, _) = text[at + head.len()..].split_once('t')?;
            let (height, width) = params.split_once(';')?;
            Some((width.parse().ok()?, height.parse().ok()?))
        })
        .collect()
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
    let (mut client, tab_id) = h.live_tab().await;

    let (_desktop, _a) = h.attached_at(tab_id, (100, 30), (8, 16), true).await;
    let (_phone, mut b) = h.attached_at(tab_id, (60, 20), (8, 16), false).await;
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
    let (mut client, tab_id) = h.live_tab().await;

    let (_accepted, mut same) = h.attached_at(tab_id, (100, 30), (8, 16), true).await;
    let (_accepted, mut wider_cells) = h.attached_at(tab_id, (100, 30), (9, 16), false).await;

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

/// `tab.resize` states two of the four numbers, so the tab keeps the
/// cell metrics its last geometry-bearing client declared — and the
/// report it emits quotes the child real pixels rather than `0x0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grid_resize_keeps_the_cell_metrics_a_client_declared() {
    let h = harness().await;
    let (mut client, tab_id) = h.live_tab().await;

    let (_accepted, _data) = h.attached_at(tab_id, (100, 30), (9, 18), true).await;
    watch_size_reports(&mut client, tab_id).await;

    let mut other = h.control().await;
    resize(&mut other, tab_id, 80, 24).await;

    let captured = read_pty_input_until(&mut client, tab_id, &[b"48;24;80"]).await;
    assert_eq!(
        size_report_pixels(&captured, (80, 24)),
        vec![(720, 432)],
        "the report quotes 80x9 by 24x18 pixels ({:?})",
        String::from_utf8_lossy(&captured)
    );
}

/// And because the metrics are kept, naming the grid an attached client
/// is already at is the no-op `ipc.md` promises: the whole-tuple
/// compare sees the same four numbers rather than two of them against a
/// pair of zeros.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grid_resize_to_the_size_the_tab_already_has_reports_nothing() {
    let h = harness().await;
    let (mut client, tab_id) = h.live_tab().await;

    let (_accepted, _data) = h.attached_at(tab_id, (100, 30), (9, 18), true).await;
    watch_size_reports(&mut client, tab_id).await;

    // The second resize is the fence: both ride the tab's one command
    // channel, so its report cannot arrive ahead of one the first would
    // have emitted.
    let mut other = h.control().await;
    resize(&mut other, tab_id, 100, 30).await;
    resize(&mut other, tab_id, 60, 20).await;

    let captured = read_pty_input_until(&mut client, tab_id, &[b"48;20;60"]).await;
    assert_eq!(
        size_reports(&captured, (100, 30)),
        0,
        "the grid it is already at is not re-applied ({:?})",
        String::from_utf8_lossy(&captured)
    );
}

/// Two clients typing into one tab do not race for the PTY's size: each
/// INPUT applies its own geometry, and the tab ends up wherever the last
/// command the task *received* asked for — not the last one sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_senders_linearize_in_receive_order() {
    let h = harness().await;
    let (mut client, tab_id) = h.live_tab().await;

    let (_accepted, mut desktop) = h.attached_at(tab_id, (100, 30), (8, 16), true).await;
    let (_accepted, mut phone) = h.attached_at(tab_id, (60, 20), (8, 16), false).await;

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

/// The handshake refuses a zero-sized grid, and a RESIZE frame states the
/// same client's geometry, so the two agree: the frame is dropped rather
/// than applied, and the connection carries on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_sized_resize_frame_is_ignored() {
    let h = harness().await;
    let (mut client, tab_id) = h.live_tab().await;
    let (_accepted, mut data) = h.attached_at(tab_id, (100, 30), (8, 16), true).await;

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
/// A focused attach hears it too. Its own resize lands before the
/// encode, but raw input is open and any other client may size the tab
/// in between, so "what I asked for" is never evidence of what the
/// encode composed — `a_resume_reports_the_geometry_the_ring_was_written_at`
/// is the case where the two differ.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unfocused_attach_never_resizes_and_reports_the_snapshot_geometry() {
    let h = harness().await;
    let (mut client, tab_id) = h.live_tab().await;

    let (desktop, _a) = h.attached_at(tab_id, (100, 30), (8, 16), true).await;
    assert_eq!(
        (desktop.snapshot_cols, desktop.snapshot_rows),
        (100, 30),
        "the size the payload was encoded at, which here is what was asked for"
    );

    let (phone, _b) = h.attached_at(tab_id, (60, 20), (8, 16), false).await;
    assert_eq!((phone.snapshot_cols, phone.snapshot_rows), (100, 30));
    let d = dump(&mut client, tab_id).await;
    assert_eq!(
        (d.cols, d.rows),
        (100, 30),
        "a client that is only watching cannot shrink the one that is typing"
    );
}

/// Reading is not an interaction. `tab.dump` carries no geometry, so it
/// leaves the tab's size alone however often it is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dump_never_resizes() {
    let h = harness().await;
    let (mut client, tab_id) = h.live_tab().await;
    let (_accepted, mut data) = h.attached_at(tab_id, (100, 30), (8, 16), true).await;

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

// ---------------------------------------------------------------------
// Resume — the ring instead of a snapshot, and every way it falls back
// ---------------------------------------------------------------------

/// The hit: a client that was away for a few records gets exactly those
/// records back, as ordinary PTY frames, with no snapshot in sight — and
/// the live tee continues from them without a seam.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_replays_the_ring_and_sends_no_snapshot() {
    let h = harness().await;
    let (mut client, tab_id, applied, identity) = h.caught_up().await;

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

    let (accepted, mut data) = dial(
        &h.socket,
        h.resume_handshake(
            tab_id,
            applied + 1,
            identity.server_epoch,
            identity.tab_generation,
        ),
    )
    .await
    .expect("accepted");
    assert_eq!(accepted.mode, AttachMode::Resume);
    assert_eq!(accepted.seq, applied, "the fence is resume_from_seq - 1");
    assert_eq!(accepted.server_epoch, identity.server_epoch);
    assert_eq!(accepted.tab_generation, identity.tab_generation);

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

/// A resume names the tab's geometry too, read at the handoff (review
/// F1).
///
/// A resuming client replays the ring into the terminal it *kept*, and
/// the shared grid can have moved while it was away — every client that
/// types sizes the tab, and this one was not there to see it. Without
/// the answer it lays those records out at the width it left, wrapping
/// every line and misplacing every absolute cursor move until something
/// resizes it locally.
///
/// The dial is unfocused on purpose: that is the shape where the tab's
/// grid at the handoff is somebody else's and not this client's own. A
/// focused resume states its geometry, so the handoff would find the
/// tab already at it and the assertion would say nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_reports_the_geometry_the_ring_was_written_at() {
    let h = harness().await;
    let (mut client, tab_id, applied, identity) = h.caught_up().await;

    // Somebody else keeps typing at their own size while this client is
    // still dialing back in.
    let mut other = h.control().await;
    resize(&mut other, tab_id, 60, 20).await;
    wait_for_dump(&mut client, tab_id, "the other client's resize", |d| {
        (d.cols, d.rows) == (60, 20)
    })
    .await;

    let mut watching = h.resume_handshake(
        tab_id,
        applied + 1,
        identity.server_epoch,
        identity.tab_generation,
    );
    watching["cols"] = serde_json::json!(100);
    watching["rows"] = serde_json::json!(30);
    watching["focus"] = serde_json::json!(false);

    let (accepted, _data) = dial(&h.socket, watching).await.expect("accepted");
    assert_eq!(accepted.mode, AttachMode::Resume);
    assert_eq!(
        (accepted.snapshot_cols, accepted.snapshot_rows),
        (60, 20),
        "a resume names the grid its records were written at, not the one asked for"
    );
}

/// `last_assigned + 1` is a hit, not a miss: the client missed nothing,
/// and an empty slice is the honest answer. It must not be turned into a
/// snapshot the client already has.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_slice_resume_is_a_hit() {
    let h = harness().await;
    let (mut client, tab_id, applied, identity) = h.caught_up().await;

    let (accepted, mut data) = dial(
        &h.socket,
        h.resume_handshake(
            tab_id,
            applied + 1,
            identity.server_epoch,
            identity.tab_generation,
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
/// The generation check the resume path also runs — against the one the
/// admission read — is not here: it needs a respawn inside the handoff
/// window, which `a_respawn_around_the_resume_handoff_is_refused`
/// reaches through the hand-off seam.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unhonorable_resume_triple_falls_back_to_a_snapshot() {
    let h = harness().await;
    let (_client, tab_id, applied, identity) = h.caught_up().await;

    // Seq 0: a client holding nothing is asking for everything.
    falls_back(
        &h.socket,
        h.resume_handshake(tab_id, 0, identity.server_epoch, identity.tab_generation),
        "seq 0",
    )
    .await;

    // The triple is all-or-nothing: a seq with no identity beside it
    // names a stream on no particular server.
    let mut bare = h.handshake(tab_id);
    bare["resume_from_seq"] = serde_json::json!(applied + 1);
    falls_back(&h.socket, bare, "a resume with no identity").await;

    // A seq the tab has not reached yet: the client claims to hold
    // records that do not exist, which is the one direction a replay
    // could never fix.
    falls_back(
        &h.socket,
        h.resume_handshake(
            tab_id,
            applied + 1_000_000,
            identity.server_epoch,
            identity.tab_generation,
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
    let (_client, tab_id, applied, identity) = h.caught_up().await;

    let (accepted, mut data) = dial(
        &h.socket,
        h.resume_handshake(
            tab_id,
            applied + 1,
            identity.server_epoch,
            identity.tab_generation + 1,
        ),
    )
    .await
    .expect("a stale resume is served, not refused");
    assert_eq!(accepted.mode, AttachMode::Snapshot);
    assert_eq!(
        accepted.tab_generation, identity.tab_generation,
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
    let (_client, tab_id, applied, identity) = h.caught_up().await;

    let (accepted, mut data) = dial(
        &h.socket,
        h.resume_handshake(
            tab_id,
            applied + 1,
            identity.server_epoch ^ 1,
            identity.tab_generation,
        ),
    )
    .await
    .expect("a stale resume is served, not refused");
    assert_eq!(accepted.mode, AttachMode::Snapshot);
    assert_eq!(accepted.server_epoch, identity.server_epoch);
    data.read_snapshot().await;
}

/// A client away long enough for its seq to fall out of the 2 MiB ring
/// cannot be replayed — the records are gone. It pays for a snapshot
/// rather than being told off.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seq_the_ring_no_longer_covers_falls_back_to_a_snapshot() {
    let h = harness().await;
    let (mut client, tab_id, applied, identity) = h.caught_up().await;

    let mut flood = vec![b'.'; 3 * 1024 * 1024];
    flood.extend_from_slice(b"\r\nROOST_FLOOD_DONE\r\n");
    feed(&mut client, tab_id, flood).await;
    wait_for_dump(&mut client, tab_id, "the ring to be overrun", |d| {
        d.rows_text
            .iter()
            .any(|row| row.contains("ROOST_FLOOD_DONE"))
    })
    .await;

    let (accepted, mut data) = dial(
        &h.socket,
        h.resume_handshake(
            tab_id,
            applied + 1,
            identity.server_epoch,
            identity.tab_generation,
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
/// pipeline and falls back (and its handshake would already have been
/// refused). Only the reap's own race window can produce it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_connection_replays_its_slice_then_exits() {
    let h = harness().await;
    let (mut client, tab_id, applied, identity) = h.caught_up().await;

    let missed = b"ROOST_LAST_WORDS\r\n";
    feed(&mut client, tab_id, missed.to_vec()).await;
    wait_for_dump(&mut client, tab_id, "the missed bytes to land", |d| {
        d.rows_text
            .iter()
            .any(|row| row.contains("ROOST_LAST_WORDS"))
    })
    .await;

    let (accepted, mut data) = dial(
        &h.socket,
        h.resume_handshake(
            tab_id,
            applied + 1,
            identity.server_epoch,
            identity.tab_generation,
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

// ---------------------------------------------------------------------
// The handshake — the data connection negotiates for itself
// ---------------------------------------------------------------------

/// One connection and one line: the terms ride the handshake and the
/// reply names what was negotiated. No control round trip exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handshake_attaches_with_no_control_round_trip() {
    let h = harness().await;
    let (_client, tab_id) = h.live_tab().await;

    let (accepted, mut data) = dial(&h.socket, h.handshake(tab_id))
        .await
        .expect("the handshake is accepted");
    assert_eq!(accepted.kind.as_str(), AttachPayloadKind::GHOSTTY_SNAPSHOT);
    assert_eq!(accepted.mode, AttachMode::Snapshot);
    assert_eq!((accepted.snapshot_cols, accepted.snapshot_rows), (80, 24));
    let (snapshot, _pty) = data.read_snapshot().await;
    assert!(has_tag(&snapshot, TAG_READY), "the payload carries READY");
}

/// The control op is gone: there is no second leg to an attach, and a
/// client still calling one is told so by name rather than being handed
/// a ticket nothing would consume.
///
/// Spelled out rather than taken from a constant — the constant is
/// deleted, and what this pins is the name on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tab_attach_is_not_an_op_any_more() {
    let h = harness().await;
    let (_client, tab_id) = h.live_tab().await;
    let mut client = h.control().await;

    let refused = client
        .call_raw(
            "tab.attach",
            serde_json::json!({
                "tab_id": tab_id.to_string(),
                "kinds": [AttachPayloadKind::GHOSTTY_SNAPSHOT],
                "cols": 80,
                "rows": 24,
                "libghostty_build": roost_vt::libghostty_build(),
                "focus": true,
            }),
        )
        .await
        .expect_err("tab.attach is not served any more");
    assert_eq!(
        refused.server_code(),
        Some(roost_ipc::client::ServerCode::UnknownOp)
    );
}

/// `attach` names a tab and nothing else. It carried a bearer token
/// until this generation, so the distinction is worth stating: anything
/// that is not a tab id is `invalid-param` and never a credential this
/// session goes looking for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_is_a_tab_id_and_a_malformed_one_says_so() {
    let h = harness().await;
    let (_client, tab_id) = h.live_tab().await;

    let mut tokenish = h.handshake(tab_id);
    tokenish["attach"] = serde_json::json!("1a0be5c37d924f68b1c05e3a7f2d8496");
    let error = dial(&h.socket, tokenish)
        .await
        .expect_err("a hex string is not a tab id");
    assert_eq!(error.code, "invalid-param");

    // `kinds` is an ordinary required term now: absent or null, the
    // line is a `parse-error` that names it.
    for absent in [None, Some(serde_json::Value::Null)] {
        let mut line = h.handshake(tab_id);
        match absent {
            None => {
                line.as_object_mut().unwrap().remove("kinds");
            }
            Some(null) => line["kinds"] = null,
        }
        let error = dial(&h.socket, line)
            .await
            .expect_err("a handshake with no kinds states no kind to serve");
        assert_eq!(error.code, "parse-error");
        assert!(
            error.message.contains("kinds"),
            "the rejection names the term: {}",
            error.message
        );
    }

    // And present-and-empty is a preference order with nothing in it,
    // which is a negotiation failure and not a decode one.
    let mut empty = h.handshake(tab_id);
    empty["kinds"] = serde_json::json!([]);
    assert_eq!(
        dial(&h.socket, empty)
            .await
            .expect_err("no kind to serve")
            .code,
        "unsupported-kind"
    );
}

/// The generation check runs on the raw line, ahead of the typed decode:
/// two ends that disagree about the generation disagree about what every
/// other field means, so naming a malformed term first would send a
/// version-skewed client chasing the wrong bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_protocol_wins_over_malformed_terms() {
    let h = harness().await;
    let (_client, tab_id) = h.live_tab().await;

    let mut both_wrong = h.handshake(tab_id);
    both_wrong["protocol_version"] = serde_json::json!(1);
    both_wrong["cols"] = serde_json::json!("eighty");
    both_wrong.as_object_mut().unwrap().remove("session_id");

    let error = dial(&h.socket, both_wrong)
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

/// A handshake is all-or-nothing, and a term it left out is a
/// `parse-error` that names the term.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handshake_missing_a_term_is_a_parse_error_naming_it() {
    let h = harness().await;
    let (_client, tab_id) = h.live_tab().await;

    let mut no_session = h.handshake(tab_id);
    no_session.as_object_mut().unwrap().remove("session_id");
    let error = dial(&h.socket, no_session)
        .await
        .expect_err("session_id is required");
    assert_eq!(error.code, "parse-error");
    assert!(
        error.message.contains("session_id"),
        "the rejection names the missing term: {}",
        error.message
    );
}

/// A newer client's extra term is tolerated: the handshake is the one
/// permissive request on this wire, which is what makes a future term
/// additive instead of a generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handshake_tolerates_a_term_this_build_never_heard_of() {
    let h = harness().await;
    let (_client, tab_id) = h.live_tab().await;

    let mut newer = h.handshake(tab_id);
    newer["viewport_hint"] = serde_json::json!({"top": 0});
    let (accepted, _data) = dial(&h.socket, newer)
        .await
        .expect("an unknown term is not a refusal");
    assert_eq!(accepted.kind.as_str(), AttachPayloadKind::GHOSTTY_SNAPSHOT);
}

/// The identity the ticket used to carry. Without it a dial released
/// after a drop could land on a replacement session listening at the
/// same socket path — so the check runs before anything is registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_restarted_under_the_socket_refuses_the_old_identity() {
    let first = harness().await;
    let (_client, tab_id) = first.live_tab().await;
    // Prepared against the session that was there, the way a client's
    // queued dial is.
    let prepared = first.handshake(tab_id);

    let replacement = first.restart().await;
    let (_client, live_tab) = replacement.live_tab().await;

    // Re-aimed at a tab the replacement really has, so the session
    // identity is the only thing left that can refuse it.
    let mut prepared = prepared;
    prepared["attach"] = serde_json::Value::String(live_tab.to_string());
    let error = dial(&replacement.socket, prepared)
        .await
        .expect_err("a dial prepared for the session that went away");
    assert_eq!(error.code, "session-mismatch");
    replacement.wait_for_data_conns(0).await;
}

/// The refusal order, top to bottom: each code names a different thing
/// for the client to fix, so an earlier failure must never be masked by
/// a later one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_refusals_keep_their_order() {
    let h = harness().await;
    let (_client, tab_id) = h.live_tab().await;

    // Wrong session AND a missing tab AND an unservable kind: identity
    // first.
    let mut wrong_session = h.handshake(tab_id + 9_999);
    wrong_session["session_id"] = serde_json::json!("01KSOMEBODYELSE0000000000");
    wrong_session["kinds"] = serde_json::json!(["sixel-mosaic-v9"]);
    assert_eq!(
        dial(&h.socket, wrong_session).await.unwrap_err().code,
        "session-mismatch"
    );

    // A missing tab AND an unservable kind: the tab first.
    let mut missing_tab = h.handshake(tab_id + 9_999);
    missing_tab["kinds"] = serde_json::json!(["sixel-mosaic-v9"]);
    assert_eq!(
        dial(&h.socket, missing_tab).await.unwrap_err().code,
        "not-found"
    );

    // An unservable kind AND a zero grid: the kind first.
    let mut unservable = h.handshake(tab_id);
    unservable["kinds"] = serde_json::json!(["sixel-mosaic-v9"]);
    unservable["rows"] = serde_json::json!(0);
    assert_eq!(
        dial(&h.socket, unservable).await.unwrap_err().code,
        "unsupported-kind"
    );

    // A build skew AND a zero grid: the build first.
    let mut skewed = h.handshake(tab_id);
    skewed["libghostty_build"] = serde_json::json!("ghostty-0000000000000000+snapshot.v1");
    skewed["rows"] = serde_json::json!(0);
    assert_eq!(
        dial(&h.socket, skewed).await.unwrap_err().code,
        "build-mismatch"
    );

    // And then the grid, which is checked for an unfocused attach too:
    // it is still the geometry this connection's first frame applies.
    for focus in [true, false] {
        let mut zero_grid = h.handshake(tab_id);
        zero_grid["rows"] = serde_json::json!(0);
        zero_grid["focus"] = serde_json::json!(focus);
        assert_eq!(
            dial(&h.socket, zero_grid).await.unwrap_err().code,
            "invalid-param"
        );
    }
    h.wait_for_data_conns(0).await;
}

/// After a stop there is nothing left to stream from, and nothing is
/// registered on the way to saying so.
///
/// The code is `shutting-down` and not `not-found`, although the stop
/// reaped this tab before the dial landed: a client can reasonably read
/// `not-found` as "that tab was deleted" and drop it from its UI, where
/// the truth is that the whole session went away. That is what the early
/// latch read before the tab lookup buys — the locked recheck at step 8
/// stays where it is, because only it is atomic with the registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_after_a_stop_is_refused() {
    let h = harness().await;
    let (mut client, tab_id) = h.live_tab().await;
    let prepared = h.handshake(tab_id);

    let report: SessionStopResult = client
        .call(ops::SESSION_STOP, SessionStopParams {})
        .await
        .expect("session.stop");
    assert!(
        report.reaped.contains(&tab_id),
        "the stop accounted for the tab before this dial"
    );

    let error = dial(&h.socket, prepared)
        .await
        .expect_err("a stopped session serves no new attach");
    assert_eq!(error.code, "shutting-down");
    h.wait_for_data_conns(0).await;
}

/// The one bound on live data connections. The handshake negotiates on
/// the connection itself, so there is no earlier credential to count and
/// nothing else standing in front of this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_serves_only_so_many_data_connections() {
    let h = harness().await;
    let (_client, tab_id) = h.live_tab().await;

    let mut held = Vec::new();
    for nth in 0..MAX_DATA_CONNS_PER_SESSION {
        let (_accepted, data) = dial(&h.socket, h.handshake(tab_id))
            .await
            .unwrap_or_else(|error| panic!("attach {nth} was refused: {}", error.code));
        held.push(data);
    }
    h.wait_for_data_conns(MAX_DATA_CONNS_PER_SESSION).await;

    let error = dial(&h.socket, h.handshake(tab_id))
        .await
        .expect_err("one past the bound");
    assert_eq!(error.code, "too-many-attaches");

    // And a slot freed by a departing client is a slot the next one
    // gets: the bound is on what is live, not on what ever attached.
    held.pop();
    h.wait_for_data_conns(MAX_DATA_CONNS_PER_SESSION - 1).await;
    let (_accepted, _data) = dial(&h.socket, h.handshake(tab_id))
        .await
        .expect("the freed slot is usable");
}

/// The registration happens before the fence, and every rejection past
/// it hands the slot back — otherwise a session would leak its way to
/// `too-many-attaches` on nothing but failed attaches.
///
/// The park is a `vt` encode with nothing to carry: the parser sits
/// inside an unfinished DCS longer than the retained continuation, so
/// the snapshot cannot answer until something closes it. Closing the tab
/// instead is what turns the parked fence into a rejection — the
/// sequence can never be closed now, which is exactly `snapshot-failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejection_after_registration_releases_the_slot() {
    let h = harness().await;
    let (mut client, tab_id) = h.live_tab().await;

    let mut unfinished = Vec::from(&b"\x1bP"[..]);
    unfinished.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
    feed(&mut client, tab_id, unfinished).await;

    let mut parked = h.handshake(tab_id);
    parked["kinds"] = serde_json::json!([AttachPayloadKind::VT]);
    let socket = h.socket.clone();
    let dialing = tokio::spawn(async move { dial(&socket, parked).await });

    // Registered at step 8, long before the fence it is now stuck in.
    h.wait_for_data_conns(1).await;

    client
        .call::<_, serde_json::Value>(ops::TAB_CLOSE, TabCloseParams { tab_id })
        .await
        .expect("tab.close");

    let error = timeout(BUDGET, dialing)
        .await
        .expect("the parked fence answers once its tab is gone")
        .expect("join")
        .expect_err("a snapshot from a tab that no longer exists");
    assert_eq!(error.code, "snapshot-failed");
    h.wait_for_data_conns(0).await;
}

/// A respawn that lands while the snapshot command is still queued.
///
/// The forwarder's lookup said the tab was this generation, and the
/// cloned sender keeps that task alive and answering long after the
/// supervisor has replaced it — so the reply carries the DEAD terminal's
/// screen and seq space while the tab id already names the replacement.
/// Accepting it paints the old terminal, usually straight into its EXIT,
/// for a client that asked for the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_respawn_around_the_snapshot_handoff_is_refused() {
    let (h, mut releases) = harness_pausing_handoffs().await;
    let (_client, tab_id) = h.live_tab().await;

    let socket = h.socket.clone();
    let line = h.handshake(tab_id);
    let dialing = tokio::spawn(async move { dial(&socket, line).await });

    // Parked inside the serving turn: the forwarder's lookup is behind
    // us and the snapshot has not been cut yet.
    let release = parked(&mut releases).await;
    h.respawn(tab_id);
    let _ = release.send(());

    let error = timeout(BUDGET, dialing)
        .await
        .expect("the parked hand-off answers once it is released")
        .expect("join")
        .expect_err("the snapshot came from a terminal this tab id no longer names");
    assert_eq!(error.code, "not-found");
    h.wait_for_data_conns(0).await;
}

/// A respawn between the admission and the focused resize: the attach
/// is refused for the generation it was admitted for, and the
/// replacement tab is left exactly as it was.
///
/// Step 10 is the one step of an attach that changes the tab before the
/// generation check has had the last word, so it has to act on the
/// pipeline the admission checked — never on whatever the id names by
/// the time it runs. Otherwise a refused attach resizes a terminal
/// somebody else is typing in, which is a side effect from an operation
/// that answered `not-found`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_refused_for_a_respawn_never_resizes_the_replacement() {
    let (h, mut admitted) = harness_pausing_admission().await;
    let (_client, tab_id) = h.live_tab().await;

    let mut wide = h.handshake(tab_id);
    wide["cols"] = serde_json::json!(100);
    wide["rows"] = serde_json::json!(30);
    let socket = h.socket.clone();
    let dialing = tokio::spawn(async move { dial(&socket, wide).await });

    // Registered, and nothing done with the tab yet.
    let release = parked(&mut admitted).await;
    h.respawn(tab_id);
    let _ = release.send(());

    let error = timeout(BUDGET, dialing)
        .await
        .expect("the parked attach answers once it is released")
        .expect("join")
        .expect_err("the generation it was admitted for is gone");
    assert_eq!(error.code, "not-found");

    // An unfocused attach states no geometry of its own, so the size it
    // reports is the replacement tab's own — the one it was spawned at,
    // unless the refused attach reached it.
    let mut watching = h.handshake(tab_id);
    watching["focus"] = serde_json::json!(false);
    let socket = h.socket.clone();
    let second = tokio::spawn(async move { dial(&socket, watching).await });
    let _ = parked(&mut admitted).await.send(());
    let (accepted, _data) = second
        .await
        .expect("join")
        .expect("the replacement tab attaches");
    assert_eq!(
        (accepted.snapshot_cols, accepted.snapshot_rows),
        (80, 24),
        "a refused attach resized the tab that replaced its own"
    );
}

/// The resume twin, and the one the client would never recover from on
/// its own: a resume hands over a ring slice and a live subscription in
/// one task turn, so an accepted stale hand-off is the old terminal's
/// backlog *and* its tail, seq-contiguous and indistinguishable from the
/// tab the client asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_respawn_around_the_resume_handoff_is_refused() {
    let (h, mut releases) = harness_pausing_handoffs().await;
    let (mut client, tab_id) = h.live_tab().await;

    // A client that has been attached once and knows the stream's
    // identity, the way a resuming one does.
    let socket = h.socket.clone();
    let line = h.handshake(tab_id);
    let first = tokio::spawn(async move { dial(&socket, line).await });
    let _ = parked(&mut releases).await.send(());
    let (accepted, mut data) = first
        .await
        .expect("join")
        .expect("the first attach is accepted");
    let (_snapshot, pty) = data.read_snapshot().await;
    let after = pty.last().map_or(accepted.seq, |(seq, _)| *seq);
    feed(&mut client, tab_id, b"ROOST_CAUGHT_UP\r\n".to_vec()).await;
    let (applied, _) = data.read_pty_until(after, b"ROOST_CAUGHT_UP").await;
    drop(data);

    let mut resume = h.handshake(tab_id);
    resume["resume_from_seq"] = serde_json::json!(applied + 1);
    resume["server_epoch"] = serde_json::json!(accepted.server_epoch);
    resume["tab_generation"] = serde_json::json!(accepted.tab_generation);
    let socket = h.socket.clone();
    let dialing = tokio::spawn(async move { dial(&socket, resume).await });

    let release = parked(&mut releases).await;
    h.respawn(tab_id);
    let _ = release.send(());

    let error = timeout(BUDGET, dialing)
        .await
        .expect("the parked hand-off answers once it is released")
        .expect("join")
        .expect_err("the ring slice came from a terminal this tab id no longer names");
    // The resume falls back to the snapshot path, as every unhonorable
    // resume does; that path then finds the generation it was admitted
    // for is gone.
    assert_eq!(error.code, "not-found");
    h.wait_for_data_conns(0).await;
}

/// What the focused resize is actually for, measured on the **child**:
/// a program in the tab reads the geometry the handshake stated, not the
/// one the tab had when the connection was dialed.
///
/// The prompt is written from the CONTROL connection (`tab.write` states
/// no geometry, so it cannot size anything itself) — an INPUT frame from
/// this data connection would carry the handshake's geometry with it and
/// resize the tab on its own, which would make the assertion true
/// whatever the handshake did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_focused_attach_sizes_the_child_before_it_can_be_read() {
    let h = harness().await;
    let (mut client, tab_id) = h.shell_tab(80, 24).await;

    let mut wide = h.handshake(tab_id);
    wide["cols"] = serde_json::json!(100);
    wide["rows"] = serde_json::json!(30);
    let (accepted, mut data) = dial(&h.socket, wide).await.expect("accepted");
    assert_eq!((accepted.snapshot_cols, accepted.snapshot_rows), (100, 30));

    let (_snapshot, pty) = data.read_snapshot().await;
    let after = pty.last().map_or(accepted.seq, |(seq, _)| *seq);

    write_tab(&mut client, tab_id, b"stty size\n").await;
    let (_last, text) = data.read_pty_until(after, b"30 100").await;
    assert!(
        contains(&text, b"30 100"),
        "the child reported the handshake's rows and cols: {:?}",
        String::from_utf8_lossy(&text)
    );
}
