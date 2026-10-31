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

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use roost_engine::ipc::{DumpData, ResolvedCellData, ResolvedCellsData};
use roost_engine::osc::{OscColorSnapshot, OscRgb};
use roost_engine::program_status::RawOscAuthority;
use roost_engine::tab_task::{
    AttachDecision, AttachPause, ServerVtConfig, ServerVtWorkspace, TabCmd, TabError,
    SERVER_VT_CONTINUATION_MAX,
};
use roost_engine::TabOpener;
use roost_engine::{
    Geometry, PtyOutputEvent, PtySupervisor, TabEffectKind, Workspace, WorkspaceEvent,
};
use roost_ipc::agent::{AgentLifecycle, OwnershipAction, ShellState, TabAgentReportParams};
use roost_ipc::dataframe::InputTake;
use roost_ipc::messages::{
    bytes_base64, AttachPayloadKind, ClipboardEffectTarget, SizeOwner, Tab,
    CLIPBOARD_EFFECT_MAX_BYTES,
};
use roost_ipc::program_status::{
    DecidedBy, ProgramStatusKind, ProgramStatusReport, ProgramStatusState, PromptMark, StatusEvent,
};
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
    /// Every `apply_osc` and `apply_program_status` call, in call order:
    /// where a chunk's status batch and its OSC actions interleave.
    order: Mutex<Vec<Seam>>,
    closed: Mutex<Vec<i64>>,
    effects: Mutex<Vec<(i64, TabEffectKind)>>,
    resized: Mutex<Vec<Resized>>,
    /// The newest pipeline registered per tab: a resize from an older one
    /// is not published — `Workspace`'s rule, under one lock as there.
    pipelines: Mutex<HashMap<i64, u64>>,
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

    fn effects(&self) -> Vec<(i64, TabEffectKind)> {
        self.effects.lock().unwrap().clone()
    }

    fn order(&self) -> Vec<Seam> {
        self.order.lock().unwrap().clone()
    }

    fn resized(&self) -> Vec<Resized> {
        self.resized.lock().unwrap().clone()
    }
}

/// One `tab.resized` the task published: tab, cols, rows, owner.
type Resized = (i64, u16, u16, Option<SizeOwner>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seam {
    Osc(u32, String),
    Status(Vec<StatusEvent>),
}

impl ServerVtWorkspace for TestWorkspace {
    fn apply_osc(
        &self,
        tab_id: i64,
        command: u32,
        payload: &str,
        _authority: &mut RawOscAuthority,
    ) {
        self.order
            .lock()
            .unwrap()
            .push(Seam::Osc(command, payload.to_string()));
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

    fn tab_effect(&self, tab_id: i64, effect: TabEffectKind) {
        self.effects.lock().unwrap().push((tab_id, effect));
    }

    fn apply_program_status(
        &self,
        _tab_id: i64,
        events: Vec<StatusEvent>,
        _now_ms: i64,
    ) -> RawOscAuthority {
        self.order.lock().unwrap().push(Seam::Status(events));
        RawOscAuthority::default()
    }

    fn tab_pipeline(&self, tab_id: i64, generation: u64) {
        let mut pipelines = self.pipelines.lock().unwrap();
        let newest = pipelines.entry(tab_id).or_default();
        *newest = (*newest).max(generation);
    }

    fn tab_resized(
        &self,
        tab_id: i64,
        generation: u64,
        cols: u16,
        rows: u16,
        owner: Option<SizeOwner>,
    ) {
        let pipelines = self.pipelines.lock().unwrap();
        if generation < pipelines.get(&tab_id).copied().unwrap_or(0) {
            return;
        }
        self.resized
            .lock()
            .unwrap()
            .push((tab_id, cols, rows, owner));
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
    ask(commands, |reply| TabCmd::Dump {
        scrollback: 0,
        reply,
    })
    .await
    .expect("dump")
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
            &TabOpener::None,
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
            &TabOpener::None,
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
        .spawn(
            912,
            "/tmp",
            &sh("exec sleep 30"),
            80,
            24,
            &socket("reply"),
            &TabOpener::None,
        )
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
        .spawn(
            913,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("dump"),
            &TabOpener::None,
        )
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

/// `tab.memory_usage`'s session half (plan 082 D6.2): the task reads its
/// own terminal, so history written through the tab grows the figure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_usage_reads_the_tabs_own_terminal() {
    let (sup, _workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(
            935,
            "/tmp",
            &sh("exec sleep 30"),
            80,
            24,
            &socket("memory"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(935).expect("server-vt tab task");

    let before = ask(&commands, TabCmd::MemoryUsage).await.expect("memory");
    let history: String = (0..2000)
        .map(|line| format!("history line {line}\r\n"))
        .collect();
    feed(&commands, history.as_bytes()).await;
    quiesce(&commands).await;
    let after = ask(&commands, TabCmd::MemoryUsage).await.expect("memory");

    assert!(before.primary.pages > 0, "{before:?}");
    assert!(
        after.primary.pages > before.primary.pages,
        "{before:?} -> {after:?}"
    );
    assert_eq!(
        after.resident_bytes,
        after.primary.resident_bytes + after.alternate.resident_bytes
    );

    drop(commands);
    sup.close(935);
}

/// D6's ring validity window, at all four boundaries. `last_assigned + 1`
/// is a *hit* with an empty slice — the client missed nothing — which is
/// the case a naive `from_seq <= last_assigned` check gets wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_accepts_the_ring_window_and_the_empty_slice() {
    let (sup, _workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(
            914,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("resume"),
            &TabOpener::None,
        )
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
        .spawn(
            915,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("ring"),
            &TabOpener::None,
        )
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
    // The child parks at `read` until this test holds the command
    // handle: the reaper drops the session on exit, so a child that ran
    // to completion first would leave `tab_commands` nothing to return.
    let mut output = sup
        .spawn(
            916,
            "/tmp",
            &sh("read _; printf 'bye\\n'; exit 7"),
            20,
            6,
            &socket("postexit"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(916).expect("server-vt tab task");
    commands
        .send(TabCmd::Input {
            data: b"\n".to_vec(),
            from: None,
            take: InputTake::None,
        })
        .await
        .expect("tab task is alive");

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
        .spawn(
            917,
            "/tmp",
            &sh(&script),
            80,
            24,
            &socket("stall"),
            &TabOpener::None,
        )
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
    const CHUNKS: usize = 200;
    const MARKER: &[u8] = b"\x1b[2J\x1b[Hstill-here";

    let (sup, _workspace) = enabled_supervisor(false);
    // `-echo` stops the line discipline bouncing every CPR reply back at
    // the reader. That echo is the whole flake this fence removes: the
    // replies land after the marker and overwrite row 0, and thousands of
    // them lap the 256-slot broadcast. Unfixed, this test failed 8/100 on
    // Linux in exactly those two shapes; with echo off it is 100/100.
    //
    // `-icanon` is what makes "the writer is stuck on a full input
    // buffer" true on both kernels: macOS rings the BEL on a full
    // *canonical* buffer no matter what `-echo` says (each bell one more
    // tee event), and Linux only ever stalls the master in non-canonical
    // mode. On Linux that stall used to be a `write(2)` nothing could
    // release — 100/100 hangs, #409 — so this test ran it on macOS only;
    // the writer now waits on the reactor and the slave's hang-up ends
    // the wait, on both.
    let line_mode = "stty -echo -icanon";
    let mut output = sup
        .spawn(
            918,
            "/tmp",
            &sh(&format!("{line_mode}; printf READY; exec sleep 30")),
            80,
            24,
            &socket("flood"),
            &TabOpener::None,
        )
        .expect("spawn");
    // Close on every exit path, not just the happy one. The writer task
    // ends this test waiting on a full PTY input buffer, and close is
    // what ends that wait: SIGHUP takes the child, the slave closes, the
    // master reports the hang-up. A failed assertion would otherwise
    // unwind past the close and leave the child alive until `sleep 30`
    // expires. Declared before `commands` so it drops after it: sender
    // gone first, then close, as before.
    struct CloseOnDrop<'a>(&'a PtySupervisor, i64);
    impl Drop for CloseOnDrop<'_> {
        fn drop(&mut self) {
            self.0.close(self.1);
        }
    }
    let _close = CloseOnDrop(&sup, 918);
    let commands = sup.tab_commands(918).expect("server-vt tab task");

    // Nothing may be written before `stty` has run, or the replies to it
    // come back echoed.
    let mut greeting = Vec::new();
    while !greeting.windows(5).any(|window| window == b"READY") {
        match timeout(BUDGET, output.recv()).await {
            Ok(Ok(PtyOutputEvent::Bytes { data, .. })) => greeting.extend_from_slice(&data),
            other => panic!("expected the child's READY, got {other:?}"),
        }
    }

    // `sleep` never reads its stdin, so the PTY's input buffer fills,
    // the writer task stalls mid-chunk, and the writer channel backs up.
    // Each of these chunks answers with well over a kilobyte, so the
    // 64 KiB pending cap is crossed many times over.
    let queries = b"\x1b[6n".repeat(256);
    for _ in 0..CHUNKS {
        timeout(BUDGET, async { feed(&commands, &queries).await })
            .await
            .expect("queuing a chunk must never block on the writer");
    }

    // The load-bearing assertion: the task is still servicing its
    // pipeline after the flood.
    timeout(BUDGET, feed(&commands, MARKER))
        .await
        .expect("queuing the marker must never block on the writer");
    let dump = quiesce(&commands).await;
    let row = dump.rows_text.first().map(String::as_str).unwrap_or("");
    assert_eq!(
        row, "still-here",
        "the tab task must keep processing output through a reply flood"
    );

    // …and the tee carried exactly the flood, contiguously: one event
    // per `FeedBytes` chunk plus the marker, which fits the 256-slot
    // broadcast. With echo off nothing else can reach the reader, so a
    // lag here is a defect rather than a lossy-consumer fact.
    let mut seen = Vec::new();
    loop {
        match timeout(Duration::from_millis(50), output.recv()).await {
            Ok(Ok(event)) => seen.push(event),
            Ok(Err(err)) => panic!("the flood must fit the broadcast; got {err}"),
            Err(_) => break,
        }
    }
    assert_eq!(
        seen.len(),
        CHUNKS + 1,
        "every flood chunk and the marker was teed, exactly once each"
    );
    let first = seen[0].seq();
    for (index, event) in seen.iter().enumerate() {
        assert_eq!(
            event.seq(),
            first + index as u64,
            "gap at teed event {index}"
        );
    }
    for (index, event) in seen[..CHUNKS].iter().enumerate() {
        assert_eq!(
            bytes_of(std::slice::from_ref(event)),
            queries,
            "teed event {index} is not the flood chunk verbatim"
        );
    }
    assert_eq!(
        bytes_of(&seen[seen.len() - 1..]),
        MARKER,
        "the last teed event is the marker"
    );
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
            &TabOpener::None,
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
        .spawn(
            920,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("snap"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(920).expect("server-vt tab task");

    feed(&commands, b"\x1b[2J\x1b[Hsnapshot me").await;
    let snapshot = ask(&commands, |reply| TabCmd::Snapshot {
        kind: AttachPayloadKind::from(AttachPayloadKind::GHOSTTY_SNAPSHOT),
        reply,
    })
    .await
    .expect("the server terminal encodes");
    assert_eq!(snapshot.seq, 1, "the fence is the last assigned PTY seq");
    assert_eq!(snapshot.server_epoch, sup.server_epoch().expect("epoch"));
    assert_eq!(
        snapshot.tab_generation,
        sup.tab_generation(920).expect("generation")
    );
    assert!(!snapshot.bytes.is_empty(), "a snapshot has content");

    // Every kind the encoder serves is named. An unknown one fails
    // rather than falling back to GHOSTSNP, which would put libghostty's
    // binary state on the wire under a name no build check guards.
    let unknown = ask(&commands, |reply| TabCmd::Snapshot {
        kind: AttachPayloadKind::from("sixel-mosaic-v9"),
        reply,
    })
    .await;
    assert!(
        matches!(unknown, Err(TabError::SnapshotFailed(_))),
        "an unknown kind is not encodable, got {:?}",
        unknown.map(|encoded| encoded.bytes.len())
    );

    drop(commands);
    sup.close(920);
}

/// A `vt` encode that found the parser inside a sequence longer than the
/// retained continuation parks: only a later chunk can change the
/// answer. A tab that exits has no later chunks, so the reply is owed
/// *then* — leaving it parked would make an attach that can never be
/// satisfied sit out the client's whole time budget before failing, and
/// the re-attach it prompts land in the same dead state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_vt_encode_is_failed_by_the_tab_s_exit() {
    let (sup, _workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(
            925,
            "/tmp",
            &sh("read _"),
            20,
            6,
            &socket("vt-park"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(925).expect("server-vt tab task");

    // A DCS left open, its body past what libghostty retains. DCS rather
    // than OSC so the unfinished sequence has no side effect of its own.
    let mut unfinished = Vec::from(&b"\x1bP"[..]);
    unfinished.extend(std::iter::repeat_n(b'x', 2 * SERVER_VT_CONTINUATION_MAX));
    feed(&commands, &unfinished).await;

    let (reply, parked) = oneshot::channel();
    commands
        .send(TabCmd::Snapshot {
            kind: AttachPayloadKind::from(AttachPayloadKind::VT),
            reply,
        })
        .await
        .expect("tab task is alive");
    sup.close(925);

    let answer = timeout(BUDGET, parked)
        .await
        .expect("the exit answers the parked encode")
        .expect("the reply is not dropped");
    assert!(
        matches!(answer, Err(TabError::SnapshotFailed(_))),
        "a tab that cannot ever be encoded says so: {answer:?}"
    );
}

/// Attach `id` as `client` and commit it, waiting out the child half of
/// any take — an attachment whose accepted reply is on the wire. `true`
/// when it took the tab.
async fn attached(
    commands: &mpsc::Sender<TabCmd>,
    tab_id: i64,
    id: u64,
    client: &str,
    take: bool,
    geometry: Geometry,
) -> bool {
    let decision = decide(commands, id, client, take, geometry).await;
    let took = decision
        .settled(tab_id)
        .await
        .expect("the child took the size");
    commands
        .send(TabCmd::AttachCommitted { id })
        .await
        .expect("tab task is alive");
    took
}

/// The attach decision alone, its child half still in flight.
async fn decide(
    commands: &mpsc::Sender<TabCmd>,
    id: u64,
    client: &str,
    take: bool,
    geometry: Geometry,
) -> AttachDecision {
    ask(commands, |reply| TabCmd::Attach {
        id,
        client_id: client.into(),
        label: format!("{client}-label"),
        take,
        geometry,
        reply,
    })
    .await
}

/// A background `tab.resize`, answered after the decision.
async fn resize_grid(
    commands: &mpsc::Sender<TabCmd>,
    cols: u16,
    rows: u16,
) -> Result<(), TabError> {
    ask(commands, |reply| TabCmd::ResizeGrid { cols, rows, reply }).await
}

fn geometry(cols: u16, rows: u16, cell_w: u32, cell_h: u32) -> Geometry {
    Geometry {
        cols,
        rows,
        cell_w,
        cell_h,
    }
}

/// A resize orders the server terminal ahead of TIOCSWINSZ and drains
/// the mode-2048 report the resize itself produces — a report that fires
/// outside any `vt_write`, so a drain that only followed writes would
/// lose it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resize_drains_the_in_band_size_report() {
    let (sup, _workspace) = enabled_supervisor(true);
    let _output = sup
        .spawn(
            921,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("resize"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(921).expect("server-vt tab task");

    // Mode 2048 on; drop the enable's own report before resizing.
    feed(&commands, b"\x1b[?2048h").await;
    quiesce(&commands).await;
    let _ = capture(&commands, true).await;

    attached(&commands, 921, 1, "a", true, geometry(40, 12, 8, 16)).await;
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

/// An attach whose take finds the geometry already applied is still
/// answered, and answered successfully.
///
/// That path queues a redundant winsize so the decision's child half
/// fences on the writer's FIFO: a background resize queues its own
/// winsize with no ack, so a waiter arriving at the same tuple would
/// otherwise be told the child was sized while that `ioctl` was still
/// queued, and an attach that takes is exactly such a waiter. **What this
/// pins is that the extra write cannot strand the waiter** — the ordering
/// it buys is not observable from here, because forcing the writer to lag
/// needs a child that has stopped reading and an assertion on an ack that
/// does *not* fire. Stated rather than tested, deliberately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_take_at_the_grid_already_applied_is_still_answered() {
    let (sup, _workspace) = enabled_supervisor(true);
    let _output = sup
        .spawn(
            931,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("fence"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(931).expect("server-vt tab task");

    // A background resize moves the grid without waiting for the child.
    resize_grid(&commands, 80, 24)
        .await
        .expect("an unowned tab takes a background resize");

    // The same tuple the background resize just recorded. The task must
    // not answer off the record alone.
    let decision = decide(&commands, 1, "a", true, geometry(80, 24, 0, 0)).await;
    assert!(decision.provisional, "an unowned tab is taken");
    timeout(BUDGET, decision.settled(931))
        .await
        .expect("the child half is answered")
        .expect("the child was sized");

    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (80, 24), "the grid stands");

    drop(commands);
    sup.close(931);
}

/// `TabCmd::ResizeGrid` states the grid and lets the task fill the cell
/// metrics in from the ones the tab already holds, so the report the
/// resize drains quotes the pixels the last owner declared.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grid_resize_keeps_the_cell_metrics_the_tab_holds() {
    let (sup, _workspace) = enabled_supervisor(true);
    let _output = sup
        .spawn(
            922,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("resize-grid"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(922).expect("server-vt tab task");

    // One client states all four numbers and leaves, so the tab is
    // nobody's again; only the grid moves after it.
    attached(&commands, 922, 1, "a", true, geometry(100, 30, 9, 18)).await;
    commands
        .send(TabCmd::Detach {
            id: 1,
            deliberate: true,
        })
        .await
        .expect("tab task is alive");

    feed(&commands, b"\x1b[?2048h").await;
    quiesce(&commands).await;
    let _ = capture(&commands, true).await;

    resize_grid(&commands, 80, 24)
        .await
        .expect("an unowned tab takes a background resize");
    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (80, 24), "the server VT resized");

    let captured = capture(&commands, true).await;
    let text = String::from_utf8_lossy(&captured).into_owned();
    assert!(
        text.contains("48;24;80;432;720"),
        "expected the report to quote 24x18 by 80x9 pixels, got {text:?}"
    );

    drop(commands);
    sup.close(922);
}

/// A resize the terminal refuses records nothing, so the next attempt at
/// the same size is a real attempt (review F6).
///
/// Identical geometry short-circuits — two clients alternating
/// keystrokes at one size must not pay for a terminal resize per frame —
/// and that is precisely why the recording has to come *after* the
/// apply. Stored first, a refused resize would make every retry of that
/// size a silent no-op, and the tab would report a size it never took.
///
/// A zero-column grid is the resize libghostty refuses (`cols` and
/// `rows` must both be greater than zero); the control ops reject one
/// before it gets here, so this drives the tab task directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_resize_leaves_the_geometry_retryable() {
    let (sup, _workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(
            931,
            "/tmp",
            &sh("exec cat"),
            20,
            6,
            &socket("refused-resize"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(931).expect("server-vt tab task");

    for attempt in 1..=2 {
        let outcome = resize_grid(&commands, 0, 6).await;
        assert!(
            outcome.is_err(),
            "attempt {attempt} must be refused, not short-circuited: {outcome:?}"
        );
    }
    let dump = quiesce(&commands).await;
    assert_eq!(
        (dump.cols, dump.rows),
        (20, 6),
        "the tab reports the size it actually has"
    );

    // And the tab is not wedged: a legal geometry still applies.
    resize_grid(&commands, 40, 12)
        .await
        .expect("a legal resize still lands");
    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (40, 12));

    drop(commands);
    sup.close(931);
}

/// A take's child half means **both** halves (review F9) — and is waited
/// out by the attach, never by the tab task.
///
/// The child's `TIOCSWINSZ` happens on the PTY writer's own task — one
/// FIFO shared with client input, so the winsize can only land once
/// everything queued ahead of it has (#80). First that the child really
/// is told: it reads its own winsize back. Then that the wait is
/// genuinely behind the FIFO: the child stops reading, the writer blocks
/// on a large write, and the decision's child half must not settle until
/// that clears — while the decision itself, and the next command, are
/// answered at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_take_waits_for_the_childs_winsize_outside_the_tab_task() {
    let (sup, _workspace) = enabled_supervisor(false);
    // Reports its winsize on demand and nothing else, so what comes back
    // is what the child saw at the moment it was asked.
    let _output = sup
        .spawn(
            932,
            "/tmp",
            &sh("while read _line; do stty size; done"),
            20,
            6,
            &socket("acked-resize"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(932).expect("server-vt tab task");

    attached(&commands, 932, 1, "a", true, geometry(40, 12, 8, 16)).await;

    commands
        .send(TabCmd::Input {
            data: b"\n".to_vec(),
            from: None,
            take: InputTake::None,
        })
        .await
        .expect("tab task is alive");

    let deadline = std::time::Instant::now() + BUDGET;
    loop {
        let dump = quiesce(&commands).await;
        if dump.rows_text.iter().any(|row| row.contains("12 40")) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the child never reported the new winsize; rows were {:?}",
            dump.rows_text
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    drop(commands);
    sup.close(932);

    // A child that never reads its input. Past the tty's own input
    // buffer the writer task blocks in `write_all_nonblocking`, and the
    // resize behind it cannot reach the `ioctl`.
    let _output = sup
        .spawn(
            933,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("stalled"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(933).expect("server-vt tab task");
    // Newline-terminated on purpose: in canonical mode the tty
    // *discards* a line that never ends, and discarding is not blocking.
    // Completed lines fill the reader's buffer instead, the line
    // discipline throttles, and the master write stops making progress.
    let stalling: Vec<u8> = std::iter::repeat_n(b'x', 63)
        .chain(std::iter::once(b'\n'))
        .cycle()
        .take(512 * 1024)
        .collect();
    commands
        .send(TabCmd::Input {
            data: stalling,
            from: None,
            take: InputTake::None,
        })
        .await
        .expect("tab task is alive");

    let decision = decide(&commands, 1, "a", true, geometry(40, 12, 8, 16)).await;
    assert!(decision.provisional);
    assert!(
        timeout(Duration::from_millis(300), decision.settled(933))
            .await
            .is_err(),
        "the take's child half must not settle while the child's winsize is still queued"
    );
    // And the task itself is not blocked behind that wait: it answers
    // the very next command.
    let dump = quiesce(&commands).await;
    assert_eq!(
        (dump.cols, dump.rows),
        (40, 12),
        "the server terminal half applied without waiting on the child"
    );

    drop(commands);
    sup.close(933);
}

/// Once the tab has exited its size is frozen: an attach that asks to
/// take answers at once, and nothing about the tab moves (plan 084 D3:
/// "no GRID after EXIT").
///
/// It also keeps review F3's promise. An attach to an exited tab that
/// waited on a writer that no longer exists would sit out the whole
/// `WINSIZE_ACK_BUDGET` and then report the opposite of the truth, which
/// turns into a refused attach five seconds late; the freeze answers
/// before any winsize is queued at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_take_after_the_tab_exited_answers_at_once_and_changes_nothing() {
    let (sup, _workspace) = enabled_supervisor(false);
    let mut output = sup
        .spawn(
            934,
            "/tmp",
            &sh("exit 0"),
            20,
            6,
            &socket("writer-gone"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(934).expect("server-vt tab task");

    loop {
        match timeout(BUDGET, output.recv()).await {
            Ok(Ok(PtyOutputEvent::Exit { .. })) => break,
            Ok(Ok(_)) => continue,
            other => panic!("expected an Exit event, got {other:?}"),
        }
    }
    // The child took the slave with it, so this write to the master
    // fails and the writer task ends.
    commands
        .send(TabCmd::Input {
            data: b"x\n".to_vec(),
            from: None,
            take: InputTake::None,
        })
        .await
        .expect("the tab task outlives its child");

    for (attempt, rows) in (7..=12u16).enumerate() {
        let decision = decide(
            &commands,
            attempt as u64,
            "a",
            true,
            geometry(40, rows, 8, 16),
        )
        .await;
        assert!(!decision.provisional, "an exited tab is taken by nobody");
        timeout(Duration::from_secs(1), decision.settled(934))
            .await
            .unwrap_or_else(|_| panic!("attach {attempt} sat out the winsize budget"))
            .unwrap_or_else(|error| panic!("attach {attempt} must settle: {error}"));
    }
    assert!(resize_grid(&commands, 40, 12).await.is_ok());
    let dump = quiesce(&commands).await;
    assert_eq!(
        (dump.cols, dump.rows),
        (20, 6),
        "the exited tab's size is frozen"
    );
    assert!(
        matches!(
            output.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed)
        ),
        "nothing is teed after EXIT"
    );

    drop(commands);
    sup.close(934);
}

/// A background resize of a tab somebody owns is refused with the
/// owner's label and changes nothing (rule 8); an unowned tab takes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_background_resize_of_an_owned_tab_is_refused_with_its_owners_label() {
    let (sup, _workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(
            935,
            "/tmp",
            &sh("exec cat"),
            20,
            6,
            &socket("owned"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(935).expect("server-vt tab task");

    attached(&commands, 935, 1, "desk", true, geometry(100, 30, 8, 16)).await;
    match resize_grid(&commands, 60, 20).await {
        Err(TabError::Owned(label)) => assert_eq!(label, "desk-label"),
        other => panic!("expected owned, got {other:?}"),
    }
    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (100, 30));

    commands
        .send(TabCmd::ReleaseSize {
            client_id: "desk".into(),
            reply: oneshot::channel().0,
        })
        .await
        .expect("tab task is alive");
    resize_grid(&commands, 60, 20)
        .await
        .expect("a released tab is nobody's");
    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (60, 20));

    drop(commands);
    sup.close(935);
}

/// `tab.fail_next_resize`: the take it fails changes nothing, the input
/// that asked for it is still delivered, and a background resize it
/// fails reports the failure (rule 3's failed apply).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_apply_keeps_the_owner_and_still_delivers_the_input() {
    let (sup, workspace) = enabled_supervisor(true);
    let mut tee = sup
        .spawn(
            936,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("failed"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(936).expect("server-vt tab task");

    attached(&commands, 936, 1, "a", true, geometry(80, 24, 8, 16)).await;
    attached(&commands, 936, 2, "b", false, geometry(100, 30, 8, 16)).await;
    let _ = capture(&commands, true).await;
    let announced = workspace.resized().len();

    commands
        .send(TabCmd::FailNextResize)
        .await
        .expect("tab task is alive");
    commands
        .send(TabCmd::Input {
            data: b"typed".to_vec(),
            from: Some(2),
            take: InputTake::Before,
        })
        .await
        .expect("tab task is alive");
    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (80, 24), "the size did not move");
    assert_eq!(
        capture(&commands, true).await,
        b"typed",
        "the bytes went anyway"
    );
    assert_eq!(
        workspace.resized().len(),
        announced,
        "nothing changed, so nothing is announced"
    );

    // The next take is a real one.
    commands
        .send(TabCmd::TakeFrom { id: 2 })
        .await
        .expect("tab task is alive");
    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (100, 30));
    let (_, cols, rows, owner) = workspace.resized().last().cloned().expect("announced");
    assert_eq!((cols, rows), (100, 30));
    assert_eq!(owner.map(|o| o.client_id).as_deref(), Some("b"));
    let grids: Vec<_> = std::iter::from_fn(|| tee.try_recv().ok())
        .filter_map(|event| match event {
            PtyOutputEvent::Grid { cols, rows, .. } => Some((cols, rows)),
            _ => None,
        })
        .collect();
    assert_eq!(grids.last(), Some(&(100, 30)), "and teed in band beside it");

    commands
        .send(TabCmd::ReleaseSize {
            client_id: "b".into(),
            reply: oneshot::channel().0,
        })
        .await
        .expect("tab task is alive");
    commands
        .send(TabCmd::ReleaseSize {
            client_id: "a".into(),
            reply: oneshot::channel().0,
        })
        .await
        .expect("tab task is alive");
    commands
        .send(TabCmd::FailNextResize)
        .await
        .expect("tab task is alive");
    let dump = quiesce(&commands).await;
    let before = (dump.cols, dump.rows);
    assert!(
        matches!(
            resize_grid(&commands, 60, 20).await,
            Err(TabError::ResizeFailed(_))
        ),
        "the failure is reported"
    );
    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), before, "and changed nothing");

    drop(commands);
    sup.close(936);
}

/// A dropped owner keeps the tab through its grace, and the task's own
/// timer hands it on when the grace runs out (rule 7).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_grace_timer_hands_a_dropped_owners_tab_on() {
    let workspace = Arc::new(TestWorkspace::default());
    let sup = PtySupervisor::new();
    sup.enable_server_vt(
        ServerVtConfig::new(workspace.clone()).with_owner_grace(Duration::from_millis(200)),
    )
    .expect("server-vt enables once");
    let _output = sup
        .spawn(
            937,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("grace"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(937).expect("server-vt tab task");

    attached(&commands, 937, 1, "a", true, geometry(80, 24, 8, 16)).await;
    attached(&commands, 937, 2, "b", false, geometry(100, 30, 8, 16)).await;
    commands
        .send(TabCmd::Detach {
            id: 1,
            deliberate: false,
        })
        .await
        .expect("tab task is alive");
    let dump = quiesce(&commands).await;
    assert_eq!((dump.cols, dump.rows), (80, 24), "nothing resizes in grace");
    assert!(
        sup.holds_grace("a"),
        "the grace record keeps a's label in the registry"
    );

    let deadline = std::time::Instant::now() + BUDGET;
    loop {
        let dump = quiesce(&commands).await;
        if (dump.cols, dump.rows) == (100, 30) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the grace never ran out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (_, _, _, owner) = workspace.resized().last().cloned().expect("announced");
    assert_eq!(owner.map(|o| o.client_id).as_deref(), Some("b"));
    assert!(!sup.holds_grace("a"), "and the hold went with it");

    drop(commands);
    sup.close(937);
}

/// `tab.resized` names only the tab id, so a generation still deciding
/// after a respawn took its id must keep its announcements to its own
/// stream, or it would describe the replacement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replaced_generation_never_announces_for_its_successor() {
    let (sup, workspace) = enabled_supervisor(false);
    // Ignores the hangup and keeps the slave open past the SIGKILL, so the
    // old generation goes on deciding for a while after its close — once
    // the trap is in place, which is what the marker says.
    let mut old_output = sup
        .spawn(
            939,
            "/tmp",
            &sh("trap '' HUP; sleep 5 & printf 'TRAPPED\\n'; exec sleep 5"),
            80,
            24,
            &socket("replaced-old"),
            &TabOpener::None,
        )
        .expect("spawn");
    let old = sup.tab_commands(939).expect("server-vt tab task");
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("TRAPPED") {
        match timeout(BUDGET, old_output.recv()).await {
            Ok(Ok(PtyOutputEvent::Bytes { data, .. })) => seen.extend_from_slice(&data),
            Ok(Ok(_)) => {}
            other => panic!("expected the trap's marker, got {other:?}"),
        }
    }
    sup.close(939);
    let _new_output = sup
        .spawn(
            939,
            "/tmp",
            &sh("exec cat"),
            80,
            24,
            &socket("replaced-new"),
            &TabOpener::None,
        )
        .expect("respawn");
    let new = sup.tab_commands(939).expect("the replacement's tab task");

    let decision = decide(&old, 1, "a", true, geometry(100, 30, 8, 16)).await;
    assert!(
        decision.provisional,
        "the old generation must still be deciding for this to test anything"
    );
    quiesce(&old).await;
    attached(&new, 939, 1, "b", true, geometry(90, 25, 8, 16)).await;
    quiesce(&new).await;

    let resized: Vec<_> = workspace
        .resized()
        .into_iter()
        .map(|(_, cols, rows, owner)| (cols, rows, owner.map(|o| o.client_id)))
        .collect();
    assert_eq!(
        resized,
        [(90, 25, Some("b".to_string()))],
        "only the replacement speaks for tab 939"
    );

    drop((old, new));
    sup.close(939);
}

/// Every worker's busy time so far. Only a task that never stops being
/// ready keeps a worker from parking, so on an otherwise quiet runtime
/// this barely moves.
fn runtime_busy() -> Duration {
    let metrics = tokio::runtime::Handle::current().metrics();
    (0..metrics.num_workers())
        .map(|worker| metrics.worker_total_busy_duration(worker))
        .sum()
}

/// A child that exits inside its owner's grace leaves the grace record
/// armed but undecidable — nothing is decided once the wire has closed —
/// so the deadline passing must not wake the task again and again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grace_outlived_by_its_tab_leaves_the_task_idle() {
    const GRACE: Duration = Duration::from_millis(600);
    const WINDOW: Duration = Duration::from_millis(500);
    let workspace = Arc::new(TestWorkspace::default());
    let sup = PtySupervisor::new();
    sup.enable_server_vt(ServerVtConfig::new(workspace).with_owner_grace(GRACE))
        .expect("server-vt enables once");
    let mut output = sup
        .spawn(
            938,
            "/tmp",
            &sh("read line; exit 0"),
            20,
            6,
            &socket("outlived"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(938).expect("server-vt tab task");

    attached(&commands, 938, 1, "a", true, geometry(80, 24, 8, 16)).await;
    let dropped = std::time::Instant::now();
    commands
        .send(TabCmd::Detach {
            id: 1,
            deliberate: false,
        })
        .await
        .expect("tab task is alive");
    commands
        .send(TabCmd::Input {
            data: b"\n".to_vec(),
            from: None,
            take: InputTake::None,
        })
        .await
        .expect("tab task is alive");
    loop {
        match timeout(BUDGET, output.recv()).await {
            Ok(Ok(PtyOutputEvent::Exit { .. })) => break,
            Ok(Ok(_)) => continue,
            other => panic!("expected an Exit event, got {other:?}"),
        }
    }
    assert!(
        dropped.elapsed() < GRACE,
        "the child must exit inside the grace for this to test anything"
    );
    assert!(sup.holds_grace("a"), "the grace is still armed at the exit");

    tokio::time::sleep_until((dropped + GRACE + Duration::from_millis(100)).into()).await;
    let before = runtime_busy();
    tokio::time::sleep(WINDOW).await;
    let busy = runtime_busy() - before;
    assert!(
        busy < WINDOW / 2,
        "the tab task kept a worker busy for {busy:?} of {WINDOW:?} after its grace ran out"
    );

    drop(commands);
    sup.close(938);
}

/// Output the reader had already queued when a resize lands reaches the
/// tee before the resize's `GRID` (plan 084 D3), so a client never
/// applies a grid change ahead of bytes written for the old one.
///
/// The task is parked inside a snapshot hand-off while the child writes
/// and while the take is queued behind it, so on release the queued
/// chunks and the take are both waiting — and an unbiased loop would
/// pick between them at random without the drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn output_already_queued_reaches_the_tee_before_the_grid() {
    for round in 0..4 {
        let workspace = Arc::new(TestWorkspace::default());
        let sup = PtySupervisor::new();
        let (pause, mut releases) = AttachPause::new();
        sup.enable_server_vt(ServerVtConfig::new(workspace.clone()).with_handoff_pause(pause))
            .expect("server-vt enables once");
        let mut tee = sup
            .spawn(
                938,
                "/tmp",
                &sh("read _; head -c 40000 /dev/zero | tr '\\000' x; printf 'ROOST_QUEUED_END'; exec sleep 30"),
                80,
                24,
                &socket("queued"), &TabOpener::None,
            )
            .expect("spawn");
        let commands = sup.tab_commands(938).expect("server-vt tab task");
        attached(&commands, 938, 1, "a", true, geometry(80, 24, 8, 16)).await;
        attached(&commands, 938, 2, "b", false, geometry(100, 30, 8, 16)).await;
        commands
            .send(TabCmd::Input {
                data: b"\n".to_vec(),
                from: None,
                take: InputTake::None,
            })
            .await
            .expect("tab task is alive");

        let (snapshot_tx, _snapshot) = oneshot::channel();
        commands
            .send(TabCmd::Snapshot {
                kind: AttachPayloadKind::from(AttachPayloadKind::VT),
                reply: snapshot_tx,
            })
            .await
            .expect("tab task is alive");
        let release = timeout(BUDGET, releases.recv())
            .await
            .expect("the task parks")
            .expect("the seam is alive");
        // Parked: the child's output piles up in the reader's channel.
        tokio::time::sleep(Duration::from_millis(500)).await;
        commands
            .send(TabCmd::TakeFrom { id: 2 })
            .await
            .expect("tab task is alive");
        let _ = release.send(());
        drop(releases);
        quiesce(&commands).await;

        let mut text = String::new();
        let mut grid_after = None;
        while let Ok(event) = tee.try_recv() {
            match event {
                PtyOutputEvent::Bytes { data, .. } => {
                    text.push_str(&String::from_utf8_lossy(&data));
                }
                PtyOutputEvent::Grid { cols: 100, .. } => grid_after = Some(text.len()),
                _ => {}
            }
        }
        let at = grid_after.expect("the take was announced");
        assert!(
            text[..at].contains("ROOST_QUEUED_END"),
            "round {round}: the GRID overtook output the reader had already queued \
             ({at} of {} bytes before it)",
            text.len()
        );

        drop(commands);
        sup.close(938);
    }
}

/// OSC that carries workspace facts still reaches the workspace — the
/// job `drain.rs` did in HS-1a, now done by the task that also owns the
/// terminal. Client-local effects take the other seam (`tab_effect`),
/// except the ones HS-2 left out of scope: a pointer shape is still
/// dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_osc_and_client_effects_take_their_own_seams() {
    let (sup, workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(
            922,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("osc"),
            &TabOpener::None,
        )
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
        "only workspace-directed actions cross the workspace seam"
    );
    assert_eq!(
        workspace.effects(),
        vec![(
            922,
            TabEffectKind::ClipboardWrite {
                text: "hello".to_string(),
                target: ClipboardEffectTarget::System,
            }
        )],
        "the clipboard write is an effect; the pointer shape is still dropped"
    );

    drop(commands);
    sup.close(922);
}

/// Plan 037 §3.6, the effects envelope's two halves.
///
/// The bell is the interesting one: a BEL that TERMINATES an OSC is not
/// a bell, and telling the two apart is the whole reason the scan owns
/// the count. The primary-selection target rides along so a `p` write
/// cannot be mistaken for a system-clipboard one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bells_and_clipboard_writes_reach_the_effects_seam() {
    let (sup, workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(
            923,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("effects"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(923).expect("server-vt tab task");

    // Two BELs in one chunk, around an OSC 0 whose own BEL terminator
    // must not ring: one `bell` effect for the chunk.
    feed(&commands, b"\x07\x1b]0;title\x07\x07").await;
    // A second chunk rings again — the coalescing is per chunk, not a
    // latch that swallows every later bell.
    feed(&commands, b"\x07").await;
    // A primary-selection write, base64 of "sel".
    feed(&commands, b"\x1b]52;p;c2Vs\x07").await;
    quiesce(&commands).await;

    assert_eq!(
        workspace.effects(),
        vec![
            (923, TabEffectKind::Bell),
            (923, TabEffectKind::Bell),
            (
                923,
                TabEffectKind::ClipboardWrite {
                    text: "sel".to_string(),
                    target: ClipboardEffectTarget::Selection,
                }
            ),
        ]
    );

    drop(commands);
    sup.close(923);
}

/// The cap is on the DECODED payload and it is a drop, not a truncation:
/// half a clipboard is worse than none, and a client that trusted a
/// truncated one would paste it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_clipboard_write_is_dropped_whole() {
    let (sup, workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(
            924,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("cap"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(924).expect("server-vt tab task");

    // Straddle the cap from both sides in one run, so a regression that
    // moved the bound either way shows up here.
    let at_cap = "a".repeat(CLIPBOARD_EFFECT_MAX_BYTES);
    let over_cap = "b".repeat(CLIPBOARD_EFFECT_MAX_BYTES + 1);
    for payload in [&at_cap, &over_cap] {
        let mut osc = b"\x1b]52;c;".to_vec();
        osc.extend_from_slice(bytes_base64::encode(payload.as_bytes()).as_bytes());
        osc.push(0x07);
        feed(&commands, &osc).await;
    }
    quiesce(&commands).await;

    assert_eq!(
        workspace.effects(),
        vec![(
            924,
            TabEffectKind::ClipboardWrite {
                text: at_cap,
                target: ClipboardEffectTarget::System,
            }
        )],
        "exactly the at-cap write survives, and its payload is unmodified"
    );

    drop(commands);
    sup.close(924);
}

/// `session.set_theme` reaches the terminal, not just the tracker: the
/// proof is the terminal's own OSC 11 answer, which libghostty writes
/// through `write_pty` from the colors it actually holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_theme_reseed_changes_what_a_color_query_is_answered_with() {
    let (sup, _workspace) = enabled_supervisor(true);
    let _output = sup
        .spawn(
            925,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("theme"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(925).expect("server-vt tab task");

    // The headless default is white on black.
    feed(&commands, b"\x1b]11;?\x07").await;
    quiesce(&commands).await;
    let before = String::from_utf8_lossy(&capture(&commands, true).await).into_owned();
    assert!(
        before.contains("0000/0000/0000"),
        "expected the headless black background, got {before:?}"
    );

    commands
        .send(TabCmd::SetTheme(
            theme((0xff, 0xff, 0xff), (0x1c, 0x2b, 0x3a)),
            // Any generation newer than the build's (0 here — no
            // session-wide theme was stored before this tab spawned).
            1,
        ))
        .await
        .expect("tab task is alive");
    feed(&commands, b"\x1b]11;?\x07").await;
    quiesce(&commands).await;
    let after = String::from_utf8_lossy(&capture(&commands, true).await).into_owned();
    assert!(
        after.contains("1c1c/2b2b/3a3a"),
        "expected the reseeded background, got {after:?}"
    );

    // A stale fan-out must not roll the terminal back: generation 1 is
    // already applied, so a racing older send (same generation or
    // lower) is ignored — last-writer-wins is decided by the store's
    // counter, not by whose send lands last.
    commands
        .send(TabCmd::SetTheme(
            theme((0x00, 0x00, 0x00), (0x99, 0x99, 0x99)),
            1,
        ))
        .await
        .expect("tab task is alive");
    feed(&commands, b"\x1b]11;?\x07").await;
    quiesce(&commands).await;
    let stale = String::from_utf8_lossy(&capture(&commands, true).await).into_owned();
    assert!(
        stale.contains("1c1c/2b2b/3a3a"),
        "a stale generation must not reseed, got {stale:?}"
    );

    drop(commands);
    sup.close(925);
}

/// The seed is session-wide, so a tab opened AFTER the theme landed
/// starts on it — otherwise every tab created during a session renders
/// the client's colors wrong until the next reseed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tab_opened_after_a_reseed_starts_on_the_new_theme() {
    let (sup, _workspace) = enabled_supervisor(true);
    assert_eq!(
        sup.set_theme(&theme((0xff, 0xff, 0xff), (0x0a, 0x0b, 0x0c)))
            .await
            .expect("server-vt is on"),
        0,
        "no tabs exist yet, so the theme is only recorded"
    );

    let _output = sup
        .spawn(
            926,
            "/tmp",
            &sh("exec sleep 30"),
            20,
            6,
            &socket("theme2"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(926).expect("server-vt tab task");
    feed(&commands, b"\x1b]11;?\x07").await;
    quiesce(&commands).await;
    let answer = String::from_utf8_lossy(&capture(&commands, true).await).into_owned();
    assert!(
        answer.contains("0a0a/0b0b/0c0c"),
        "a tab built after the reseed starts on the stored theme, got {answer:?}"
    );

    drop(commands);
    sup.close(926);
}

/// A theme seed with a distinguishable background; the palette is a
/// per-index ramp so a test that cares can tell entries apart.
fn theme(foreground: OscRgb, background: OscRgb) -> OscColorSnapshot {
    let mut palette = [(0u8, 0u8, 0u8); 256];
    for (index, entry) in palette.iter_mut().enumerate() {
        let value = u8::try_from(index).expect("256 entries fit a u8");
        *entry = (value, value, value);
    }
    OscColorSnapshot::new(foreground, background, foreground, palette)
}

fn osc7501(body: &str) -> Vec<u8> {
    format!("\x1b]7501;{body}\x1b\\").into_bytes()
}

/// Plan 082 D4.1: a session tab's terminal answers the program-status
/// probe itself, once, byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_program_status_probe_is_answered_once() {
    let (sup, _workspace) = enabled_supervisor(true);
    let _output = sup
        .spawn(
            930,
            "/tmp",
            &sh("exec sleep 30"),
            80,
            24,
            &socket("probe"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(930).expect("server-vt tab task");

    feed(&commands, b"\x1b]7501;?\x1b\\").await;
    quiesce(&commands).await;
    assert_eq!(
        capture(&commands, true).await,
        b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\"
    );

    drop(commands);
    sup.close(930);
}

/// Plan 082 D4.3: a chunk's reports and prompt marks reach the workspace,
/// in byte order, in one batch ahead of the same chunk's OSC actions. Plan
/// 083 D-B2: the prompt mark rides only the batch; no OSC action carries
/// it a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chunks_status_events_precede_its_osc_actions() {
    let (sup, workspace) = enabled_supervisor(false);
    let _output = sup
        .spawn(
            931,
            "/tmp",
            &sh("exec sleep 30"),
            80,
            24,
            &socket("order"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(931).expect("server-vt tab task");

    let mut chunk = osc7501("state=working:app=claude-code");
    chunk.extend_from_slice(b"\x1b]133;A\x07\x1b]0;turn\x07");
    chunk.extend_from_slice(&osc7501("state=blocked:kind=permission"));
    feed(&commands, &chunk).await;
    feed(&commands, b"plain output, no events\r\n").await;
    quiesce(&commands).await;

    let report = |state, kind, app: &str| ProgramStatusReport {
        state,
        kind,
        progress: None,
        id: String::new(),
        app: app.into(),
        title: String::new(),
        msg: String::new(),
    };
    assert_eq!(
        workspace.order(),
        vec![
            Seam::Status(vec![
                StatusEvent::Report(report(ProgramStatusState::Working, None, "claude-code")),
                StatusEvent::Prompt(PromptMark::PromptStart),
                StatusEvent::Report(report(
                    ProgramStatusState::Blocked,
                    Some(ProgramStatusKind::Permission),
                    ""
                )),
            ]),
            Seam::Osc(0, "turn".into()),
        ],
        "one batch per chunk with events, ahead of the chunk's OSC actions"
    );

    drop(commands);
    sup.close(931);
}

/// What one run of a byte stream left on its tab, minus the clock.
#[derive(Debug, PartialEq)]
struct Outcome {
    tab: Tab,
    banners: Vec<(String, String)>,
}

/// Feed `chunks` to a fresh session tab of a real workspace and read back
/// its `tab.list` row and the banners it raised. With `hook_owner`, a hook
/// owner still saying Waiting holds the tab first, so the marks move the
/// hook layer under the records (the B3 review's sequence).
async fn run_stream(
    sup: &PtySupervisor,
    workspace: &Arc<Workspace>,
    project: i64,
    hook_owner: bool,
    chunks: &[&[u8]],
) -> Outcome {
    let tab_id = workspace
        .open_tab(project, "/tmp", "t", false, TabOpener::None)
        .unwrap()
        .id;
    if hook_owner {
        let claim = TabAgentReportParams {
            session_id: "s1".into(),
            ..TabAgentReportParams::sessionless(
                tab_id,
                "claude",
                OwnershipAction::Claim,
                Some(AgentLifecycle::Waiting),
            )
        };
        assert!(workspace.agent_report(&claim).unwrap().0);
    }
    let mut events = workspace.subscribe();
    let _output = sup
        .spawn(
            tab_id,
            "/tmp",
            &sh("exec sleep 30"),
            80,
            24,
            &socket("partition"),
            &TabOpener::None,
        )
        .expect("spawn");
    let commands = sup.tab_commands(tab_id).expect("server-vt tab task");
    for chunk in chunks {
        feed(&commands, chunk).await;
    }
    quiesce(&commands).await;
    let mut tab = workspace.tab(tab_id).unwrap();
    drop(commands);
    sup.close(tab_id);

    let mut banners = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let WorkspaceEvent::NotificationFired {
            tab_id: id,
            title,
            body,
            ..
        } = event
        {
            if id == tab_id {
                banners.push((title, body));
            }
        }
    }
    if let Some(record) = tab
        .program_status
        .as_mut()
        .and_then(|status| status.record.as_mut())
    {
        record.updated_at = 0;
    }
    if let Some(owner) = tab.ownership.as_mut() {
        owner.last_event_at = 0;
    }
    tab.id = 0;
    tab.position = 0;
    tab.created_at = 0;
    tab.last_active = 0;
    Outcome { tab, banners }
}

/// Plan 082 B-4's partition invariance, through the real tab task: one
/// byte stream fed whole, split at every byte, and split at random gives
/// the same tab. The stream moves both axes: its OSC 133 marks reach the
/// record layer and the hook layer through libghostty, under a hook owner
/// whose lifecycle the first mark ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn program_status_does_not_depend_on_read_boundaries() {
    let workspace = Arc::new(Workspace::new());
    workspace.set_window_focused(false);
    let project = workspace.create_project("p", "/tmp").unwrap().id;
    // A row no run closes, so closing a run's tab never takes the project.
    workspace
        .open_tab(project, "/tmp", "keep", true, TabOpener::None)
        .unwrap();
    let sup = PtySupervisor::new();
    sup.enable_server_vt(ServerVtConfig::new(workspace.clone()))
        .expect("server-vt enables once");

    // The blocked child right after the prompt mark is the B3 review's
    // sequence: its banner exists only if the record layer sees the hook
    // layer the mark left behind, whichever read the mark arrived in.
    let mut stream = Vec::new();
    for part in [
        osc7501("state=working:app=claude-code:msg=dGhpbmtpbmc="),
        b"\x1b]133;A\x07".to_vec(),
        osc7501("state=blocked:id=build/test:kind=permission:msg=YXBwcm92ZSBCYXNo"),
        b"some output\r\n".to_vec(),
        b"\x1b]133;C\x07".to_vec(),
        osc7501("state=clear:id=build/test"),
        osc7501("state=working:app=claude-code"),
        osc7501("state=done:app=claude-code:title=VGVzdHM="),
        b"\x1b]133;D;0\x07\x1b]133;A\x07".to_vec(),
    ] {
        stream.extend_from_slice(&part);
    }

    let whole = run_stream(&sup, &workspace, project, true, &[&stream]).await;
    for cut in 1..stream.len() {
        let split = run_stream(
            &sup,
            &workspace,
            project,
            true,
            &[&stream[..cut], &stream[cut..]],
        )
        .await;
        assert_eq!(split, whole, "split at byte {cut}");
    }

    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..16 {
        let mut cuts: Vec<usize> = (0..1 + next() % 12)
            .map(|_| 1 + (next() as usize) % (stream.len() - 1))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let bounds: Vec<usize> = [0]
            .into_iter()
            .chain(cuts.iter().copied())
            .chain([stream.len()])
            .collect();
        let chunks: Vec<&[u8]> = bounds.windows(2).map(|w| &stream[w[0]..w[1]]).collect();
        let random = run_stream(&sup, &workspace, project, true, &chunks).await;
        assert_eq!(random, whole, "round {round}, cuts {cuts:?}");
    }

    let status = whole.tab.program_status.as_ref();
    assert_eq!(
        status.map(|status| (status.effective_lifecycle, status.decided_by)),
        Some((AgentLifecycle::Finished, DecidedBy::KeptRecord)),
        "the stream ends on a kept done: {whole:?}"
    );
    assert_eq!(
        whole.banners,
        vec![("t".to_string(), "approve Bash".to_string())],
        "the blocked child announced, under the tab's title since the mark \
         dropped the root it would inherit an app from; the done fell in the \
         startup guard"
    );
}

/// Plan 082 D5.8's amendment, the branch review's sequence through the
/// real tab task: on an unwatched tab no hook owns, a `blocked` record, the
/// program's own OSC 9, then the `A` that ends the record's authority. The
/// OSC 9 falls inside the authority, so the record's banner is the only
/// one, whether the three arrive in one read or the read splits anywhere,
/// the OSC 9's ST included.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raw_notification_is_judged_where_it_falls_whatever_the_reads() {
    let workspace = Arc::new(Workspace::new());
    workspace.set_window_focused(false);
    let project = workspace.create_project("p", "/tmp").unwrap().id;
    workspace
        .open_tab(project, "/tmp", "keep", true, TabOpener::None)
        .unwrap();
    let sup = PtySupervisor::new();
    sup.enable_server_vt(ServerVtConfig::new(workspace.clone()))
        .expect("server-vt enables once");

    for terminator in TERMINATORS {
        let mut stream = osc7501("state=blocked:app=pi");
        stream.extend(osc("9;turn over", terminator));
        stream.extend_from_slice(b"\x1b]133;A\x07");

        let whole = run_stream(&sup, &workspace, project, false, &[&stream]).await;
        assert_eq!(
            whole.banners,
            vec![("Pi".to_string(), "Needs your input".to_string())],
            "the record's banner, and not the OSC 9 it covered: {whole:?}"
        );
        for cut in 1..stream.len() {
            let split = run_stream(
                &sup,
                &workspace,
                project,
                false,
                &[&stream[..cut], &stream[cut..]],
            )
            .await;
            assert_eq!(split, whole, "{terminator:?}: split at byte {cut}");
        }
    }
}

/// The banners a session tab raises for `lead`, a `blocked` record, an
/// `OSC 9` both parsers accept, and the `A` ending the record, in one read,
/// on an unwatched tab no hook owns.
async fn banners_after(lead: &[u8]) -> Vec<(String, String)> {
    let workspace = Arc::new(Workspace::new());
    workspace.set_window_focused(false);
    let project = workspace.create_project("p", "/tmp").unwrap().id;
    workspace
        .open_tab(project, "/tmp", "keep", true, TabOpener::None)
        .unwrap();
    let sup = PtySupervisor::new();
    sup.enable_server_vt(ServerVtConfig::new(workspace.clone()))
        .expect("server-vt enables once");
    let mut stream = lead.to_vec();
    stream.extend_from_slice(&osc7501("state=blocked:app=pi"));
    stream.extend_from_slice(b"\x1b]9;inside\x07\x1b]133;A\x07");
    run_stream(&sup, &workspace, project, false, &[&stream])
        .await
        .banners
}

/// Plan 082 D5.8's amendment: OSC 777 with a title alone is a form only
/// roost-osc raises. It finds no place and is judged at the chunk's end,
/// where nothing decides, so it fires; the agreeing OSC 9 after it keeps
/// its own place inside the record's authority, so it doesn't.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_roost_osc_only_form_does_not_take_another_notifications_place() {
    assert_eq!(
        banners_after(b"\x1b]777;notify;Roost only\x07").await,
        vec![
            ("Pi".to_string(), "Needs your input".to_string()),
            ("Roost only".to_string(), String::new()),
        ]
    );
}

/// Plan 082 D5.8's amendment: a bare `OSC 9;4` is a form only libghostty
/// places (roost-osc reads it as ConEmu). Its place, outside the record's
/// authority, is claimed by nobody, and the agreeing OSC 9 after it still
/// gets its own, inside.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_libghostty_only_place_does_not_shift_another_notifications() {
    assert_eq!(
        banners_after(b"\x1b]9;4\x07").await,
        vec![("Pi".to_string(), "Needs your input".to_string())]
    );
}

/// The banners `stream` raises on an unwatched session tab no hook owns,
/// fed whole and split at every byte: the same each time.
async fn banners_whatever_the_reads(stream: &[u8]) -> Vec<(String, String)> {
    outcome_whatever_the_reads(false, stream).await.banners
}

/// [`outcome_whatever_the_reads`], and split at every byte with an empty
/// feed between the halves.
async fn outcome_however_split(hook_owner: bool, stream: &[u8]) -> Outcome {
    let whole = outcome_whatever_the_reads(hook_owner, stream).await;
    let workspace = Arc::new(Workspace::new());
    workspace.set_window_focused(false);
    let project = workspace.create_project("p", "/tmp").unwrap().id;
    workspace
        .open_tab(project, "/tmp", "keep", true, TabOpener::None)
        .unwrap();
    let sup = PtySupervisor::new();
    sup.enable_server_vt(ServerVtConfig::new(workspace.clone()))
        .expect("server-vt enables once");
    for cut in 0..=stream.len() {
        let split = run_stream(
            &sup,
            &workspace,
            project,
            hook_owner,
            &[&stream[..cut], b"", &stream[cut..]],
        )
        .await;
        assert_eq!(
            split,
            whole,
            "{:?} split at byte {cut} around an empty feed",
            String::from_utf8_lossy(&stream[..stream.len().min(40)])
        );
    }
    whole
}

/// What `stream` leaves on an unwatched session tab, fed whole and split
/// at every byte: the same each time. With `hook_owner`, as
/// [`run_stream`] has it.
async fn outcome_whatever_the_reads(hook_owner: bool, stream: &[u8]) -> Outcome {
    let workspace = Arc::new(Workspace::new());
    workspace.set_window_focused(false);
    let project = workspace.create_project("p", "/tmp").unwrap().id;
    workspace
        .open_tab(project, "/tmp", "keep", true, TabOpener::None)
        .unwrap();
    let sup = PtySupervisor::new();
    sup.enable_server_vt(ServerVtConfig::new(workspace.clone()))
        .expect("server-vt enables once");
    let whole = run_stream(&sup, &workspace, project, hook_owner, &[stream]).await;
    for cut in 1..stream.len() {
        let split = run_stream(
            &sup,
            &workspace,
            project,
            hook_owner,
            &[&stream[..cut], &stream[cut..]],
        )
        .await;
        assert_eq!(
            split,
            whole,
            "{:?} split at byte {cut}",
            String::from_utf8_lossy(&stream[..stream.len().min(40)])
        );
    }
    whole
}

/// The B7 review's first collision: a bare `OSC 9;4` (a place libghostty
/// reports with the text `4`, ConEmu to roost-osc) ahead of a real
/// `OSC 777;notify;4;` inside a `blocked` record's authority. The real one
/// must not claim the ConEmu one's place, from before the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conemu_place_is_not_claimed_by_a_notification_with_its_text() {
    let mut stream = b"\x1b]9;4\x07".to_vec();
    stream.extend_from_slice(&osc7501("state=blocked:app=pi"));
    stream.extend_from_slice(b"\x1b]777;notify;4;\x07\x1b]133;A\x07");
    assert_eq!(
        banners_whatever_the_reads(&stream).await,
        vec![("Pi".to_string(), "Needs your input".to_string())]
    );
}

/// The B7 review's second collision: a tab inside the two bytes of `¢`.
/// libghostty drops it before decoding; roost-osc must too, or the texts
/// differ and the OSC 9 is judged at the chunk's end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_control_inside_a_utf8_sequence_leaves_the_same_text_on_both_sides() {
    let mut stream = osc7501("state=blocked:app=pi");
    stream.extend_from_slice(b"\x1b]9;A\xc2\t\xa2\x07\x1b]133;A\x07");
    assert_eq!(
        banners_whatever_the_reads(&stream).await,
        vec![("Pi".to_string(), "Needs your input".to_string())]
    );
}

/// libghostty's OSC data buffer (`Parser.MAX_BUF`, `osc.zig`, at the pinned
/// SHA): a sequence whose data past `133;` overflows it is dropped whole.
const OSC_BUFFER: usize = 2048;

fn osc133(body: &[u8]) -> Vec<u8> {
    let mut bytes = b"\x1b]133;".to_vec();
    bytes.extend_from_slice(body);
    bytes.push(0x07);
    bytes
}

/// Plan 083 B-1, on a session tab: each D-B3 row moves the hook layer of a
/// tab a hook owner holds, Waiting, as libghostty reads the mark, fed
/// whole and split at every byte. `cmdline_url` bodies sit on either side
/// of libghostty's buffer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_osc_133_form_moves_the_hook_layer_as_libghostty_reads_it() {
    let prompt = (ShellState::AtPrompt, AgentLifecycle::Inactive);
    let command = (ShellState::ForegroundProcess, AgentLifecycle::Waiting);
    let ignored = (ShellState::Unknown, AgentLifecycle::Waiting);
    let cmdline = |len: usize| {
        let mut body = b"C;cmdline_url=".to_vec();
        body.resize(len, b'a');
        osc133(&body)
    };
    let rows = [
        (osc133(b"A"), prompt),
        (b"\x1b]133;A;k=i\x1b\\".to_vec(), prompt),
        (osc133(b"A;redraw=0"), prompt),
        (osc133(b"P;k=i"), prompt),
        (osc133(b"N"), prompt),
        (osc133(b"A;k=s"), ignored),
        (osc133(b"A;k=r"), ignored),
        (osc133(b"A;k=c"), ignored),
        (osc133(b"P;k=s"), ignored),
        (osc133(b"P;k=r"), ignored),
        (osc133(b"P;k=c"), ignored),
        (osc133(b"B"), prompt),
        (osc133(b"I"), prompt),
        (osc133(b"C"), command),
        (osc133(b"C;cmdline_url=ls%20-la"), command),
        (cmdline(OSC_BUFFER), command),
        (cmdline(OSC_BUFFER + 1), ignored),
        (osc133(b"D"), prompt),
        (b"\x1b]133;D;0\x1b\\".to_vec(), prompt),
        (osc133(b"D;127"), prompt),
        (osc133(b"Axyz"), ignored),
        (osc133(b"Cxyz"), ignored),
        (osc133(b"D0"), ignored),
        (osc133(b"L"), ignored),
        (osc133(b""), ignored),
        (osc133(b"Z"), ignored),
        (b"\x1b]133\x07".to_vec(), ignored),
    ];
    for (bytes, want) in rows {
        let tab = outcome_whatever_the_reads(true, &bytes).await.tab;
        assert_eq!(
            (tab.shell_state, tab.agent_lifecycle),
            want,
            "{:?} ({} bytes)",
            String::from_utf8_lossy(&bytes[..bytes.len().min(24)]),
            bytes.len()
        );
        assert!(tab.hook_active, "ownership is kept");
    }
}

/// #650's scenarios, on a session tab: a prompt mark only libghostty took
/// (`P;k=i`, `N`) or only roost-osc took (`A;k=s`), in the same read as a
/// record change, under a hook owner still Waiting. One reading of the
/// mark for both layers gives one banner set for every read split.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_third_party_prompt_mark_beside_a_record_banners_the_same_whatever_the_reads() {
    let blocked = osc7501("state=blocked:app=pi");
    for (mark, banners) in [
        (
            osc133(b"P;k=i"),
            vec![("Pi".to_string(), "Needs your input".to_string())],
        ),
        (
            osc133(b"N"),
            vec![("Pi".to_string(), "Needs your input".to_string())],
        ),
        (osc133(b"A;k=s"), Vec::new()),
    ] {
        let mut stream = mark.clone();
        stream.extend_from_slice(&blocked);
        assert_eq!(
            outcome_whatever_the_reads(true, &stream).await.banners,
            banners,
            "{}",
            String::from_utf8_lossy(&mark)
        );
    }
}

/// The OSC terminators: BEL, and ST (`ESC \`), which a read can split
/// between its `ESC` and its `\`.
const TERMINATORS: [&[u8]; 2] = [b"\x07", b"\x1b\\"];

/// `ESC ] body`, ended by `terminator`.
fn osc(body: &str, terminator: &[u8]) -> Vec<u8> {
    [b"\x1b]", body.as_bytes(), terminator].concat()
}

/// Plan 083 B-4, on a session tab: under a hook owner and no record, an
/// `OSC 9` and an `OSC 777` before the mark that ends the agent's turn are
/// suppressed, and the pair after it is not, whether the bytes arrive in
/// one read or split anywhere — between an ST's `ESC` and `\` included.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_notifications_around_a_prompt_mark_are_judged_where_they_fall() {
    for terminator in TERMINATORS {
        for mark in [&b"A"[..], b"B", b"D;0"] {
            let mut stream = osc("9;nine before", terminator);
            stream.extend(osc("777;notify;seven;before", terminator));
            stream.extend_from_slice(&osc133(mark));
            stream.extend(osc("9;nine after", terminator));
            stream.extend(osc("777;notify;seven;after", terminator));
            let outcome = outcome_however_split(true, &stream).await;
            assert_eq!(
                outcome.banners,
                vec![
                    ("nine after".to_string(), String::new()),
                    ("seven".to_string(), "after".to_string()),
                ],
                "{} ended by {terminator:?}",
                String::from_utf8_lossy(mark)
            );
            assert_eq!(outcome.tab.program_status, None, "no record decided");
        }
    }
}

/// The `(title, body)` of each banner a stream raised, in order.
type Banners = Vec<(String, String)>;

/// The astra review's sequences (plan 083 B1, all three rounds), on a session
/// tab, each fed whole, split at every byte, and split at every byte
/// around an empty feed. Every one turns on where a sequence ends:
/// libghostty and roost-osc end it at the same `ESC`, whatever follows, so
/// each notification is judged at its own place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_ended_at_an_esc_are_judged_where_they_fall() {
    let st = b"\x1b\\".as_slice();
    let x = || vec![("x".to_string(), String::new())];
    let working = osc7501("state=working:app=pi");
    let a = b"\x1b]133;A\x07".as_slice();
    // A notification ended by an `ESC [`: a real one, to both parsers.
    let cut_short = b"\x1b]9;x\x1b[m".as_slice();
    let cases: [(bool, Vec<u8>, Banners); 7] = [
        // Round 1 and 2: an ST-ended notification under a driving agent,
        // then the mark that ends its turn.
        (true, [&osc("9;x", st), a].concat(), Vec::new()),
        (true, [&osc("777;notify;t;b", st), a].concat(), Vec::new()),
        // The same under a working record and no agent.
        (false, [&working, &osc("9;x", st), a].concat(), Vec::new()),
        // Round 2: a notification before a working record, then another
        // with its text under it, in a form only roost-osc accepts (a
        // title-only OSC 777, an OSC 9 libghostty's CAN cancels) or one
        // both accept.
        (
            false,
            [cut_short, &working, &osc("777;notify;x", st)].concat(),
            x(),
        ),
        (
            false,
            [cut_short, &working, &osc("9;x\x18", st)].concat(),
            x(),
        ),
        (false, [cut_short, &working, &osc("9;x", st)].concat(), x()),
        // Under a driving agent instead: the first is suppressed, and the
        // one after the mark that ends the turn is not.
        (true, [cut_short, a, &osc("777;notify;x", st)].concat(), x()),
    ];
    // Round 3: a C0 control or DEL between the `ESC` that ends a title and
    // the `]` that opens a notification after it, under a driving agent,
    // then the mark that ends its turn and another.
    let round_3 = [b'\n', 0x00, b'\t', 0x7f].map(|between| {
        let first = [&b"\x1b]0;t\x1b"[..], &[between], b"]9;x\x07"].concat();
        (true, [&first[..], a, b"\x1b]9;x\x07"].concat(), x())
    });
    for (hook_owner, stream, banners) in cases.into_iter().chain(round_3) {
        assert_eq!(
            outcome_however_split(hook_owner, &stream).await.banners,
            banners,
            "{:?}",
            String::from_utf8_lossy(&stream)
        );
    }
}
