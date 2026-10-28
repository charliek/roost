//! The shared callback context: `set_status_sink` (OSC 7501 reports and
//! OSC 133 marks into one ordered sink) alongside `set_write_pty_buffer`,
//! through real bytes fed to `vt_write`. The crafted-struct guards (short
//! and long structs, unknown state) are inline in `src/program_status.rs`.
#![cfg(feature = "ffi")]

use std::sync::{Arc, Mutex};

use roost_vt::{
    ProgramStatusKind, ProgramStatusReport, ProgramStatusState, PromptMark, Terminal,
    TerminalEvent, TerminalOptions,
};

type Sink = Arc<Mutex<Vec<TerminalEvent>>>;
type Replies = Arc<Mutex<Vec<u8>>>;

const PROBE: &[u8] = b"\x1b]7501;?\x1b\\";
const PROBE_REPLY: &[u8] =
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

fn new_terminal() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
        ..Default::default()
    })
    .expect("terminal")
}

fn install_sink(term: &mut Terminal) -> Sink {
    let sink = Arc::new(Mutex::new(Vec::new()));
    term.set_status_sink(Arc::clone(&sink))
        .expect("install status sink");
    sink
}

fn install_replies(term: &mut Terminal) -> Replies {
    let replies = Arc::new(Mutex::new(Vec::new()));
    term.set_write_pty_buffer(Arc::clone(&replies))
        .expect("install write_pty buffer");
    replies
}

fn take<T>(buf: &Arc<Mutex<Vec<T>>>) -> Vec<T> {
    std::mem::take(
        &mut *buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

fn osc7501(body: &str) -> Vec<u8> {
    format!("\x1b]7501;{body}\x1b\\").into_bytes()
}

fn report(state: ProgramStatusState) -> ProgramStatusReport {
    ProgramStatusReport {
        state,
        kind: None,
        progress: None,
        id: String::new(),
        app: String::new(),
        title: String::new(),
        message: String::new(),
    }
}

fn status(state: ProgramStatusState) -> TerminalEvent {
    TerminalEvent::Status(report(state))
}

/// The sink and the reply buffer both work, then the probe and a report
/// arrive where they belong.
fn assert_both_work(term: &mut Terminal, sink: &Sink, replies: &Replies) {
    term.vt_write(PROBE);
    assert_eq!(take(replies), PROBE_REPLY, "probe reply");
    term.vt_write(&osc7501("state=working"));
    assert_eq!(take(sink), vec![status(ProgramStatusState::Working)]);
}

#[test]
fn write_pty_then_status_sink_both_work() {
    let mut term = new_terminal();
    let replies = install_replies(&mut term);
    let sink = install_sink(&mut term);
    assert_both_work(&mut term, &sink, &replies);
}

#[test]
fn status_sink_then_write_pty_both_work() {
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    let replies = install_replies(&mut term);
    assert_both_work(&mut term, &sink, &replies);
}

/// Clearing `write_pty` used to null the shared userdata slot, which
/// would silence every other callback.
#[test]
fn clearing_write_pty_keeps_the_status_sink_working() {
    let mut term = new_terminal();
    let replies = install_replies(&mut term);
    let sink = install_sink(&mut term);

    term.clear_write_pty().expect("clear write_pty");
    term.vt_write(&osc7501("state=done"));
    assert_eq!(take(&sink), vec![status(ProgramStatusState::Done)]);
    term.vt_write(b"\x1b[5n");
    assert!(take(&replies).is_empty(), "write_pty is cleared");
}

#[test]
fn clearing_the_status_sink_keeps_write_pty_working() {
    let mut term = new_terminal();
    let replies = install_replies(&mut term);
    let sink = install_sink(&mut term);

    term.clear_status_sink().expect("clear status sink");
    term.vt_write(b"\x1b[5n");
    assert_eq!(take(&replies), b"\x1b[0n");
    term.vt_write(&osc7501("state=done"));
    term.vt_write(b"\x1b]133;A\x07");
    assert!(take(&sink).is_empty(), "the status sink is cleared");
    assert_eq!(Arc::strong_count(&sink), 1, "the cleared sink was dropped");
}

/// No C getter exists for `OPT_USERDATA`, so this proves it stays set
/// behaviourally: after both callbacks are cleared and the terminal has
/// moved, a re-install still reaches its sink.
#[test]
fn userdata_stays_set_across_clears_and_moves() {
    let mut term = new_terminal();
    let _replies = install_replies(&mut term);
    let _sink = install_sink(&mut term);
    term.clear_write_pty().expect("clear write_pty");
    term.clear_status_sink().expect("clear status sink");

    let mut moved = Box::new(term);
    let replies = install_replies(&mut moved);
    let sink = install_sink(&mut moved);
    assert_both_work(&mut moved, &sink, &replies);
}

#[test]
fn replacing_the_status_sink_routes_to_the_new_one() {
    let mut term = new_terminal();
    let first = install_sink(&mut term);
    let second = install_sink(&mut term);
    assert_eq!(Arc::strong_count(&first), 1, "the old sink was dropped");

    term.vt_write(&osc7501("state=idle"));
    assert!(take(&first).is_empty());
    assert_eq!(take(&second), vec![status(ProgramStatusState::Idle)]);
}

/// Pinned: a poisoned sink still receives the report (the trampoline
/// recovers the guard), and nothing unwinds across the C frame.
#[test]
fn a_poisoned_sink_still_receives_reports() {
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    let poisoner = Arc::clone(&sink);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.lock().expect("lock");
        panic!("poison the sink");
    })
    .join();
    assert!(sink.is_poisoned());

    term.vt_write(&osc7501("state=working"));
    assert_eq!(take(&sink), vec![status(ProgramStatusState::Working)]);
}

#[test]
fn drop_with_both_installed_releases_both() {
    let mut term = new_terminal();
    let replies = install_replies(&mut term);
    let sink = install_sink(&mut term);
    term.vt_write(&osc7501("state=working"));
    term.vt_write(b"\x1b[5n");

    drop(term);
    assert_eq!(Arc::strong_count(&sink), 1);
    assert_eq!(Arc::strong_count(&replies), 1);
    assert_eq!(take(&sink), vec![status(ProgramStatusState::Working)]);
    assert_eq!(take(&replies), b"\x1b[0n");
}

#[test]
fn each_state_is_reported() {
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    for (body, state) in [
        ("idle", ProgramStatusState::Idle),
        ("working", ProgramStatusState::Working),
        ("done", ProgramStatusState::Done),
        ("blocked", ProgramStatusState::Blocked),
        ("error", ProgramStatusState::Error),
        ("clear", ProgramStatusState::Clear),
    ] {
        term.vt_write(&osc7501(&format!("state={body}")));
        assert_eq!(take(&sink), vec![status(state)], "state={body}");
    }
}

#[test]
fn each_kind_is_reported_and_an_unknown_kind_is_none() {
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    for (body, kind) in [
        ("permission", Some(ProgramStatusKind::Permission)),
        ("question", Some(ProgramStatusKind::Question)),
        ("auth", Some(ProgramStatusKind::Auth)),
        ("telepathy", None),
    ] {
        term.vt_write(&osc7501(&format!("state=blocked:kind={body}")));
        let expected = ProgramStatusReport {
            kind,
            ..report(ProgramStatusState::Blocked)
        };
        assert_eq!(
            take(&sink),
            vec![TerminalEvent::Status(expected)],
            "kind={body}"
        );
    }
}

#[test]
fn every_field_is_copied() {
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    // title "Tests" and msg "Apply?" are base64 on the wire.
    term.vt_write(&osc7501(
        "state=working:id=build/test:app=cargo:progress=40:title=VGVzdHM=:msg=QXBwbHk/",
    ));
    assert_eq!(
        take(&sink),
        vec![TerminalEvent::Status(ProgramStatusReport {
            state: ProgramStatusState::Working,
            kind: None,
            progress: Some(40),
            id: "build/test".into(),
            app: "cargo".into(),
            title: "Tests".into(),
            message: "Apply?".into(),
        })]
    );
}

#[test]
fn progress_absent_or_out_of_range_is_none() {
    use ProgramStatusState::{Blocked, Working};
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    for (body, state, progress) in [
        ("state=working", Working, None),
        ("state=working:progress=101", Working, None),
        ("state=blocked:progress=0", Blocked, Some(0)),
        ("state=working:progress=100", Working, Some(100)),
    ] {
        term.vt_write(&osc7501(body));
        let expected = ProgramStatusReport {
            progress,
            ..report(state)
        };
        assert_eq!(take(&sink), vec![TerminalEvent::Status(expected)], "{body}");
    }
}

#[test]
fn ris_reports_clear_with_an_empty_id() {
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    term.vt_write(&osc7501("state=working:id=build"));
    let _ = take(&sink);

    term.vt_write(b"\x1bc");
    assert_eq!(take(&sink), vec![status(ProgramStatusState::Clear)]);
}

/// Without the sink the probe stays silent (`write_pty_test.rs`
/// pins that); with it, the reply is exactly one, byte for byte.
#[test]
fn the_probe_is_answered_once_the_sink_is_set() {
    let mut term = new_terminal();
    let replies = install_replies(&mut term);
    term.vt_write(PROBE);
    assert!(take(&replies).is_empty(), "no sink, no reply");

    let sink = install_sink(&mut term);
    term.vt_write(PROBE);
    assert_eq!(take(&replies), PROBE_REPLY);
    assert!(take(&sink).is_empty(), "a probe is not a report");
}

#[test]
fn osc_133_marks_map_in_byte_order() {
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    term.vt_write(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
    assert_eq!(
        take(&sink),
        vec![
            TerminalEvent::Prompt(PromptMark::PromptStart),
            TerminalEvent::Prompt(PromptMark::InputStart),
            TerminalEvent::Prompt(PromptMark::OutputStart),
            TerminalEvent::Prompt(PromptMark::CommandEnd),
        ]
    );
}

#[test]
fn only_a_primary_prompt_start_is_a_mark() {
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    term.vt_write(b"\x1b]133;P;k=r\x07\x1b]133;P;k=c\x07\x1b]133;A;k=s\x07");
    assert!(
        take(&sink).is_empty(),
        "right/continuation/secondary prompts"
    );

    term.vt_write(b"\x1b]133;P;k=i\x07\x1b]133;N\x07");
    assert_eq!(
        take(&sink),
        vec![
            TerminalEvent::Prompt(PromptMark::PromptStart),
            TerminalEvent::Prompt(PromptMark::PromptStart),
        ]
    );
}

/// The captured Claude turn start: `working, A, working` in one read
/// must come out in that order, not reports first and marks after.
#[test]
fn reports_and_marks_interleave_in_one_sink() {
    let mut term = new_terminal();
    let sink = install_sink(&mut term);
    let mut bytes = osc7501("state=working:app=claude-code");
    bytes.extend_from_slice(b"\x1b]133;A\x07");
    bytes.extend_from_slice(&osc7501("state=working:app=claude-code"));
    bytes.extend_from_slice(b"\x1b]133;C\x07");
    term.vt_write(&bytes);

    let working = TerminalEvent::Status(ProgramStatusReport {
        app: "claude-code".into(),
        ..report(ProgramStatusState::Working)
    });
    assert_eq!(
        take(&sink),
        vec![
            working.clone(),
            TerminalEvent::Prompt(PromptMark::PromptStart),
            working,
            TerminalEvent::Prompt(PromptMark::OutputStart),
        ]
    );
}

/// libghostty's parser is stateful across writes, so splitting the bytes
/// at any offset, or not at all, yields the same events.
#[test]
fn events_do_not_depend_on_write_boundaries() {
    let mut bytes = osc7501("state=blocked:kind=auth:msg=QXBwbHk/");
    bytes.extend_from_slice(b"\x1b]133;A\x07\x1b]133;D\x07");
    bytes.extend_from_slice(&osc7501("state=done"));

    let expected = vec![
        TerminalEvent::Status(ProgramStatusReport {
            kind: Some(ProgramStatusKind::Auth),
            message: "Apply?".into(),
            ..report(ProgramStatusState::Blocked)
        }),
        TerminalEvent::Prompt(PromptMark::PromptStart),
        TerminalEvent::Prompt(PromptMark::CommandEnd),
        status(ProgramStatusState::Done),
    ];

    for split in 0..=bytes.len() {
        let mut term = new_terminal();
        let sink = install_sink(&mut term);
        let (head, tail) = bytes.split_at(split);
        term.vt_write(head);
        term.vt_write(tail);
        assert_eq!(take(&sink), expected, "split at {split}");
    }
}
