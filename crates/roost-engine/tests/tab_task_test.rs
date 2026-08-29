#![cfg(feature = "server-vt")]
//! The server-VT tab task (plan 036 C2).
//!
//! `pty_seq_test.rs` pins the default pipeline — reader → publisher →
//! broadcast — and keeps passing unmodified. This file is its twin for
//! the runtime opt-in: the same seq/exit invariants must hold when the
//! tab task, not the publisher, is the numbering authority, plus the
//! things only a server terminal can do (answer queries, dump a screen,
//! replay a ring).
//!
//! Everything drives a REAL supervisor and a real PTY, because the parts
//! most likely to break are the seams: terminal-before-spawn, the
//! bounded reader channel, and the two exit producers. Content that
//! would need a cooperative child is injected with `TabCmd::FeedBytes`,
//! which takes the identical per-chunk path.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use roost_engine::ipc::{DumpData, ResolvedCellData, ResolvedCellsData};
use roost_engine::tab_task::{ServerVtConfig, ServerVtWorkspace, TabCmd, TabError};
use roost_engine::{PtyOutputEvent, PtySupervisor};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

mod support;
use support::{assert_seqs_contiguous_from_one, bytes_of, collect_until_closed, text_of};

const BUDGET: Duration = Duration::from_secs(10);

/// A workspace stub that records what the tab task routed to it, and can
/// be made to block inside `apply_osc` — the only place a test can stall
/// the task from outside and prove the reader's backpressure.
#[derive(Default)]
struct TestWorkspace {
    osc: Mutex<Vec<(i64, u32, String)>>,
    closed: Mutex<Vec<i64>>,
    gate: Mutex<bool>,
    released: Condvar,
}

impl TestWorkspace {
    fn hold(&self) {
        *self.gate.lock().unwrap() = true;
    }

    fn release(&self) {
        *self.gate.lock().unwrap() = false;
        self.released.notify_all();
    }

    fn closed_rows(&self) -> Vec<i64> {
        self.closed.lock().unwrap().clone()
    }

    fn osc_calls(&self) -> Vec<(i64, u32, String)> {
        self.osc.lock().unwrap().clone()
    }
}

impl ServerVtWorkspace for TestWorkspace {
    fn apply_osc(&self, tab_id: i64, command: u32, payload: &str) {
        self.osc
            .lock()
            .unwrap()
            .push((tab_id, command, payload.to_string()));
        let mut held = self.gate.lock().unwrap();
        while *held {
            held = self.released.wait(held).unwrap();
        }
    }

    fn close_row(&self, tab_id: i64) {
        self.closed.lock().unwrap().push(tab_id);
    }
}

fn enabled_supervisor(capture: bool) -> (PtySupervisor, Arc<TestWorkspace>) {
    let workspace = Arc::new(TestWorkspace::default());
    let supervisor = PtySupervisor::new();
    supervisor
        .enable_server_vt(ServerVtConfig::new(workspace.clone()).with_input_capture(capture))
        .expect("server-vt enables once");
    (supervisor, workspace)
}

fn socket(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/roost-tab-task-{name}.sock"))
}

fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

async fn ask<T>(
    commands: &mpsc::Sender<TabCmd>,
    make: impl FnOnce(oneshot::Sender<T>) -> TabCmd,
) -> T {
    let (tx, rx) = oneshot::channel();
    commands.send(make(tx)).await.expect("tab task is alive");
    timeout(BUDGET, rx)
        .await
        .expect("reply in time")
        .expect("reply")
}

async fn capture(commands: &mpsc::Sender<TabCmd>, drain: bool) -> Vec<u8> {
    ask(commands, |reply| TabCmd::CaptureInput { drain, reply }).await
}

async fn feed(commands: &mpsc::Sender<TabCmd>, bytes: &[u8]) {
    commands
        .send(TabCmd::FeedBytes(bytes.to_vec()))
        .await
        .expect("tab task is alive");
}

/// Wait until the tab task has processed everything sent so far. The
/// command channel is FIFO and the task is single-threaded, so any
/// round-tripped command is a fence.
async fn quiesce(commands: &mpsc::Sender<TabCmd>) -> DumpData {
    ask(commands, TabCmd::Dump).await.expect("dump")
}

/// D3, the fence twin: the same invariants `pty_seq_test.rs` pins on the
/// publisher must hold when the tab task assigns the numbers instead —
/// contiguous from 1 in receive order, exactly one `Exit`, and `Exit`
/// carrying its own (highest) ordinal rather than repeating the last
/// `Bytes` seq.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seqs_relocate_to_the_tab_task_without_moving_the_fence() {
    let (sup, workspace) = enabled_supervisor(false);
    let mut output = sup
        .spawn(
            910,
            "/tmp",
            &sh("printf 'ROOST_VT_ONE\\n'; printf 'ROOST_VT_TWO\\n'; exit 0"),
            80,
            24,
            &socket("fence"),
        )
        .expect("spawn");

    let events = collect_until_closed(&mut output, BUDGET).await;
    sup.close(910);

    let text = text_of(&events);
    assert!(
        text.contains("ROOST_VT_ONE") && text.contains("ROOST_VT_TWO"),
        "expected the shell's output, got:\n{text}"
    );
    assert_seqs_contiguous_from_one(&events, "server-vt eof path");
    let last = events.last().expect("at least one event");
    assert!(
        matches!(last, PtyOutputEvent::Exit { .. }),
        "expected Exit last on the EOF path, got {last:?}"
    );
    assert_eq!(
        last.seq(),
        events.len() as u64,
        "Exit must carry its own ordinal, the highest of the spawn"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, PtyOutputEvent::Exit { .. }))
            .count(),
        1,
        "exactly one Exit per spawn"
    );
    assert_eq!(
        workspace.closed_rows(),
        vec![910],
        "the tab task closes the workspace row when the PTY exits"
    );
}

/// D2's exit shape when a background descendant holds the slave fd: the
/// reap deadline routes through the task, which drains what the reader
/// already queued and only then publishes `Exit`. Nothing may be teed
/// after it — the wire is closed even though the VT keeps eating bytes.
///
/// macOS revokes the tty when the session leader exits, collapsing the
/// same script to the plain EOF shape; both are legal, and the assertion
/// (Exit last, exactly one, contiguous) holds either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_descendant_holding_the_slave_fd_cannot_tee_bytes_after_exit() {
    let (sup, _workspace) = enabled_supervisor(false);
    let mut output = sup
        .spawn(
            911,
            "/tmp",
            &sh(
                "trap '' HUP; { sleep 1; printf 'ROOST_VT_TRAILING\\n'; } & \
                 printf 'ROOST_VT_FIRST\\n'; exit 0",
            ),
            80,
            24,
            &socket("descendant"),
        )
        .expect("spawn");

    let events = collect_until_closed(&mut output, Duration::from_secs(20)).await;
    sup.close(911);

    let text = text_of(&events);
    assert!(
        text.contains("ROOST_VT_FIRST"),
        "expected the shell's own output, got:\n{text}"
    );
    assert_seqs_contiguous_from_one(&events, "server-vt descendant path");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, PtyOutputEvent::Exit { .. }))
            .count(),
        1,
        "the reader and the deadline backstop must arbitrate to exactly one Exit"
    );
    let exit_index = events
        .iter()
        .position(|e| matches!(e, PtyOutputEvent::Exit { .. }))
        .expect("the tab must report its exit");
    assert_eq!(
        exit_index,
        events.len() - 1,
        "the tab task must tee nothing after Exit, got {} event(s) after it",
        events.len() - exit_index - 1
    );
    assert!(
        !text.contains("ROOST_VT_TRAILING"),
        "the descendant's post-exit bytes must stay off the wire:\n{text}"
    );
}

/// §6: the server terminal answers device queries itself, exactly once,
/// with nothing attached. `tab.capture_pty_input`'s buffer is every byte
/// the task queued toward the child, so a doubled answer shows up as a
/// second copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_queries_are_answered_exactly_once_with_no_client() {
    let (sup, _workspace) = enabled_supervisor(true);
    let _output = sup
        .spawn(912, "/tmp", &sh("exec sleep 30"), 80, 24, &socket("reply"))
        .expect("spawn");
    let commands = sup.tab_commands(912).expect("server-vt tab task");

    // DA1, DSR cursor position, and an OSC color query — the three
    // families a headless HS-1a session could not answer at all.
    feed(&commands, b"\x1b[c").await;
    feed(&commands, b"\x1b[6n").await;
    feed(&commands, b"\x1b]11;?\x07").await;
    quiesce(&commands).await;

    // `drain: false` is a peek: the same bytes twice, still there.
    let peeked = capture(&commands, false).await;
    assert!(!peeked.is_empty(), "the queries were answered");
    assert_eq!(
        capture(&commands, false).await,
        peeked,
        "a peek must leave the buffer where it found it"
    );

    let captured = capture(&commands, true).await;
    assert_eq!(captured, peeked, "the drain returns what the peek showed");
    let text = String::from_utf8_lossy(&captured).into_owned();
    assert_eq!(
        text.matches("\x1b[?").count(),
        1,
        "expected exactly one primary device attributes reply, got {text:?}"
    );
    assert_eq!(
        text.matches("R").count(),
        1,
        "expected exactly one cursor position report, got {text:?}"
    );
    assert_eq!(
        text.matches("]11;").count(),
        1,
        "expected exactly one OSC 11 color answer, got {text:?}"
    );
    // A second drain returns nothing: the buffer is a drain, not a log.
    let again = capture(&commands, true).await;
    assert!(again.is_empty(), "capture must drain, got {again:?}");

    drop(commands);
    sup.close(912);
}

fn cell_at(dump: &ResolvedCellsData, row: u32, col: u16) -> &ResolvedCellData {
    dump.cells
        .iter()
        .find(|cell| cell.row == row && cell.col == col)
        .unwrap_or_else(|| panic!("no resolved cell at ({row}, {col})"))
}

/// Dump parity: the headless walk goes through the same `RenderedRow`
/// densifier the UIs render from, so the shapes below are the UI's own
/// conventions — spacer columns contribute a space, short rows are
/// `trim_end`ed, absent cells fill in with the terminal's live defaults,
/// and an invisible cursor is omitted entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dump_and_dump_resolved_match_the_shared_densifier() {
    let (sup, _workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(913, "/tmp", &sh("exec sleep 30"), 20, 6, &socket("dump"))
        .expect("spawn");
    let commands = sup.tab_commands(913).expect("server-vt tab task");

    // OSC 11 first: the default background every absent cell resolves
    // to has to be the post-OSC value, not the construction-time one.
    feed(&commands, b"\x1b]11;rgb:12/34/56\x07").await;
    feed(&commands, b"\x1b[2J\x1b[H").await;
    // Row 0: narrow, wide (+ its spacer), and a combining grapheme.
    feed(&commands, "ab\u{6f22}e\u{301}".as_bytes()).await;
    // Row 1: an explicit truecolor background.
    feed(&commands, b"\x1b[2;1H\x1b[48;2;10;20;30mX\x1b[0m").await;
    // Row 2: inverse video.
    feed(&commands, b"\x1b[3;1H\x1b[7mY\x1b[0m").await;
    // Row 3: a short row — the trailing blanks must be trimmed.
    feed(&commands, b"\x1b[4;1Hhi").await;
    // Row 4/5 stay blank. Hide the cursor last.
    feed(&commands, b"\x1b[?25l").await;

    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (20, 6));
    assert_eq!(
        dump.rows_text,
        vec![
            "ab\u{6f22} e\u{301}".to_string(),
            "X".to_string(),
            "Y".to_string(),
            "hi".to_string(),
            String::new(),
            String::new(),
        ],
        "wide-cell spacer, combining grapheme, short row and blank rows"
    );
    assert_eq!(dump.cursor, None, "an invisible cursor is omitted");

    let resolved: ResolvedCellsData = ask(&commands, TabCmd::DumpResolved)
        .await
        .expect("dump_resolved");
    assert_eq!((resolved.cols, resolved.rows), (20, 6));
    assert_eq!(
        resolved.cells.len(),
        20 * 6,
        "dump_resolved is dense: one cell per row/col"
    );

    let default_fg = (0xff, 0xff, 0xff);
    let osc_bg = (0x12, 0x34, 0x56);

    let blank = cell_at(&resolved, 5, 19);
    assert_eq!(
        (
            blank.text.as_str(),
            blank.fg,
            blank.bg,
            blank.has_explicit_bg
        ),
        (" ", default_fg, osc_bg, false),
        "an absent cell fills in from the terminal's live defaults"
    );

    let explicit = cell_at(&resolved, 1, 0);
    assert_eq!(explicit.text, "X");
    assert_eq!(explicit.bg, (10, 20, 30));
    assert!(explicit.has_explicit_bg, "an SGR background is explicit");
    assert!(!explicit.inverse);

    let inverse = cell_at(&resolved, 2, 0);
    assert_eq!(inverse.text, "Y");
    assert!(inverse.inverse);
    assert_eq!(
        (inverse.fg, inverse.bg),
        (osc_bg, default_fg),
        "inverse swaps the resolved pair"
    );
    assert!(
        inverse.has_explicit_bg,
        "inverse paints a background even without an SGR bg"
    );

    let wide = cell_at(&resolved, 0, 2);
    assert_eq!(wide.text, "\u{6f22}");
    let spacer = cell_at(&resolved, 0, 3);
    assert_eq!(spacer.text, " ", "the wide cell's spacer column is blank");
    let combining = cell_at(&resolved, 0, 4);
    assert_eq!(combining.text, "e\u{301}");

    drop(commands);
    sup.close(913);
}

/// D6's ring validity window, at all four boundaries. `last_assigned + 1`
/// is a *hit* with an empty slice — the client missed nothing — which is
/// the case a naive `from_seq <= last_assigned` check gets wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_accepts_the_ring_window_and_the_empty_slice() {
    let (sup, _workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(914, "/tmp", &sh("exec sleep 30"), 20, 6, &socket("resume"))
        .expect("spawn");
    let commands = sup.tab_commands(914).expect("server-vt tab task");

    feed(&commands, b"one").await;
    feed(&commands, b"two").await;
    feed(&commands, b"three").await;
    quiesce(&commands).await;

    let hit = ask(&commands, |reply| TabCmd::Resume { from_seq: 1, reply })
        .await
        .expect("front of the ring resumes");
    assert_eq!(hit.last_assigned, 3);
    assert_eq!(
        hit.slice
            .iter()
            .map(|(seq, data)| (*seq, String::from_utf8_lossy(data).into_owned()))
            .collect::<Vec<_>>(),
        vec![
            (1, "one".to_string()),
            (2, "two".to_string()),
            (3, "three".to_string())
        ]
    );
    assert!(hit.stored_exit.is_none(), "the tab is still running");

    let empty = ask(&commands, |reply| TabCmd::Resume { from_seq: 4, reply })
        .await
        .expect("last_assigned + 1 is a valid empty-slice resume");
    assert!(empty.slice.is_empty());
    assert_eq!(empty.last_assigned, 3);

    let past = ask(&commands, |reply| TabCmd::Resume { from_seq: 5, reply }).await;
    assert!(
        matches!(past, Err(TabError::RingMiss { from_seq: 5, .. })),
        "last_assigned + 2 is a miss, got {past:?}"
    );
    let zero = ask(&commands, |reply| TabCmd::Resume { from_seq: 0, reply }).await;
    assert!(
        matches!(zero, Err(TabError::RingMiss { from_seq: 0, .. })),
        "seq 0 never existed, got {zero:?}"
    );

    drop(commands);
    sup.close(914);
}

/// Eviction: past the byte cap the oldest records go, and a resume that
/// wanted one of them is a miss (the caller's cue to fall back to a
/// snapshot) while the new front still resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_replay_ring_evicts_oldest_first() {
    let (sup, _workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(915, "/tmp", &sh("exec sleep 30"), 20, 6, &socket("ring"))
        .expect("spawn");
    let commands = sup.tab_commands(915).expect("server-vt tab task");

    // Three records of 1 MiB against a 2 MiB cap: exactly one eviction.
    let chunk = vec![b'x'; 1024 * 1024];
    for _ in 0..3 {
        feed(&commands, &chunk).await;
    }
    quiesce(&commands).await;

    let miss = ask(&commands, |reply| TabCmd::Resume { from_seq: 1, reply }).await;
    assert!(
        matches!(
            miss,
            Err(TabError::RingMiss {
                from_seq: 1,
                front: 2,
                last_assigned: 3
            })
        ),
        "the evicted record must be a miss, got {miss:?}"
    );

    let hit = ask(&commands, |reply| TabCmd::Resume { from_seq: 2, reply })
        .await
        .expect("the new ring front still resumes");
    assert_eq!(
        hit.slice.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        vec![2, 3]
    );

    drop(commands);
    sup.close(915);
}

/// A resume that arrives after the tab died still ends in EXIT: the task
/// keeps serving commands past its own exit for exactly this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_after_the_exit_replays_the_stored_exit() {
    let (sup, _workspace) = enabled_supervisor(false);
    let mut output = sup
        .spawn(
            916,
            "/tmp",
            &sh("printf 'bye\\n'; exit 7"),
            20,
            6,
            &socket("postexit"),
        )
        .expect("spawn");
    let commands = sup.tab_commands(916).expect("server-vt tab task");

    let exit = loop {
        match timeout(BUDGET, output.recv()).await {
            Ok(Ok(PtyOutputEvent::Exit { seq, code })) => break (seq, code),
            Ok(Ok(_)) => continue,
            other => panic!("expected an Exit event, got {other:?}"),
        }
    };
    assert_eq!(exit.1, 7, "the child's status reaches the tee");

    let resumed = ask(&commands, |reply| TabCmd::Resume { from_seq: 1, reply })
        .await
        .expect("the ring outlives the child");
    assert_eq!(
        resumed.stored_exit,
        Some(exit),
        "a late resume must still be told the tab exited"
    );
    assert_eq!(
        resumed.last_assigned + 1,
        exit.0,
        "final_seq is the last PTY seq plus one"
    );

    drop(commands);
    sup.close(916);
}

/// Architecture §3's flow control: the reader → task channel is bounded,
/// so a stalled task backs the child up rather than dropping anything.
/// The stall is a blocked `apply_osc` — the one seam a test can hold —
/// and the child pushes far more than the channel can buffer while it
/// is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_task_backs_the_child_up_without_losing_bytes() {
    const LINES: usize = 200;
    const WIDTH: usize = 1024;

    let (sup, workspace) = enabled_supervisor(false);
    workspace.hold();
    // The OSC title lands in the first chunk and parks the task inside
    // `apply_osc`; everything after it queues behind the bounded channel.
    let script = format!(
        "printf '\\033]0;stall\\007'; \
         awk 'BEGIN{{s=\"\"; for(i=0;i<{WIDTH};i++) s=s \"x\"; \
         for(i=0;i<{LINES};i++) print s}}'; exit 0"
    );
    let mut output = sup
        .spawn(917, "/tmp", &sh(&script), 80, 24, &socket("stall"))
        .expect("spawn");

    // Long enough for the child to fill the 32-chunk channel and the
    // kernel PTY buffer and block on write.
    tokio::time::sleep(Duration::from_millis(300)).await;
    workspace.release();

    let events = collect_until_closed(&mut output, BUDGET).await;
    sup.close(917);

    assert_seqs_contiguous_from_one(&events, "stalled task");
    let produced = bytes_of(&events);
    assert_eq!(
        produced.iter().filter(|byte| **byte == b'x').count(),
        LINES * WIDTH,
        "every byte the child wrote while the task was stalled must arrive"
    );
    assert_eq!(
        workspace.osc_calls(),
        vec![(917, 0, "stall".to_string())],
        "the stall was the OSC title the task parked on"
    );
}

/// The reply cap (panel D2): a child that spews queries and never reads
/// its input must not wedge its own tab. The task queues replies into a
/// byte-capped buffer and drops the oldest past it rather than blocking
/// on a full writer channel — so output processing keeps up regardless.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_query_flood_against_a_full_writer_never_blocks_the_task() {
    let (sup, _workspace) = enabled_supervisor(false);
    let mut output = sup
        .spawn(918, "/tmp", &sh("exec sleep 30"), 80, 24, &socket("flood"))
        .expect("spawn");
    let commands = sup.tab_commands(918).expect("server-vt tab task");

    // `sleep` never reads its stdin, so the PTY's input buffer fills,
    // the writer task blocks mid-write, and the writer channel backs up.
    // Each of these chunks answers with well over a kilobyte, so the
    // 64 KiB pending cap is crossed many times over.
    let queries = b"\x1b[6n".repeat(256);
    for _ in 0..200 {
        timeout(BUDGET, async { feed(&commands, &queries).await })
            .await
            .expect("queuing a chunk must never block on the writer");
    }

    // The load-bearing assertion: the task is still servicing its
    // pipeline after the flood.
    feed(&commands, b"\x1b[2J\x1b[Hstill-here").await;
    let dump = quiesce(&commands).await;
    assert_eq!(
        dump.rows_text.first().map(String::as_str),
        Some("still-here"),
        "the tab task must keep processing output through a reply flood"
    );

    // …and the tee kept flowing through it, contiguously. The replies
    // this test floods out get ECHOED by the PTY line discipline
    // (`sleep` never reads, echo is on), and how much of that echo makes
    // it back through the reader depends on the platform's PTY buffer —
    // on a roomy kernel it is enough to lap this plain subscriber. Lag
    // is legal for a subscriber by contract (the authoritative terminal
    // is fed synchronously, not via this broadcast), so the drain
    // tolerates it and pins contiguity from the first event it kept.
    let mut seen = Vec::new();
    loop {
        match timeout(Duration::from_millis(50), output.recv()).await {
            Ok(Ok(event)) => seen.push(event),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        }
    }
    assert!(!seen.is_empty(), "the flood's chunks were teed");
    let first = seen[0].seq();
    for (index, event) in seen.iter().enumerate() {
        assert_eq!(
            event.seq(),
            first + index as u64,
            "gap at teed event {index}"
        );
    }

    drop(commands);
    sup.close(918);
}

/// D1's inertness guarantee: with the runtime flag off — which is every
/// UI build, even one that compiled this feature through unification —
/// no tab task exists and the publisher numbers the stream exactly as
/// `pty_seq_test.rs` pins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_runtime_flag_off_builds_no_tab_task() {
    let sup = PtySupervisor::new();
    let mut output = sup
        .spawn(
            919,
            "/tmp",
            &sh("printf 'ROOST_VT_OFF\\n'; exit 0"),
            80,
            24,
            &socket("off"),
        )
        .expect("spawn");
    assert!(
        sup.tab_commands(919).is_none(),
        "no tab task without the runtime opt-in"
    );
    assert!(sup.server_epoch().is_none(), "no epoch without the opt-in");

    let events = collect_until_closed(&mut output, BUDGET).await;
    sup.close(919);

    let text = text_of(&events);
    assert!(text.contains("ROOST_VT_OFF"), "got:\n{text}");
    assert_seqs_contiguous_from_one(&events, "runtime flag off");
    let last = events.last().expect("at least one event");
    assert!(matches!(last, PtyOutputEvent::Exit { .. }));
}

/// The snapshot fence is the last assigned PTY seq: a client that
/// applies the snapshot then the tee starts at `seq + 1`. Identity comes
/// from the supervisor, so a resuming client can check both halves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_snapshot_carries_the_fence_and_the_stream_identity() {
    let (sup, _workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(920, "/tmp", &sh("exec sleep 30"), 20, 6, &socket("snap"))
        .expect("spawn");
    let commands = sup.tab_commands(920).expect("server-vt tab task");

    feed(&commands, b"\x1b[2J\x1b[Hsnapshot me").await;
    let snapshot = ask(&commands, TabCmd::Snapshot)
        .await
        .expect("the server terminal encodes");
    assert_eq!(snapshot.seq, 1, "the fence is the last assigned PTY seq");
    assert_eq!(snapshot.server_epoch, sup.server_epoch().expect("epoch"));
    assert_eq!(
        snapshot.tab_generation,
        sup.tab_generation(920).expect("generation")
    );
    assert!(!snapshot.bytes.is_empty(), "a snapshot has content");

    drop(commands);
    sup.close(920);
}

/// A resize orders the server terminal ahead of TIOCSWINSZ and drains
/// the mode-2048 report the resize itself produces — a report that fires
/// outside any `vt_write`, so a drain that only followed writes would
/// lose it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resize_drains_the_in_band_size_report() {
    let (sup, _workspace) = enabled_supervisor(true);
    let _output = sup
        .spawn(921, "/tmp", &sh("exec sleep 30"), 20, 6, &socket("resize"))
        .expect("spawn");
    let commands = sup.tab_commands(921).expect("server-vt tab task");

    // Mode 2048 on; drop the enable's own report before resizing.
    feed(&commands, b"\x1b[?2048h").await;
    quiesce(&commands).await;
    let _ = capture(&commands, true).await;

    commands
        .send(TabCmd::Resize {
            cols: 40,
            rows: 12,
            cell_w: 8,
            cell_h: 16,
            ack: None,
        })
        .await
        .expect("tab task is alive");
    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (40, 12), "the server VT resized");

    let captured = capture(&commands, true).await;
    let text = String::from_utf8_lossy(&captured).into_owned();
    assert!(
        text.contains("48;12;40"),
        "expected the mode-2048 in-band size report, got {text:?}"
    );

    drop(commands);
    sup.close(921);
}

/// OSC that carries workspace facts still reaches the workspace — the
/// job `drain.rs` did in HS-1a, now done by the task that also owns the
/// terminal. Client-local effects (clipboard, pointer) are dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_osc_routes_through_the_seam_and_effects_are_dropped() {
    let (sup, workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(922, "/tmp", &sh("exec sleep 30"), 20, 6, &socket("osc"))
        .expect("spawn");
    let commands = sup.tab_commands(922).expect("server-vt tab task");

    feed(&commands, b"\x1b]0;build\x07").await;
    feed(&commands, b"\x1b]7;file:///tmp/work\x07").await;
    feed(&commands, b"\x1b]52;c;aGVsbG8=\x07").await;
    feed(&commands, b"\x1b]22;pointer\x07").await;
    quiesce(&commands).await;

    assert_eq!(
        workspace.osc_calls(),
        vec![
            (922, 0, "build".to_string()),
            (922, 7, "file:///tmp/work".to_string()),
        ],
        "only workspace-directed actions cross the seam"
    );

    drop(commands);
    sup.close(922);
}
