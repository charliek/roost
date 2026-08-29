//! The attach data plane decoded by a real client (plan 036 C7/D8).
//!
//! Everything here runs against a live `serve()` — the same session a
//! `roostctl session start` produces, minus the fork — and dials it the
//! way a host client will: `session.connect`, `tab.attach`, a second
//! connection carrying the JSON handshake, the preamble, and then binary
//! frames. The SNAP payload goes into plan 034's [`SnapshotDecoder`] and
//! the decoded terminal is walked through the same densifier the session
//! itself dumps from, so a passing assertion means the client and the
//! server are looking at the same screen.
//!
//! **This file is the only place a GHOSTSNP stream is semantically
//! decoded.** The engine's forwarder tests walk the records for their
//! tags, and the Python contract lane never parses them at all
//! (architecture §12) — one client implementation, kept honest here.
//!
//! Two things every case leans on. The tabs are parked on children that
//! emit nothing on their own, so every byte on the wire is one the test
//! caused (`tab.feed_pty_bytes`, which this harness's sessions serve
//! because [`support::Layout`] runs them in test mode). And every wait is
//! a poll against a scaled deadline — there are no sleeps standing in for
//! synchronization.

mod support;

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use roost_ipc::client::{ClientError, DataConnection};
use roost_ipc::dataframe::{DataFrame, FRAME_EXIT, FRAME_INPUT, FRAME_PTY, FRAME_SNAP};
use roost_ipc::messages::{
    ops, AttachAccepted, AttachHandshake, AttachMode, AttachPayloadKind, ResolvedCell,
    TabAttachParams, TabAttachResult, TabCapturePtyInputParams, TabCapturePtyInputResult,
    TabCloseParams, TabDumpCursor, TabDumpResolvedResult, TabDumpResult, TabFeedPtyBytesParams,
    WireTabRef,
};
use roost_ipc::IpcClient;
use roost_vt::{
    Cell, CursorInfo, DecodedTerminal, DrawCell, HistoryStep, ReadyState, RenderState, RenderedRow,
    ScrollViewport, SnapshotDecodeOptions, SnapshotDecoder, Terminal,
};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

/// The viewport every test opens, resizes and attaches at — one
/// geometry throughout, so `tab.attach` never reflows a tab out from
/// under content the test just laid down.
const COLS: u16 = 80;
const ROWS: u16 = 24;
/// Enough seeded lines that the snapshot carries HISTORY pages behind
/// its READY record — the viewport alone rides inside READY, so a
/// smaller seed would make "history arrives and applies" vacuous.
const HISTORY_LINES: usize = 1200;

/// Budget for one blocking step — a frame, a dump that has not caught up
/// yet, a whole snapshot. Scaled the same way every other session test's
/// waits are.
fn budget() -> Duration {
    support::scaled(Duration::from_secs(30))
}

// ---------------------------------------------------------------------
// A session, leased, with tabs a test can drive
// ---------------------------------------------------------------------

/// One running session plus the control connection that holds its lease
/// — `tab.attach` is lease-gated, so no test gets anywhere without it.
struct Session {
    layout: support::Layout,
    served: tokio::task::JoinHandle<anyhow::Result<()>>,
    control: IpcClient,
    lease: String,
    project_id: i64,
}

impl Session {
    async fn start() -> Self {
        let layout = support::Layout::new();
        let launch_cwd = layout.launch_cwd.clone();
        let served = layout.spawn(&launch_cwd);
        let mut control = support::connect(&layout.socket_path()).await;
        let lease = support::session_connect(&mut control).await.lease;
        let project_id = support::tabs(&mut control).await[0].project_id;
        Self {
            layout,
            served,
            control,
            lease,
            project_id,
        }
    }

    fn socket(&self) -> PathBuf {
        self.layout.socket_path()
    }

    /// A tab at [`COLS`]×[`ROWS`] parked on a child that produces
    /// nothing. Sized here rather than at `tab.attach` so the attach
    /// geometry is a no-op and the snapshot is taken at the same size
    /// the seeded content was laid out at.
    async fn quiet_tab(&mut self) -> i64 {
        self.tab(&["/bin/sh", "-c", "exec sleep 300"]).await
    }

    async fn tab(&mut self, argv: &[&str]) -> i64 {
        let cwd = self.layout.subdir("tab");
        let tab = support::open_tab(&mut self.control, self.project_id, &cwd, "", argv).await;
        support::resize_tab(&mut self.control, tab.id, u32::from(COLS), u32::from(ROWS))
            .await
            .expect("tab.resize");
        self.wait_for_geometry(tab.id, COLS, ROWS).await;
        tab.id
    }

    /// Inject bytes into the tab's PTY-output drain. Split into frames
    /// the JSON transport is comfortable with, because the ring-eviction
    /// case feeds megabytes.
    async fn feed(&mut self, tab_id: i64, data: &[u8]) {
        for chunk in data.chunks(256 * 1024) {
            self.control
                .call::<_, serde_json::Value>(
                    ops::TAB_FEED_PTY_BYTES,
                    TabFeedPtyBytesParams {
                        tab_id,
                        data: chunk.to_vec(),
                    },
                )
                .await
                .expect("tab.feed_pty_bytes");
        }
    }

    /// Every byte the session has queued toward the child — keystrokes
    /// forwarded from `INPUT` frames and the server terminal's own
    /// answers to device queries, in the order the writer took them.
    async fn captured_input(&mut self, tab_id: i64) -> Vec<u8> {
        let batch: TabCapturePtyInputResult = self
            .control
            .call(
                ops::TAB_CAPTURE_PTY_INPUT,
                TabCapturePtyInputParams {
                    tab_id: WireTabRef::Local(tab_id),
                    drain: true,
                },
            )
            .await
            .expect("tab.capture_pty_input");
        batch.data
    }

    async fn close_tab(&mut self, tab_id: i64) {
        self.control
            .call::<_, serde_json::Value>(ops::TAB_CLOSE, TabCloseParams { tab_id })
            .await
            .expect("tab.close");
    }

    async fn attach(&mut self, tab_id: i64) -> TabAttachResult {
        self.control
            .call(
                ops::TAB_ATTACH,
                TabAttachParams {
                    lease: self.lease.clone(),
                    tab_id,
                    kinds: vec![AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT)],
                    cols: COLS,
                    rows: ROWS,
                    cell_w_px: 0,
                    cell_h_px: 0,
                    libghostty_build: roost_vt::libghostty_build(),
                },
            )
            .await
            .expect("tab.attach")
    }

    /// Poll `tab.dump` until a row carries `needle`, and hand back that
    /// dump. The tabs here go quiet once their seeded bytes have landed,
    /// so the dump the marker arrives in is also the settled one.
    async fn dump_showing(&mut self, tab_id: i64, needle: &str) -> TabDumpResult {
        self.wait_for_dump(tab_id, &format!("{needle:?} to reach the screen"), |dump| {
            dump.rows_text.iter().any(|row| row.contains(needle))
        })
        .await
    }

    async fn wait_for_geometry(&mut self, tab_id: i64, cols: u16, rows: u16) -> TabDumpResult {
        self.wait_for_dump(tab_id, &format!("the tab to reach {cols}x{rows}"), |dump| {
            (dump.cols, dump.rows) == (u32::from(cols), u32::from(rows))
        })
        .await
    }

    async fn wait_for_dump(
        &mut self,
        tab_id: i64,
        what: &str,
        mut predicate: impl FnMut(&TabDumpResult) -> bool,
    ) -> TabDumpResult {
        let deadline = Instant::now() + budget();
        loop {
            let dump = support::tab_dump(&mut self.control, tab_id).await;
            if predicate(&dump) {
                return dump;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the screen was {:#?}",
                dump.rows_text
            );
            support::tick().await;
        }
    }

    async fn stop(mut self) {
        support::session_stop(&mut self.control).await;
        self.served.await.expect("join").expect("serve");
    }
}

// ---------------------------------------------------------------------
// The client half of the data plane
// ---------------------------------------------------------------------

/// The shipped client transport ([`roost_ipc::client::DataConnection`])
/// plus this lane's deadline discipline: every read is bounded, because
/// a stalled stream is a failure to report and not something to wait
/// out. The handshake, the preamble, the residue hand-off and the
/// framing are all the library's — running the real session against the
/// same code a host client uses is the point.
struct DataClient<R>(DataConnection<R, OwnedWriteHalf>);

fn handshake(token: &str) -> AttachHandshake {
    AttachHandshake::snapshot(token)
}

fn resume_handshake(ticket: &TabAttachResult, from_seq: u64) -> AttachHandshake {
    AttachHandshake::resume(
        &ticket.attach_token,
        from_seq,
        ticket.server_epoch,
        ticket.tab_generation,
    )
}

/// Dial a data connection and run the handshake: one JSON line out, one
/// JSON line back, then the preamble and binary for the rest of its life.
async fn dial(
    socket: &Path,
    handshake: AttachHandshake,
) -> Result<(AttachAccepted, DataClient<OwnedReadHalf>), ClientError> {
    dial_with(socket, handshake, |half| half).await
}

/// The same, with every socket read capped at one byte — the handshake
/// line, the preamble, each frame header and each payload all arrive
/// split across as many reads as they have bytes.
async fn dial_one_byte_at_a_time(
    socket: &Path,
    handshake: AttachHandshake,
) -> Result<(AttachAccepted, DataClient<OneByteAtATime<OwnedReadHalf>>), ClientError> {
    dial_with(socket, handshake, OneByteAtATime).await
}

async fn dial_with<R: AsyncRead + Unpin>(
    socket: &Path,
    handshake: AttachHandshake,
    wrap: impl FnOnce(OwnedReadHalf) -> R,
) -> Result<(AttachAccepted, DataClient<R>), ClientError> {
    let stream = UnixStream::connect(socket).await.expect("dial the session");
    let (read_half, writer) = stream.into_split();
    let (accepted, conn) = timeout(
        budget(),
        DataConnection::handshake(wrap(read_half), writer, &handshake),
    )
    .await
    .expect("a handshake reply in time")?;
    Ok((accepted, DataClient(conn)))
}

impl<R: AsyncRead + Unpin> DataClient<R> {
    async fn next(&mut self) -> Option<DataFrame> {
        timeout(budget(), self.0.next_frame())
            .await
            .expect("a frame in time")
            .expect("frame read")
    }

    async fn frame(&mut self) -> DataFrame {
        self.next().await.expect("the stream is still open")
    }

    /// Client → server. The only thing this file ever writes: proving
    /// the client's *own* device-query answers stay off the wire is a
    /// claim about what was never sent, and it is only worth anything
    /// because every send goes through here.
    async fn send(&mut self, frame_type: u8, payload: &[u8]) {
        self.0
            .send_frame(frame_type, payload)
            .await
            .expect("write the frame");
    }
}

/// An `AsyncRead` that hands back at most one byte per call, however
/// much the socket has ready.
///
/// The reassembly under test is the client's: `FrameReader`,
/// `DataFrameReader` and the decoder all have to hold a partial line,
/// header, payload or record across reads. The server's writes are
/// already chunked by its own framing, so fragmenting the *reads* is
/// what actually exercises the boundary logic.
struct OneByteAtATime<R>(R);

impl<R: AsyncRead + Unpin> AsyncRead for OneByteAtATime<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut byte = [0u8; 1];
        let mut one = ReadBuf::new(&mut byte);
        match Pin::new(&mut self.0).poll_read(cx, &mut one) {
            Poll::Ready(Ok(())) => {
                let filled = one.filled();
                if !filled.is_empty() {
                    buf.put_slice(filled);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------
// Decoding one attach
// ---------------------------------------------------------------------

/// The caps a host client sets on its decoder: the transport's own
/// budget, so a stream this session could never have sent is refused by
/// the wrapper before libghostty sees it. Everything else stays at the
/// wrapper's defaults — in particular the continuation cap, which the
/// mid-sequence fence case needs to be permissive enough to accept.
fn decode_options() -> SnapshotDecodeOptions {
    SnapshotDecodeOptions {
        max_total_bytes: roost_engine::attach::ATTACH_BYTE_BUDGET,
        ..SnapshotDecodeOptions::default()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct DecodeOpts {
    /// Feed each SNAP payload into the decoder one byte at a time.
    trickle: bool,
    /// Resize the decoded terminal the moment READY lands, before a
    /// single history page has been consumed.
    resize_at_ready: Option<(u16, u16)>,
}

/// What one snapshot decode saw on the way through.
#[derive(Debug, Default)]
struct DecodeTrace {
    /// PTY frames that arrived before the decoder could render — they
    /// are held and replayed in order once READY lands.
    pty_before_ready: usize,
    /// SNAP frames the snapshot took, which is how a case can claim the
    /// stream was still in flight when something raced it.
    snap_frames: usize,
    /// Total SNAP payload bytes.
    snap_bytes: usize,
    pages: usize,
    rows_prepended: usize,
    pages_after_resize: usize,
    rows_after_resize: usize,
    /// The last PTY seq applied; where a resume would pick up.
    last_seq: u64,
}

/// Drive one attach's snapshot to FINISH.
///
/// The two streams are handled exactly as a client must: SNAP payloads
/// are fed to the decoder, `try_ready` is attempted until it reports
/// READY, history is stepped with `try_next` between frames, and live
/// PTY frames are applied to the decoded terminal with `vt_write` —
/// interleaved with the history steps, which `snapshot.h` explicitly
/// allows. Frames arriving before READY are the one thing that cannot be
/// applied on the spot, so they are queued and replayed in order.
async fn drain_until_finish<R: AsyncRead + Unpin>(
    data: &mut DataClient<R>,
    fence: u64,
    opts: DecodeOpts,
) -> (DecodedTerminal, DecodeTrace) {
    let mut decoder = SnapshotDecoder::new(decode_options());
    let mut trace = DecodeTrace {
        last_seq: fence,
        ..DecodeTrace::default()
    };
    let mut ready = false;
    let mut resized = false;
    let mut deferred: Vec<Vec<u8>> = Vec::new();
    // Every SNAP byte seen up to READY, kept only that long. The
    // ordering claim has to be made against the *wire*: asserting it
    // from the decoder's own page counter would be circular, since
    // pages are only counted after READY and that counter is therefore
    // zero by construction.
    let mut prefix = Vec::new();
    let deadline = Instant::now() + budget();

    loop {
        assert!(
            Instant::now() < deadline,
            "the snapshot never reached FINISH; {trace:?}"
        );

        if !ready && decoder.try_ready().expect("try_ready") == ReadyState::Ready {
            ready = true;
            assert_eq!(
                roost_vt::ready_boundary(&prefix).ok(),
                Some(prefix.len()),
                "the forwarder sends everything through READY at full speed and stops \
                 exactly on that boundary, so the client renders before a single \
                 history page is on the wire"
            );
            prefix = Vec::new();
            if let Some((cols, rows)) = opts.resize_at_ready {
                decoder
                    .resize(cols, rows, 0, 0)
                    .expect("resize the decoded terminal");
                resized = true;
            }
            for bytes in deferred.drain(..) {
                decoder.vt_write(&bytes).expect("vt_write");
            }
        }

        if ready {
            let mut finished = false;
            loop {
                match decoder.try_next().expect("try_next") {
                    HistoryStep::NeedMoreBytes => break,
                    HistoryStep::Page { rows_prepended, .. } => {
                        trace.pages += 1;
                        trace.rows_prepended += rows_prepended;
                        if resized {
                            trace.pages_after_resize += 1;
                            trace.rows_after_resize += rows_prepended;
                        }
                    }
                    HistoryStep::Finished => {
                        finished = true;
                        break;
                    }
                }
            }
            if finished {
                break;
            }
        }

        let frame = data.frame().await;
        match frame.frame_type {
            FRAME_SNAP => {
                trace.snap_frames += 1;
                trace.snap_bytes += frame.payload.len();
                if !ready {
                    prefix.extend_from_slice(&frame.payload);
                }
                feed_snap_into_decoder(&mut decoder, &frame.payload, opts.trickle);
            }
            FRAME_PTY => {
                let (seq, bytes) = split_pty(&frame);
                assert_eq!(
                    seq,
                    trace.last_seq + 1,
                    "PTY frames must be contiguous from the fence"
                );
                trace.last_seq = seq;
                if ready {
                    decoder.vt_write(&bytes).expect("vt_write");
                } else {
                    trace.pty_before_ready += 1;
                    deferred.push(bytes);
                }
            }
            other => panic!("unexpected frame {other:#04x} before FINISH"),
        }
    }

    (decoder.finish().expect("finish"), trace)
}

fn feed_snap_into_decoder(decoder: &mut SnapshotDecoder, payload: &[u8], trickle: bool) {
    if trickle {
        for byte in payload {
            decoder.feed(std::slice::from_ref(byte)).expect("feed");
        }
    } else {
        decoder.feed(payload).expect("feed");
    }
}

/// Apply live PTY frames to an already decoded terminal until `marker`
/// has come through, asserting the stream stays contiguous. Returns the
/// bytes applied.
async fn apply_pty_until<R: AsyncRead + Unpin>(
    data: &mut DataClient<R>,
    terminal: &mut Terminal,
    last_seq: &mut u64,
    marker: &[u8],
) -> Vec<u8> {
    let mut applied = Vec::new();
    while !contains(&applied, marker) {
        let frame = data.frame().await;
        assert_eq!(
            frame.frame_type, FRAME_PTY,
            "only PTY frames follow a finished snapshot"
        );
        let (seq, bytes) = split_pty(&frame);
        assert_eq!(seq, *last_seq + 1, "PTY frames must stay contiguous");
        *last_seq = seq;
        terminal.vt_write(&bytes);
        applied.extend_from_slice(&bytes);
    }
    applied
}

fn split_pty(frame: &DataFrame) -> (u64, Vec<u8>) {
    assert!(
        frame.payload.len() >= 8,
        "a PTY frame carries a u64 seq and then bytes"
    );
    let seq = u64::from_le_bytes(frame.payload[..8].try_into().unwrap());
    (seq, frame.payload[8..].to_vec())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

// ---------------------------------------------------------------------
// Dumping the decoded terminal
// ---------------------------------------------------------------------

/// One walk of a decoded terminal's viewport, through the same
/// `RenderedRow` densifier `tab.dump` goes through on the session side —
/// which is the point: a difference here is a real divergence, not two
/// spellings of the same screen.
struct DecodedGrid {
    rows: Vec<RenderedRow>,
    cursor: Option<CursorInfo>,
}

fn walk_decoded(terminal: &Terminal, cols: u16) -> DecodedGrid {
    let mut render = RenderState::new().expect("RenderState::new");
    render.update(terminal).expect("render update");
    let colors = render.colors().expect("render colors");
    let cursor = render.cursor();
    render.mark_full().expect("mark_full");
    let defaults = (colors.foreground, colors.background);
    let mut rows: Vec<(u32, RenderedRow)> = Vec::new();
    render
        .walk_dirty(terminal, |row, cells: &[Cell]| {
            rows.push((row, RenderedRow::build(cells, defaults, cols)));
        })
        .expect("walk_dirty");
    rows.sort_by_key(|(row, _)| *row);
    DecodedGrid {
        rows: rows.into_iter().map(|(_, row)| row).collect(),
        cursor,
    }
}

impl DecodedGrid {
    /// The client's answer to `tab.dump`, in the wire type the session
    /// replies with so the two can be compared whole.
    fn dump(&self, cols: u16) -> TabDumpResult {
        TabDumpResult {
            cols: u32::from(cols),
            rows: self.rows.len() as u32,
            cursor: self
                .cursor
                .filter(|cursor| cursor.visible)
                .map(|cursor| TabDumpCursor {
                    row: cursor.row,
                    col: cursor.col,
                    visible: true,
                }),
            rows_text: self.rows.iter().map(|row| row.text.clone()).collect(),
        }
    }

    fn row(&self, row: u32) -> &[DrawCell] {
        self.rows
            .get(row as usize)
            .map_or(&[][..], |row| &row.cells)
    }
}

fn dump_decoded(terminal: &Terminal, cols: u16) -> TabDumpResult {
    walk_decoded(terminal, cols).dump(cols)
}

/// The rows at the very top of a terminal's scrollback, read the only
/// way a render walk can read history: scroll the viewport up to it,
/// walk, scroll back.
///
/// The server cannot be the reference here — `tab.dump` only ever shows
/// a viewport — but it does not need to be: the seed is a pure function
/// of its line count, so the expected text is computable.
fn scrollback_top(terminal: &mut Terminal, cols: u16) -> Vec<String> {
    terminal.scroll_viewport(ScrollViewport::Top);
    let rows = dump_decoded(terminal, cols).rows_text;
    terminal.scroll_viewport(ScrollViewport::Bottom);
    rows
}

fn row_showing(dump: &TabDumpResult, needle: &str) -> u32 {
    dump.rows_text
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no row carries {needle:?}: {:#?}", dump.rows_text)) as u32
}

/// libghostty's `#rrggbb`, spelled the way the session's
/// `tab.dump_resolved` spells it.
fn hex(color: roost_vt::ColorRgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

/// Compare one row's resolved colors: every cell the client's densifier
/// drew has to carry the same foreground, background and SGR bits the
/// session resolved for that coordinate.
///
/// This is what proves the SGR state survived the encode — `rows_text`
/// alone would pass on a snapshot that lost every colour it carried.
fn assert_row_colors_match(grid: &DecodedGrid, server: &TabDumpResolvedResult, row: u32) {
    let drawn = grid.row(row);
    assert!(
        !drawn.is_empty(),
        "row {row} of the decoded terminal drew nothing"
    );
    let mut styled = 0;
    for cell in drawn {
        let theirs = server_cell(server, row, cell.col);
        assert_eq!(cell.text, theirs.text, "text at ({row}, {})", cell.col);
        assert_eq!(
            hex(cell.foreground),
            theirs.fg,
            "foreground at ({row}, {})",
            cell.col
        );
        assert_eq!(
            hex(cell.background),
            theirs.bg,
            "background at ({row}, {})",
            cell.col
        );
        assert_eq!(
            (
                cell.explicit_background,
                cell.bold,
                cell.italic,
                cell.inverse
            ),
            (
                theirs.has_explicit_bg,
                theirs.bold,
                theirs.italic,
                theirs.inverse
            ),
            "style bits at ({row}, {})",
            cell.col
        );
        if cell.explicit_background {
            styled += 1;
        }
    }
    // The other direction: a client that simply drew fewer cells than
    // the server would satisfy every assertion above.
    let theirs = server
        .cells
        .iter()
        .filter(|cell| cell.row == row && cell.has_explicit_bg)
        .count();
    assert_eq!(
        styled, theirs,
        "the two sides disagree about how many cells on row {row} carry an explicit background"
    );
}

fn server_cell(server: &TabDumpResolvedResult, row: u32, col: u16) -> &ResolvedCell {
    server
        .cells
        .iter()
        .find(|cell| cell.row == row && cell.col == col)
        .unwrap_or_else(|| panic!("the session resolved no cell at ({row}, {col})"))
}

// ---------------------------------------------------------------------
// Seed content
// ---------------------------------------------------------------------

/// Deterministic seed bytes: coloured, bold, true-colour and inverse
/// runs on every line, and enough lines to push history off a 24-row
/// viewport. `SGR_FENCE` closes it so a poll has something unambiguous
/// to wait for.
fn seed_bytes(lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for index in 1..=lines {
        out.extend_from_slice(
            format!(
                "line {index:04} \x1b[31mRED\x1b[0m \x1b[1;38;2;12;34;56mTRUE\x1b[0m \
                 \x1b[7minv\x1b[0m plain\r\n"
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(b"\x1b[38;2;9;99;199;48;2;3;33;133mSGR_FENCE\x1b[0m tail\r\n");
    out
}

/// What one seeded line renders to once the SGR runs above have been
/// consumed by a parser — the twin of [`seed_bytes`], and the reference
/// for content that has scrolled out of every viewport.
fn seeded_line_text(index: usize) -> String {
    format!("line {index:04} RED TRUE inv plain")
}

// ---------------------------------------------------------------------
// 1. Fidelity at the fence
// ---------------------------------------------------------------------

/// The headline claim: a client that decodes the attach stream is
/// looking at the server's terminal, cell for cell and colour for
/// colour.
///
/// The tab is quiesced before the attach, so the snapshot is the whole
/// story — no live frames to reconcile — and the session's own
/// `tab.dump` / `tab.dump_resolved` are the reference, because those are
/// what a UI would render from on the other side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fidelity_at_the_fence() {
    let mut session = Session::start().await;
    let tab_id = session.quiet_tab().await;
    session.feed(tab_id, &seed_bytes(HISTORY_LINES)).await;
    let server = session.dump_showing(tab_id, "SGR_FENCE").await;
    let server_resolved = support::tab_dump_resolved(&mut session.control, tab_id).await;

    let ticket = session.attach(tab_id).await;
    let (accepted, mut data) = dial(&session.socket(), handshake(&ticket.attach_token))
        .await
        .expect("the handshake is accepted");
    assert_eq!(accepted.mode, AttachMode::Snapshot);
    assert_eq!(
        accepted.kind.as_str(),
        AttachPayloadKind::GHOSTTY_SNAPSHOT,
        "the only payload kind this session advertises"
    );

    let (mut decoded, trace) =
        drain_until_finish(&mut data, accepted.seq, DecodeOpts::default()).await;
    assert!(
        decoded.history_rows_primary > 0,
        "the seeded lines leave scrollback behind a 24-row viewport"
    );
    assert!(
        trace.pages > 0 && trace.rows_prepended > 0,
        "the history behind READY has to arrive and apply: {trace:?}"
    );

    let grid = walk_decoded(&decoded.terminal, COLS);
    assert_eq!(
        grid.dump(COLS),
        server,
        "the decoded viewport must equal the server's dump at the fence"
    );
    assert_row_colors_match(&grid, &server_resolved, row_showing(&server, "SGR_FENCE"));
    assert_row_colors_match(
        &grid,
        &server_resolved,
        row_showing(&server, &format!("line {HISTORY_LINES:04}")),
    );

    // The pages did not just arrive and count: their CONTENT is the
    // seeded content. Row counts alone would be satisfied by a
    // scrollback of blanks, and `SERVER_VT_SCROLLBACK` is 2000, so the
    // very first seeded line is still in there.
    let top = scrollback_top(&mut decoded.terminal, COLS);
    let expected: Vec<String> = (1..=usize::from(ROWS)).map(seeded_line_text).collect();
    assert_eq!(
        top, expected,
        "the top of the decoded scrollback has to be the first seeded lines"
    );

    session.stop().await;
}

// ---------------------------------------------------------------------
// 2. A fence in the middle of an escape sequence
// ---------------------------------------------------------------------

/// The snapshot is taken with the tab's VT parser mid-CSI, which is what
/// the format's CONTINUATION record exists for. Nothing about that is
/// visible on the screen — the proof is that the *rest* of the sequence,
/// delivered afterwards as ordinary live bytes, completes into the same
/// coloured run on both sides. A client that lost the continuation would
/// print `;30mCONT_TAIL` literally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuation_round_trip() {
    let mut session = Session::start().await;
    let tab_id = session.quiet_tab().await;

    // The parameters of a true-colour SGR, cut in half.
    session.feed(tab_id, b"CONT_HEAD \x1b[38;2;10;20").await;
    session.dump_showing(tab_id, "CONT_HEAD").await;

    let ticket = session.attach(tab_id).await;
    let (accepted, mut data) = dial(&session.socket(), handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    let (mut decoded, trace) =
        drain_until_finish(&mut data, accepted.seq, DecodeOpts::default()).await;
    let mut last_seq = trace.last_seq;

    session.feed(tab_id, b";30mCONT_TAIL\x1b[0m\r\n").await;
    apply_pty_until(
        &mut data,
        &mut decoded.terminal,
        &mut last_seq,
        b"CONT_TAIL",
    )
    .await;

    let server = session.dump_showing(tab_id, "CONT_TAIL").await;
    let server_resolved = support::tab_dump_resolved(&mut session.control, tab_id).await;
    let grid = walk_decoded(&decoded.terminal, COLS);
    assert_eq!(
        grid.dump(COLS),
        server,
        "the split sequence has to reassemble into the same screen"
    );

    let row = row_showing(&server, "CONT_TAIL");
    assert_row_colors_match(&grid, &server_resolved, row);
    let tail = grid
        .row(row)
        .iter()
        .find(|cell| cell.text == "C" && cell.col > 0)
        .expect("the tail's first cell");
    assert_eq!(
        hex(tail.foreground),
        "#0a141e",
        "the continuation carried the half-parsed SGR across the fence"
    );

    session.stop().await;
}

// ---------------------------------------------------------------------
// 3. Fragmentation
// ---------------------------------------------------------------------

/// Every reassembly boundary a client owns, exercised at once: the
/// socket hands back one byte per read (so the handshake line, the
/// preamble, every frame header and every payload are split), and each
/// SNAP payload is fed into the decoder one byte at a time (so every
/// record boundary is split too). Nothing about the decode may depend on
/// a frame or a record arriving whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_byte_at_a_time() {
    let mut session = Session::start().await;
    let tab_id = session.quiet_tab().await;
    session.feed(tab_id, &seed_bytes(HISTORY_LINES)).await;
    let server = session.dump_showing(tab_id, "SGR_FENCE").await;
    let server_resolved = support::tab_dump_resolved(&mut session.control, tab_id).await;

    let ticket = session.attach(tab_id).await;
    let (accepted, mut data) =
        dial_one_byte_at_a_time(&session.socket(), handshake(&ticket.attach_token))
            .await
            .expect("accepted");

    let (decoded, trace) = drain_until_finish(
        &mut data,
        accepted.seq,
        DecodeOpts {
            trickle: true,
            ..DecodeOpts::default()
        },
    )
    .await;
    assert_eq!(
        trace.pty_before_ready, 0,
        "a quiet tab sends no live frames"
    );

    let grid = walk_decoded(&decoded.terminal, COLS);
    assert_eq!(grid.dump(COLS), server);
    assert_row_colors_match(&grid, &server_resolved, row_showing(&server, "SGR_FENCE"));

    session.stop().await;
}

// ---------------------------------------------------------------------
// 4. Input, echo, and who answers a device query
// ---------------------------------------------------------------------

/// INPUT frames are the client's keyboard, and §6's two-VT rule says the
/// *server* terminal is the one that answers what the child asks.
///
/// The tab is a `cat` in raw mode with echo off, so the only bytes that
/// come back are the ones it was handed. The client sends a marker and
/// then a DSR cursor-position query; `cat` echoes both, both terminals
/// parse the query, and both compose an answer — but only the server's
/// may reach the child. The client's own answer is captured by a
/// `set_write_pty_buffer` sink and deliberately dropped, and
/// `tab.capture_pty_input` is what proves it was never forwarded: the
/// child's input is exactly the two INPUT payloads plus one reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_echo_and_replies() {
    let mut session = Session::start().await;
    let tab_id = session
        .tab(&[
            "/bin/sh",
            "-c",
            "stty raw -echo; printf CAT_READY; exec cat",
        ])
        .await;
    session.dump_showing(tab_id, "CAT_READY").await;
    // Nothing has been typed yet, so anything already queued would
    // confuse the accounting below.
    assert!(session.captured_input(tab_id).await.is_empty());

    let ticket = session.attach(tab_id).await;
    let (accepted, mut data) = dial(&session.socket(), handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    let (mut decoded, trace) =
        drain_until_finish(&mut data, accepted.seq, DecodeOpts::default()).await;
    let mut last_seq = trace.last_seq;

    // The client's own reply sink, installed before a single live byte
    // is applied — the whole point is what lands in it.
    let replies = Arc::new(Mutex::new(Vec::new()));
    decoded
        .terminal
        .set_write_pty_buffer(Arc::clone(&replies))
        .expect("install the client's reply sink");

    data.send(FRAME_INPUT, b"HELLO_TYPED").await;
    apply_pty_until(
        &mut data,
        &mut decoded.terminal,
        &mut last_seq,
        b"HELLO_TYPED",
    )
    .await;
    assert!(
        replies.lock().expect("reply sink").is_empty(),
        "plain text asks the terminal nothing"
    );

    data.send(FRAME_INPUT, b"\x1b[6n").await;
    apply_pty_until(&mut data, &mut decoded.terminal, &mut last_seq, b"\x1b[6n").await;
    let client_reply = replies.lock().expect("reply sink").clone();
    assert!(
        client_reply.starts_with(b"\x1b[") && client_reply.ends_with(b"R"),
        "the client terminal composed a cursor-position report: {client_reply:?}"
    );

    // The server's answer goes to the child, which echoes it back — the
    // point in the stream where everything either end will say has been
    // said.
    apply_pty_until(
        &mut data,
        &mut decoded.terminal,
        &mut last_seq,
        &client_reply,
    )
    .await;

    let mut captured = Vec::new();
    let deadline = Instant::now() + budget();
    let expected = [
        b"HELLO_TYPED".to_vec(),
        b"\x1b[6n".to_vec(),
        client_reply.clone(),
    ]
    .concat();
    while captured.len() < expected.len() {
        assert!(
            Instant::now() < deadline,
            "the child never saw the whole exchange: {captured:?}"
        );
        captured.extend_from_slice(&session.captured_input(tab_id).await);
        support::tick().await;
    }
    assert_eq!(
        captured, expected,
        "the child sees the client's keystrokes and exactly one answer — the server's"
    );
    // Both terminals composed the same bytes, which is the point: the
    // only way to tell whose answer reached the child is to count them.
    assert_eq!(
        occurrences(&captured, &client_reply),
        1,
        "the client's own reply must never be forwarded"
    );
    assert_eq!(
        occurrences(&captured, b"R"),
        1,
        "one cursor-position report in the child's whole input, and no other"
    );

    let server = session.dump_showing(tab_id, "HELLO_TYPED").await;
    assert_eq!(
        dump_decoded(&decoded.terminal, COLS),
        server,
        "the echo landed identically on both terminals"
    );

    session.stop().await;
}

// ---------------------------------------------------------------------
// 5. Resume, and the fallback when the ring cannot serve it
// ---------------------------------------------------------------------

/// A client that drops its data connection and comes back is either
/// replayed from the tab's ring or handed a fresh snapshot, and it
/// converges on the server's screen either way.
///
/// The hit is applied to the *same* decoded terminal the first attach
/// produced — that is what a resume is for. The miss is forced by
/// pushing more than the ring's 2 MiB through the tab while the client
/// is away, and its snapshot is decoded from scratch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_ring_hit_and_miss() {
    let mut session = Session::start().await;
    let tab_id = session.quiet_tab().await;
    session.feed(tab_id, &seed_bytes(30)).await;
    session.dump_showing(tab_id, "SGR_FENCE").await;

    let ticket = session.attach(tab_id).await;
    let (accepted, mut data) = dial(&session.socket(), handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    let (mut decoded, trace) =
        drain_until_finish(&mut data, accepted.seq, DecodeOpts::default()).await;
    let mut last_seq = trace.last_seq;
    drop(data);

    // The hit.
    session.feed(tab_id, b"RESUME_MISSED\r\n").await;
    session.dump_showing(tab_id, "RESUME_MISSED").await;
    let stale = last_seq;
    let ticket = session.attach(tab_id).await;
    let (accepted, mut data) = dial(&session.socket(), resume_handshake(&ticket, last_seq + 1))
        .await
        .expect("accepted");
    assert_eq!(accepted.mode, AttachMode::Resume);
    assert_eq!(accepted.seq, last_seq, "the fence is resume_from_seq - 1");
    apply_pty_until(
        &mut data,
        &mut decoded.terminal,
        &mut last_seq,
        b"RESUME_MISSED",
    )
    .await;
    let server = session.dump_showing(tab_id, "RESUME_MISSED").await;
    assert_eq!(
        dump_decoded(&decoded.terminal, COLS),
        server,
        "the ring slice carried the terminal forward with no seam"
    );
    drop(data);

    // The miss: more than the ring holds, so the seq the client left off
    // at is gone by the time it asks for it.
    let mut flood = Vec::new();
    while flood.len() < roost_engine::tab_task::REPLAY_RING_BYTES + 1024 * 1024 {
        flood.extend_from_slice(b"ring-eviction-filler ");
        flood.extend_from_slice(&[b'x'; 43]);
        flood.extend_from_slice(b"\r\n");
    }
    flood.extend_from_slice(b"RING_TAIL\r\n");
    session.feed(tab_id, &flood).await;
    session.dump_showing(tab_id, "RING_TAIL").await;

    let ticket = session.attach(tab_id).await;
    let (accepted, mut data) = dial(&session.socket(), resume_handshake(&ticket, stale + 1))
        .await
        .expect("accepted");
    assert_eq!(
        accepted.mode,
        AttachMode::Snapshot,
        "a seq the ring evicted is served as a full attach, never an error"
    );
    let (fresh, _) = drain_until_finish(&mut data, accepted.seq, DecodeOpts::default()).await;
    let server = session.dump_showing(tab_id, "RING_TAIL").await;
    assert_eq!(
        dump_decoded(&fresh.terminal, COLS),
        server,
        "the fallback snapshot converges on the same screen"
    );

    session.stop().await;
}

// ---------------------------------------------------------------------
// 6. The child dies mid-attach
// ---------------------------------------------------------------------

/// EXIT is the connection's last word, and it never overtakes the
/// snapshot: the tab is closed while the stream is still going out, and
/// the decoder still reaches FINISH and hands over a usable terminal.
///
/// How much of a race this really is, stated honestly. The seed is
/// [`HISTORY_LINES`], and the forwarder cuts its first SNAP frame
/// exactly on the READY boundary, so the snapshot is never fewer than
/// two frames — asserted below, because `tab.close` is issued after the
/// handshake and before the test reads any of them. Whether the pump had
/// already pushed the tail into the socket buffer when the exit was
/// published depends on the platform's buffer size, so what lands in the
/// mid-stream window is not pinned. What *is* pinned is the ordering the
/// forwarder guarantees regardless: EXIT waits for `sent >=
/// snapshot.len()` (attach.rs step 4), so FINISH precedes it and it is
/// the connection's last frame either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_during_attach() {
    let mut session = Session::start().await;
    let tab_id = session.quiet_tab().await;
    session.feed(tab_id, &seed_bytes(HISTORY_LINES)).await;
    let server = session.dump_showing(tab_id, "SGR_FENCE").await;

    let ticket = session.attach(tab_id).await;
    let (accepted, mut data) = dial(&session.socket(), handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    // Between the handshake and the first frame the test reads, so the
    // exit is racing the snapshot rather than following it.
    session.close_tab(tab_id).await;

    let (mut decoded, trace) =
        drain_until_finish(&mut data, accepted.seq, DecodeOpts::default()).await;
    // The stream had not finished going out when the close landed: the
    // forwarder stops its first frame exactly on the READY boundary, so
    // a snapshot with history is always at least two frames, and the
    // test had read none of them.
    assert!(
        trace.snap_frames > 1,
        "the snapshot has to still be in flight for this to be a race: {trace:?}"
    );
    let mut last_seq = trace.last_seq;

    let exit = loop {
        let frame = data.frame().await;
        match frame.frame_type {
            FRAME_PTY => {
                let (seq, bytes) = split_pty(&frame);
                assert_eq!(seq, last_seq + 1);
                last_seq = seq;
                decoded.terminal.vt_write(&bytes);
            }
            FRAME_EXIT => break frame,
            other => panic!("unexpected frame {other:#04x} after FINISH"),
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
    assert_eq!(
        dump_decoded(&decoded.terminal, COLS),
        server,
        "the terminal the client is left holding is the one the tab died with"
    );

    session.stop().await;
}

// ---------------------------------------------------------------------
// 7. Resizing before the history has landed
// ---------------------------------------------------------------------

/// The documented client-side loss rule: a decoder resized between READY
/// and FINISH still consumes and validates the remaining history pages,
/// but they no longer apply — `rows_prepended` comes back zero and that
/// scrollback is gone until the client re-attaches.
///
/// What this pins is the loss and the fact that the terminal survives
/// it. Note the deliberate difference from `roost-vt`'s own resize case,
/// which *records* the surviving row count rather than asserting it: the
/// wrapper battery is about libghostty's behaviour, while this lane is
/// about the rule [`SnapshotDecoder::resize`] states to its callers, so
/// a libghostty change that started applying those pages should surface
/// here as a stale contract rather than pass unnoticed.
///
/// The comparison at the end is only meaningful once BOTH ends are at
/// the new geometry, so the server is resized to match before the dumps
/// are taken — the snapshot was encoded at the old size, and pretending
/// otherwise would be comparing two different screens. The scrollback
/// behind the viewport is genuinely gone on the client; nothing here
/// claims otherwise, and a re-attach is what recovers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resize_mid_history() {
    let mut session = Session::start().await;
    let tab_id = session.quiet_tab().await;
    session.feed(tab_id, &seed_bytes(HISTORY_LINES)).await;
    session.dump_showing(tab_id, "SGR_FENCE").await;

    let ticket = session.attach(tab_id).await;
    let (accepted, mut data) = dial(&session.socket(), handshake(&ticket.attach_token))
        .await
        .expect("accepted");
    let (decoded, trace) = drain_until_finish(
        &mut data,
        accepted.seq,
        DecodeOpts {
            resize_at_ready: Some((60, 12)),
            ..DecodeOpts::default()
        },
    )
    .await;
    // Non-vacuity, both halves: there was history to lose (the snapshot
    // advertised it at READY, before the resize), and pages really did
    // arrive after the resize to lose it.
    assert!(
        decoded.history_rows_primary > 0,
        "the snapshot has to advertise history for a forfeit to mean anything"
    );
    assert!(
        trace.pages_after_resize > 0,
        "the resize has to land while history is still arriving: {trace:?}"
    );
    assert_eq!(
        trace.rows_after_resize, 0,
        "a page validated after a resize prepends nothing: {trace:?}"
    );

    support::resize_tab(&mut session.control, tab_id, 60, 12)
        .await
        .expect("tab.resize");
    let server = session.wait_for_geometry(tab_id, 60, 12).await;
    // Content, not just equality: two blank 60x12 viewports would agree
    // with each other and prove nothing.
    assert!(
        server.rows_text.iter().any(|row| row.contains("SGR_FENCE")),
        "the compared viewport has to carry the seeded content: {:#?}",
        server.rows_text
    );
    assert_eq!(
        dump_decoded(&decoded.terminal, 60),
        server,
        "the viewport still matches; only the scrollback behind it was forfeit"
    );

    session.stop().await;
}
