//! Contract battery for the snapshot wrapper — `Terminal::snapshot` on
//! the encode side, [`SnapshotDecoder`] on the decode side.
//!
//! Three things here are quiet-but-fatal for host-sessions' attach path.
//! First, the envelope: a snapshot that does not start with the
//! `"GHOSTSNP"` magic and a u16 version is not a snapshot. Second, the
//! encode precondition — libghostty refuses to encode a terminal whose
//! VT parser is mid-sequence *unless* continuation tracking was enabled
//! before the input that left it unfinished. Since the attach payload is
//! taken from a live terminal that may be interrupted anywhere, the
//! tracking knob has to be a construction-time option and it has to read
//! back. Third, the decode side has to reproduce the terminal *exactly*
//! — grid, cursor, colors, modes, scrollback and its limits — because a
//! reattached session that is subtly wrong is worse than one that
//! failed loudly.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use roost_vt::{
    ffi, ActiveScreen, Cell, CellWide, ColorRgb, DecodedTerminal, Error, HistoryStep, ReadyState,
    RenderState, ScrollViewport, SnapshotDecodeOptions, SnapshotDecoder, Style, Terminal,
    TerminalOptions, TerminalSelection, SNAPSHOT_FORMAT_VERSION,
};

/// `snapshot.h:53-118`: 8-byte magic then a u16 LE version.
const MAGIC: &[u8; 8] = b"GHOSTSNP";
const ENVELOPE_LEN: usize = 10;
const RECORD_HEADER_LEN: usize = 10;

// Record tags, from the format's framing table (`snapshot/record.zig`).
const TAG_TERMINAL: u16 = 1;
const TAG_PAGE: u16 = 3;
const TAG_READY: u16 = 5;
const TAG_FINISH: u16 = 6;

fn term(options: TerminalOptions) -> Terminal {
    Terminal::new(options).expect("Terminal::new")
}

fn assert_envelope(bytes: &[u8]) {
    assert!(
        bytes.len() > ENVELOPE_LEN,
        "snapshot must carry records past the {ENVELOPE_LEN}-byte envelope, got {} bytes",
        bytes.len()
    );
    assert_eq!(&bytes[..8], MAGIC, "snapshot magic");
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    assert_eq!(
        version, SNAPSHOT_FORMAT_VERSION,
        "snapshot format version at this pin"
    );
}

// ============================================================================
// Encode (C1)
// ============================================================================

#[test]
fn encodes_an_envelope_and_records() {
    let bytes = term(TerminalOptions::default())
        .snapshot()
        .expect("snapshot of a ground-state terminal");
    assert_envelope(&bytes);
}

#[test]
fn encodes_a_terminal_with_styled_content() {
    let options = TerminalOptions {
        cols: 40,
        rows: 8,
        max_scrollback: 200,
        ..Default::default()
    };
    let mut terminal = term(options);
    // Bold + red "hello", reset, a second line, and a wide codepoint —
    // enough that the active screen is not the empty default.
    terminal.vt_write(b"\x1b[1;31mhello\x1b[0m\r\nworld \xe4\xb8\xad\r\n");

    let bytes = terminal.snapshot().expect("snapshot with content");
    assert_envelope(&bytes);

    let empty = term(options)
        .snapshot()
        .expect("snapshot of an empty terminal");
    assert!(
        bytes.len() > empty.len(),
        "content must widen the stream: {} vs empty {}",
        bytes.len(),
        empty.len()
    );
}

/// Plan test 4b. `\x1b[3` is a CSI with no final byte, so the parser is
/// left mid-sequence.
const UNFINISHED_CSI: &[u8] = b"\x1b[3";

#[test]
fn unfinished_parser_without_tracking_is_rejected() {
    let mut terminal = term(TerminalOptions::default());
    assert_eq!(
        terminal.continuation_max_bytes().expect("read back"),
        0,
        "tracking must default off"
    );
    terminal.vt_write(UNFINISHED_CSI);

    match terminal.snapshot() {
        Err(Error::InvalidValue) => {}
        other => panic!("expected InvalidValue for an untracked unfinished parser, got {other:?}"),
    }
}

#[test]
fn unfinished_parser_with_tracking_encodes() {
    let mut terminal = term(TerminalOptions {
        continuation_max_bytes: 4096,
        ..Default::default()
    });
    terminal.vt_write(UNFINISHED_CSI);

    let bytes = terminal
        .snapshot()
        .expect("tracking enabled before the unfinished input makes encode legal");
    assert_envelope(&bytes);
}

/// D2 read-back. The two iced production constructors pass
/// `max_scrollback: 2000` with tracking explicitly off; this pins that
/// shape reading back 0. (0 is also libghostty's own default, so the
/// constructor *applying* the option is pinned by the nonzero
/// round-trip below, not here.)
#[test]
fn iced_production_shapes_read_back_tracking_off() {
    for (cols, rows) in [(80_u16, 24_u16), (120, 40)] {
        let terminal = term(TerminalOptions {
            cols,
            rows,
            max_scrollback: 2000,
            continuation_max_bytes: 0,
        });
        assert_eq!(
            terminal.continuation_max_bytes().expect("read back"),
            0,
            "{cols}x{rows} production shape must have tracking disabled"
        );
    }
}

#[test]
fn continuation_limit_round_trips() {
    let terminal = term(TerminalOptions {
        continuation_max_bytes: 4096,
        ..Default::default()
    });
    assert_eq!(terminal.continuation_max_bytes().expect("read back"), 4096);
}

#[test]
fn set_continuation_max_bytes_disables_tracking() {
    let mut terminal = term(TerminalOptions {
        continuation_max_bytes: 4096,
        ..Default::default()
    });
    terminal.vt_write(UNFINISHED_CSI);
    terminal
        .snapshot()
        .expect("tracked unfinished parser encodes before the disable");
    terminal
        .set_continuation_max_bytes(0)
        .expect("set_continuation_max_bytes(0)");
    assert_eq!(
        terminal.continuation_max_bytes().expect("read back"),
        0,
        "zeroing the limit must disable tracking — the header's cleanup rule"
    );
    // Disabling while unfinished discards the retained continuation
    // (terminal.h), so encode must now refuse — the functional proof
    // that the disable did more than flip the reported limit.
    match terminal.snapshot() {
        Err(Error::InvalidValue) => {}
        other => {
            panic!("expected InvalidValue after disabling tracking mid-sequence, got {other:?}")
        }
    }
}

// ============================================================================
// Fixture (D4)
// ============================================================================

const COLS: u16 = 80;
const ROWS: u16 = 24;
const SCROLLBACK: usize = 2000;

/// Enough lines to saturate the 2000-row scrollback and produce several
/// HISTORY pages, so per-page progress assertions have something to say.
const MULTI_PAGE_LINES: u32 = 2600;
/// Well under the scrollback cap, so live input written *after* decode
/// cannot legitimately prune rows out from under an equality assertion.
const HEADROOM_LINES: u32 = 1200;

fn fixture_options() -> TerminalOptions {
    TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback: SCROLLBACK,
        continuation_max_bytes: 0,
    }
}

/// The shared deterministic fixture: palette + OSC color overrides,
/// bracketed paste on, `lines` styled history lines, a soft-wrapped
/// line, wide + combining text, and a parked cursor. Everything here is
/// a pure function of `lines` and `alt`, so any two builds compare.
fn fixture(lines: u32, alt: bool) -> Terminal {
    let mut terminal = term(fixture_options());
    // Palette slot + default fg/bg overrides — these ride the TERMINAL
    // record, not the grid, so they catch a different class of loss.
    terminal.vt_write(b"\x1b]4;1;rgb:ff/00/7f\x07");
    terminal.vt_write(b"\x1b]10;rgb:12/34/56\x07");
    terminal.vt_write(b"\x1b]11;rgb:0a/0b/0c\x07");
    // Bracketed paste (DEC mode 2004) on — mode state must survive.
    terminal.vt_write(b"\x1b[?2004h");

    for i in 0..lines {
        let line = format!(
            "\x1b[1;{}mline {i:04} the quick brown fox jumps over the lazy dog\x1b[0m\r\n",
            31 + (i % 7)
        );
        terminal.vt_write(line.as_bytes());
    }

    // A line longer than the grid, so it soft-wraps.
    terminal.vt_write(&vec![b'w'; usize::from(COLS) + 17]);
    terminal.vt_write(b"\r\n");
    // Wide (CJK), combining (e + U+0301), and an emoji.
    terminal.vt_write("\u{4e2d}\u{6587} e\u{0301} \u{1F600}\r\n".as_bytes());
    // Inverse + italic, so the style bits are not all bold.
    terminal.vt_write(b"\x1b[3;7mstyled tail\x1b[0m\r\n");
    // Park the cursor somewhere unambiguous.
    terminal.vt_write(b"\x1b[5;13H");

    if alt {
        terminal.vt_write(b"\x1b[?1049h");
        terminal.vt_write(b"alternate screen content\r\n\x1b[2;7H");
    }
    terminal
}

/// The primary-screen fixture, encoded — for the tests that only need
/// the stream, not the terminal it came from.
fn fixture_bytes(lines: u32) -> Vec<u8> {
    fixture(lines, false).snapshot().expect("snapshot")
}

/// One cell, flattened so two terminals compare with `assert_eq!`.
#[derive(Debug, PartialEq)]
struct CellDump {
    row: u32,
    col: u16,
    text: String,
    fg: Option<ColorRgb>,
    bg: Option<ColorRgb>,
    style: Style,
    wide: CellWide,
}

impl CellDump {
    fn new(row: u32, cell: Cell) -> Self {
        Self {
            row,
            col: cell.col,
            text: cell.text,
            fg: cell.fg,
            bg: cell.bg,
            style: cell.style,
            wide: cell.wide,
        }
    }
}

fn dump(terminal: &Terminal) -> Vec<CellDump> {
    let mut render = RenderState::new().expect("RenderState::new");
    render.update(terminal).expect("render update");
    let mut cells = Vec::new();
    render
        .walk(terminal, |row, cell| cells.push(CellDump::new(row, cell)))
        .expect("render walk");
    cells
}

/// The viewport scrolled to the very top of scrollback, then restored.
/// This is how a restored terminal's *history* content gets compared —
/// the render walk only ever sees the viewport.
fn dump_scrollback_top(terminal: &mut Terminal) -> Vec<CellDump> {
    terminal.scroll_viewport(ScrollViewport::Top);
    let cells = dump(terminal);
    terminal.scroll_viewport(ScrollViewport::Bottom);
    cells
}

/// A copy of a span that lives in scrollback, taken through the
/// selection API (which routes to libghostty's formatter for anything
/// the viewport cannot serve).
fn scrollback_copy(terminal: &mut Terminal) -> Option<String> {
    terminal.scroll_viewport(ScrollViewport::Top);
    let mut render = RenderState::new().expect("RenderState::new");
    let mut selection = TerminalSelection::new();
    selection
        .set(terminal, (0, 0), (COLS - 1, 5))
        .expect("select the top of scrollback");
    let text = selection
        .selected_text(terminal, &mut render, COLS, ROWS)
        .expect("selected_text");
    terminal.scroll_viewport(ScrollViewport::Bottom);
    text
}

fn cursor_repr(terminal: &Terminal) -> String {
    let mut render = RenderState::new().expect("RenderState::new");
    render.update(terminal).expect("render update");
    format!("{:?}", render.cursor())
}

fn colors_repr(terminal: &Terminal) -> String {
    let mut render = RenderState::new().expect("RenderState::new");
    render.update(terminal).expect("render update");
    format!("{:?}", render.colors().expect("render colors"))
}

/// Read a configured scrollback limit off a terminal. `None` mirrors
/// `GHOSTTY_NO_VALUE`, i.e. "unlimited".
fn configured_limit(terminal: &Terminal, key: ffi::GhosttyTerminalData) -> Option<usize> {
    let mut out: usize = usize::MAX;
    // SAFETY: the handle is live for the borrow, and `out` is a real
    // local of the `size_t` type both scrollback data keys document.
    let rc = unsafe {
        ffi::ghostty_terminal_get(terminal.as_ffi(), key, (&mut out) as *mut usize as *mut _)
    };
    match rc {
        ffi::GhosttyResult_GHOSTTY_SUCCESS => Some(out),
        ffi::GhosttyResult_GHOSTTY_NO_VALUE => None,
        other => panic!("unexpected rc {other} reading scrollback limit"),
    }
}

/// One record, located by the same framing the wrapper's scanner walks:
/// `tag u16 | payload_len u32 | crc u32 | payload`.
#[derive(Debug, Clone, Copy)]
struct Record {
    tag: u16,
    start: usize,
    payload_start: usize,
    end: usize,
}

fn records(bytes: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let mut at = ENVELOPE_LEN;
    while at + RECORD_HEADER_LEN <= bytes.len() {
        let tag = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let payload_len =
            u32::from_le_bytes([bytes[at + 2], bytes[at + 3], bytes[at + 4], bytes[at + 5]])
                as usize;
        let payload_start = at + RECORD_HEADER_LEN;
        let end = payload_start + payload_len;
        if end > bytes.len() {
            break;
        }
        out.push(Record {
            tag,
            start: at,
            payload_start,
            end,
        });
        at = end;
        if tag == TAG_FINISH {
            break;
        }
    }
    out
}

fn first_record(bytes: &[u8], tag: u16) -> Record {
    records(bytes)
        .into_iter()
        .find(|record| record.tag == tag)
        .unwrap_or_else(|| panic!("no record with tag {tag} in the stream"))
}

/// The first PAGE record that follows READY — i.e. the first *history*
/// page, as opposed to the active-screen pages before READY.
fn first_history_page(bytes: &[u8]) -> Record {
    let ready_end = first_record(bytes, TAG_READY).end;
    records(bytes)
        .into_iter()
        .find(|record| record.tag == TAG_PAGE && record.start >= ready_end)
        .expect("the fixture must carry at least one history page")
}

fn decoder() -> SnapshotDecoder {
    SnapshotDecoder::new(SnapshotDecodeOptions::default())
}

/// A default decoder holding `bytes` and already past READY.
fn ready_decoder(bytes: &[u8]) -> SnapshotDecoder {
    let mut decode = decoder();
    decode.feed(bytes).expect("feed");
    assert_eq!(
        decode.try_ready().expect("try_ready"),
        ReadyState::Ready,
        "the buffered prefix must reach READY"
    );
    decode
}

/// Consume history to FINISH, returning every page step for per-page
/// assertions. For fully buffered streams only — a `NeedMoreBytes` here
/// means the stream was short.
fn drain_history(decode: &mut SnapshotDecoder) -> Vec<HistoryStep> {
    let mut steps = Vec::new();
    loop {
        match decode.try_next().expect("try_next") {
            HistoryStep::Finished => return steps,
            HistoryStep::NeedMoreBytes => panic!("fully buffered stream asked for more bytes"),
            step => steps.push(step),
        }
    }
}

/// Feed everything, take READY, drain history, finish.
fn decode_streaming(bytes: &[u8]) -> (DecodedTerminal, Vec<HistoryStep>) {
    let mut decode = ready_decoder(bytes);
    let steps = drain_history(&mut decode);
    (decode.finish().expect("finish"), steps)
}

fn history_rows(terminal: &Terminal) -> u64 {
    let bar = terminal.scrollbar().expect("scrollbar");
    bar.total.saturating_sub(bar.len)
}

// ============================================================================
// 0b. The READY boundary a host session streams at full speed
// ============================================================================

/// The exported scanner and this file's independent record walk must
/// agree on where READY ends. They are two implementations of the same
/// framing on purpose: the test's is written from the format doc, the
/// crate's is what a session actually streams by.
#[test]
fn ready_boundary_lands_just_past_the_ready_record() {
    for source in [
        term(TerminalOptions::default()),
        fixture(MULTI_PAGE_LINES, false),
    ] {
        let bytes = source.snapshot().expect("snapshot");
        let boundary = roost_vt::ready_boundary(&bytes).expect("READY is in every snapshot");
        assert_eq!(boundary, first_record(&bytes, TAG_READY).end);
        // Everything the client needs to render is behind it, and there
        // is always something after it (FINISH at minimum).
        assert!(boundary > ENVELOPE_LEN && boundary < bytes.len());

        // The prefix alone must decode to a renderable terminal — that
        // is the whole reason the boundary exists.
        let mut decode = SnapshotDecoder::new(SnapshotDecodeOptions::default());
        decode.feed(&bytes[..boundary]).expect("feed the prefix");
        assert_eq!(decode.try_ready().expect("try_ready"), ReadyState::Ready);
    }
}

/// A buffer that stops mid-record is an error, never a boundary in the
/// middle of a record: a server that trusted a short read would split
/// SNAP frames at a byte offset the client's decoder never asked for.
#[test]
fn ready_boundary_refuses_a_truncated_stream() {
    let bytes = fixture(MULTI_PAGE_LINES, false)
        .snapshot()
        .expect("snapshot");
    let ready_end = first_record(&bytes, TAG_READY).end;
    for cut in [0, ENVELOPE_LEN, ENVELOPE_LEN + 4, ready_end - 1] {
        assert!(
            matches!(
                roost_vt::ready_boundary(&bytes[..cut]),
                Err(Error::InvalidValue)
            ),
            "a stream cut at {cut} must not report a boundary"
        );
    }
}

// ============================================================================
// 1. Ground round-trip (buffered path)
// ============================================================================

#[test]
fn buffered_round_trip_reproduces_the_terminal() {
    let mut source = fixture(MULTI_PAGE_LINES, false);
    let bytes = source.snapshot().expect("snapshot");
    assert_envelope(&bytes);

    let decoded = SnapshotDecoder::decode_bytes(bytes.clone(), SnapshotDecodeOptions::default())
        .expect("decode_bytes");
    let mut restored = decoded.terminal;

    assert_eq!(dump(&restored), dump(&source), "active grid");
    assert_eq!(cursor_repr(&restored), cursor_repr(&source), "cursor");
    assert_eq!(colors_repr(&restored), colors_repr(&source), "colors");
    assert_eq!(
        restored.live_palette().expect("palette"),
        source.live_palette().expect("palette"),
        "OSC 4 palette override"
    );
    assert!(
        restored.mode_get(2004),
        "bracketed paste (DEC 2004) must survive"
    );
    assert_eq!(
        restored.scrollbar().expect("scrollbar"),
        source.scrollbar().expect("scrollbar"),
        "scrollback totals"
    );
    assert_eq!(
        dump_scrollback_top(&mut restored),
        dump_scrollback_top(&mut source),
        "top-of-scrollback grid"
    );
    let copy = scrollback_copy(&mut restored);
    assert_eq!(copy, scrollback_copy(&mut source), "scrollback span copy");
    // Which numbered line sits at the top of scrollback depends on the
    // page-granular prune boundary, which varies with the OS page size
    // (16K macOS vs 4K Linux) — so pin the fixture's per-line marker
    // text, not an absolute line number.
    assert!(
        copy.as_deref()
            .is_some_and(|text| text.contains("the quick brown fox")),
        "the copied span must actually contain restored history, got {copy:?}"
    );
    assert_eq!(
        decoded.source_offset,
        bytes.len(),
        "source offset at FINISH"
    );
    assert_eq!(
        decoded.history_rows_primary,
        history_rows(&source),
        "advisory primary extent"
    );
    assert_eq!(decoded.history_rows_alternate, None, "no alternate screen");
}

/// Scrolled-viewport presentation state is deliberately NOT expected to
/// survive: restore rebuilds the page list and lands the viewport at the
/// bottom. Pinned so a future format change that *does* carry it shows
/// up as a deliberate decision rather than a silent one.
#[test]
fn scrolled_viewport_position_is_not_restored() {
    let mut source = fixture(HEADROOM_LINES, false);
    source.scroll_viewport(ScrollViewport::Top);
    let scrolled = source.scrollbar().expect("scrollbar");
    assert_eq!(scrolled.offset, 0, "the fixture is scrolled to the top");

    let bytes = source.snapshot().expect("snapshot");
    let decoded = SnapshotDecoder::decode_bytes(bytes, SnapshotDecodeOptions::default())
        .expect("decode_bytes");
    let restored = decoded.terminal.scrollbar().expect("scrollbar");
    assert_eq!(
        restored.total, scrolled.total,
        "scrollback size still round-trips"
    );
    assert!(
        restored.is_at_bottom(),
        "restore lands the viewport at the bottom, got {restored:?}"
    );
}

// ============================================================================
// 2. Incremental decode + live interleave
// ============================================================================

#[test]
fn incremental_decode_interleaves_live_input() {
    let mut source = fixture(HEADROOM_LINES, false);
    let bytes = source.snapshot().expect("snapshot");
    let ready_end = first_record(&bytes, TAG_READY).end;

    let mut decode = decoder();
    decode.feed(&bytes[..ready_end]).expect("feed the prefix");
    assert_eq!(
        decode.try_ready().expect("try_ready"),
        ReadyState::Ready,
        "the prefix through READY is enough to render"
    );
    // Renderable and typeable the moment READY lands — the whole point
    // of the progressive path.
    let ready_grid = dump(decode.terminal().expect("terminal at READY"));
    assert!(
        ready_grid.iter().any(|cell| !cell.text.trim().is_empty()),
        "the READY terminal must already have content"
    );
    assert!(
        decode.history_rows_primary().is_some(),
        "advisory extents are cached at READY"
    );

    // No history buffered yet.
    assert_eq!(
        decode.try_next().expect("try_next"),
        HistoryStep::NeedMoreBytes
    );

    const LIVE: &[u8] = b"\r\nlive input between pages\r\n";
    decode.vt_write(LIVE).expect("vt_write between pages");
    decode.feed(&bytes[ready_end..]).expect("feed the history");

    let steps = drain_history(&mut decode);
    assert!(!steps.is_empty(), "the fixture carries history pages");
    for step in &steps {
        assert!(
            matches!(
                step,
                HistoryStep::Page {
                    screen: ActiveScreen::Primary,
                    ..
                }
            ),
            "every page belongs to the primary screen, got {step:?}"
        );
    }

    let decoded = decode.finish().expect("finish");
    let mut restored = decoded.terminal;
    // The comparison terminal takes the same bytes the decoder was fed.
    source.vt_write(LIVE);

    assert_eq!(
        dump(&restored),
        dump(&source),
        "active grid after interleave"
    );
    assert_eq!(
        restored.scrollbar().expect("scrollbar"),
        source.scrollbar().expect("scrollbar"),
        "scrollback totals after interleave"
    );
    assert_eq!(
        dump_scrollback_top(&mut restored),
        dump_scrollback_top(&mut source),
        "history landed under the live input"
    );
    // The advisory extent READY reported sizes exactly the history that
    // ended up on the terminal (the interleaved input lands inside the
    // active area, so it adds no scrollback of its own).
    assert_eq!(
        decoded.history_rows_primary,
        history_rows(&restored),
        "the advisory extent must match the restored total"
    );
    assert_eq!(
        history_rows(&restored),
        history_rows(&source),
        "…and the source's"
    );
}

// ============================================================================
// 3. Alternate screen
// ============================================================================

#[test]
fn alternate_screen_round_trips_with_both_extents() {
    let source = fixture(MULTI_PAGE_LINES, true);
    assert_eq!(source.active_screen(), ActiveScreen::Alternate);
    let bytes = source.snapshot().expect("snapshot");

    let (decoded, steps) = decode_streaming(&bytes);
    assert_eq!(
        decoded.terminal.active_screen(),
        ActiveScreen::Alternate,
        "the active screen must survive"
    );
    assert_eq!(dump(&decoded.terminal), dump(&source), "alternate grid");
    assert!(
        decoded.history_rows_alternate.is_some(),
        "a snapshot declaring an alternate screen reports its extent"
    );
    assert!(
        decoded.history_rows_primary > 0,
        "the primary screen keeps its history behind the alternate"
    );

    // Every page is attributed to a screen, and the per-screen remaining
    // counts run down to zero.
    assert!(!steps.is_empty(), "the fixture carries history pages");
    let mut last_remaining: Option<u32> = None;
    for step in &steps {
        let HistoryStep::Page {
            screen,
            pages_remaining_in_screen,
            ..
        } = *step
        else {
            panic!("only pages are collected");
        };
        assert_eq!(
            screen,
            ActiveScreen::Primary,
            "the alternate screen carries no scrollback"
        );
        if let Some(previous) = last_remaining {
            assert!(
                pages_remaining_in_screen < previous,
                "remaining pages must run down: {previous} -> {pages_remaining_in_screen}"
            );
        }
        last_remaining = Some(pages_remaining_in_screen);
    }
    assert_eq!(
        last_remaining,
        Some(0),
        "the last page reports none remaining"
    );
}

#[test]
fn no_alternate_screen_reports_no_alternate_extent() {
    let bytes = fixture_bytes(HEADROOM_LINES);
    let (decoded, _) = decode_streaming(&bytes);
    assert_eq!(decoded.history_rows_alternate, None);
}

// ============================================================================
// 4. Continuation
// ============================================================================

/// (head, tail, label) triples that each leave the parser or the UTF-8
/// decoder unfinished at the snapshot point.
const SPLITS: &[(&[u8], &[u8], &str)] = &[
    (b"\x1b[3;7", b"mstyled\x1b[0m", "mid-CSI"),
    (b"a\xe4\xb8", b"\xadb", "mid-UTF-8"),
    (b"\x1b]11;rgb:aa/bb/", b"cc\x07", "mid-OSC"),
];

fn tracked_fixture(head: &[u8]) -> Terminal {
    let mut terminal = term(TerminalOptions {
        continuation_max_bytes: 4096,
        ..fixture_options()
    });
    terminal.vt_write(b"\x1b[?2004h");
    terminal.vt_write(b"before the split\r\n");
    terminal.vt_write(head);
    terminal
}

fn retain_options() -> SnapshotDecodeOptions {
    SnapshotDecodeOptions {
        max_continuation_bytes: Some(4096),
        retain_continuation: true,
        ..Default::default()
    }
}

#[test]
fn retained_continuation_replays_the_tail() {
    for (head, tail, label) in SPLITS {
        let mut source = tracked_fixture(head);
        let bytes = source
            .snapshot()
            .unwrap_or_else(|err| panic!("{label}: snapshot with tracking on: {err:?}"));

        let decoded = SnapshotDecoder::decode_bytes(bytes, retain_options())
            .unwrap_or_else(|err| panic!("{label}: decode_bytes: {err:?}"));
        let mut restored = decoded.terminal;

        // Re-encoding the decoded terminal *before* the tail is the
        // proof that the continuation itself came back: encode refuses a
        // non-ground parser unless the bytes that left it unfinished are
        // retained, so a merely "restored-looking" terminal fails here.
        restored
            .snapshot()
            .unwrap_or_else(|err| panic!("{label}: the continuation must survive decode: {err:?}"));

        restored.vt_write(tail);
        source.vt_write(tail);
        assert_eq!(
            dump(&restored),
            dump(&source),
            "{label}: grid after the tail"
        );
        assert_eq!(
            format!("{:?}", restored.live_colors()),
            format!("{:?}", source.live_colors()),
            "{label}: colors after the tail"
        );
    }
}

#[test]
fn zero_continuation_limit_rejects_a_non_ground_snapshot() {
    let source = tracked_fixture(b"\x1b[3;7");
    let bytes = source.snapshot().expect("snapshot");
    let opts = SnapshotDecodeOptions {
        max_continuation_bytes: Some(0),
        ..Default::default()
    };
    match SnapshotDecoder::decode_bytes(bytes, opts) {
        Err(Error::LimitExceeded) => {}
        other => panic!("expected LimitExceeded for a ground-only decoder, got {other:?}"),
    }
}

#[test]
fn continuation_limit_below_the_encoded_size_rejects() {
    // The CONTINUATION record's payload *is* the continuation bytes, so
    // a limit one byte short is exactly the boundary case.
    let head: &[u8] = b"\x1b[3;7";
    let source = tracked_fixture(head);
    let bytes = source.snapshot().expect("snapshot");
    let opts = SnapshotDecodeOptions {
        max_continuation_bytes: Some(head.len() - 1),
        ..Default::default()
    };
    match SnapshotDecoder::decode_bytes(bytes.clone(), opts) {
        Err(Error::LimitExceeded) => {}
        other => panic!("expected LimitExceeded below the encoded continuation, got {other:?}"),
    }
    // Exactly the encoded size is accepted, which pins the rejection to
    // the limit rather than to anything else about the stream.
    let opts = SnapshotDecodeOptions {
        max_continuation_bytes: Some(head.len()),
        retain_continuation: true,
        ..Default::default()
    };
    SnapshotDecoder::decode_bytes(bytes, opts).expect("the exact size is within the limit");
}

#[test]
fn retain_off_leaves_tracking_disabled() {
    let source = tracked_fixture(b"\x1b[3;7");
    let bytes = source.snapshot().expect("snapshot");
    let decoded = SnapshotDecoder::decode_bytes(bytes, SnapshotDecodeOptions::default())
        .expect("decode without retention");
    assert_eq!(
        decoded
            .terminal
            .continuation_max_bytes()
            .expect("read back"),
        0,
        "the default decode must not leave tracking on"
    );
    // …and without the retained continuation the parser state cannot be
    // re-encoded, which is the observable consequence.
    match decoded.terminal.snapshot() {
        Err(Error::InvalidValue) => {}
        other => panic!("expected InvalidValue re-encoding an unretained continuation, {other:?}"),
    }
}

#[test]
fn retain_on_round_trips_the_limit_and_the_cleanup_rule() {
    let source = tracked_fixture(b"\x1b[3;7");
    let bytes = source.snapshot().expect("snapshot");
    let decoded =
        SnapshotDecoder::decode_bytes(bytes, retain_options()).expect("decode with retention");
    let mut restored = decoded.terminal;
    assert_eq!(
        restored.continuation_max_bytes().expect("read back"),
        4096,
        "the decoder's maximum becomes the terminal's tracking limit"
    );
    // Tracking stays enabled after the export a caller would do here.
    restored
        .snapshot()
        .expect("tracking is live on the decoded terminal");
    // The header's cleanup rule, end to end.
    restored
        .set_continuation_max_bytes(0)
        .expect("set_continuation_max_bytes(0)");
    assert_eq!(restored.continuation_max_bytes().expect("read back"), 0);
    match restored.snapshot() {
        Err(Error::InvalidValue) => {}
        other => panic!("expected InvalidValue after the cleanup disable, got {other:?}"),
    }
}

// ============================================================================
// 5. Zero history
// ============================================================================

#[test]
fn zero_history_finishes_on_the_first_step() {
    let source = term(fixture_options());
    let bytes = source.snapshot().expect("snapshot");
    let mut decode = ready_decoder(&bytes);
    assert_eq!(decode.history_rows_primary(), Some(0), "no history");
    assert_eq!(
        decode.try_next().expect("try_next"),
        HistoryStep::Finished,
        "a history-free snapshot finishes immediately"
    );
    let decoded = decode.finish().expect("finish");
    assert_eq!(decoded.history_rows_primary, 0);
    assert_eq!(decoded.source_offset, bytes.len());
}

// ============================================================================
// 6. Fragmentation
// ============================================================================

/// Written to the decoded terminal the moment READY lands, and to the
/// comparison terminal after the decode — test 2's interleave, but with
/// the stream arriving in fragments.
const FRAGMENT_LIVE: &[u8] = b"\r\ntyped while the stream was still arriving\r\n";

/// Deep enough that history arrives as several pages (1200 fits in one),
/// still under the scrollback cap so the live input written at READY
/// cannot prune rows out from under the equality assertion.
const FRAGMENT_LINES: u32 = 1900;

/// What a fragmented decode observed on its way through, so the shape of
/// the progressive path can be asserted rather than assumed.
struct Fragmented {
    decoded: DecodedTerminal,
    /// How many bytes had been fed when READY landed.
    ready_after_bytes: usize,
    pages: usize,
    /// `try_next` calls that reported the next record was not buffered
    /// yet — the state a real transport pump spends most of its time in.
    need_more_bytes: usize,
}

/// Feed `bytes` in the chunks `cuts` describes (each entry is a chunk's
/// end offset), driving `try_ready`/`try_next` opportunistically after
/// every feed — exactly the shape HS-1's pump has.
///
/// Every `try_*` is unwrapped on purpose: the wrapper only calls into
/// libghostty once the record it will read is fully buffered, so a
/// spurious EOF from a decoder that read further ahead than the scanner
/// guaranteed would surface here as a hard error (architecture risk in
/// plan 034 §8). This driver is that risk's probe.
fn decode_in_chunks(bytes: &[u8], cuts: &[usize]) -> Fragmented {
    let mut decode = decoder();
    let mut fed = 0usize;
    let mut ready_after_bytes = None;
    let mut pages = 0usize;
    let mut need_more_bytes = 0usize;
    let mut finished = false;

    for &cut in cuts {
        if finished {
            break;
        }
        assert!(
            cut >= fed && cut <= bytes.len(),
            "cut {cut} out of order or past the stream"
        );
        decode.feed(&bytes[fed..cut]).expect("feed a chunk");
        fed = cut;
        if ready_after_bytes.is_none() {
            if decode.try_ready().expect("try_ready") == ReadyState::Ready {
                ready_after_bytes = Some(fed);
                decode.vt_write(FRAGMENT_LIVE).expect("vt_write at READY");
            }
            continue;
        }
        match decode.try_next().expect("try_next") {
            HistoryStep::NeedMoreBytes => need_more_bytes += 1,
            HistoryStep::Page { .. } => pages += 1,
            HistoryStep::Finished => finished = true,
        }
    }

    assert!(
        finished || fed == bytes.len(),
        "the cut list must cover the whole stream"
    );
    while !finished {
        match decode.try_next().expect("try_next after the last chunk") {
            HistoryStep::NeedMoreBytes => panic!("the whole stream is fed; nothing can be missing"),
            HistoryStep::Page { .. } => pages += 1,
            HistoryStep::Finished => finished = true,
        }
    }

    Fragmented {
        decoded: decode.finish().expect("finish"),
        ready_after_bytes: ready_after_bytes.expect("READY must land"),
        pages,
        need_more_bytes,
    }
}

/// Compare a fragmented decode against a source terminal fed the same
/// live bytes — test 2's equality, reused verbatim.
fn assert_matches_source(fragmented: &mut Fragmented, source: &mut Terminal, encoded_len: usize) {
    source.vt_write(FRAGMENT_LIVE);
    assert_eq!(
        dump(&fragmented.decoded.terminal),
        dump(source),
        "active grid after a fragmented decode"
    );
    assert_eq!(
        fragmented.decoded.terminal.scrollbar().expect("scrollbar"),
        source.scrollbar().expect("scrollbar"),
        "scrollback totals after a fragmented decode"
    );
    assert_eq!(
        dump_scrollback_top(&mut fragmented.decoded.terminal),
        dump_scrollback_top(source),
        "history landed under the live input"
    );
    assert_eq!(
        fragmented.decoded.source_offset, encoded_len,
        "source offset at FINISH"
    );
}

/// One byte at a time: every split point in the stream is exercised at
/// once, so the envelope and every record header are crossed mid-field.
#[test]
fn a_one_byte_at_a_time_stream_decodes_identically() {
    let mut source = fixture(FRAGMENT_LINES, false);
    let bytes = source.snapshot().expect("snapshot");

    let cuts: Vec<usize> = (1..=bytes.len()).collect();
    let mut fragmented = decode_in_chunks(&bytes, &cuts);

    assert!(
        fragmented.ready_after_bytes < bytes.len(),
        "READY must land mid-stream, not once every byte is in: {} of {}",
        fragmented.ready_after_bytes,
        bytes.len()
    );
    assert_eq!(
        fragmented.ready_after_bytes,
        first_record(&bytes, TAG_READY).end,
        "READY must land on the very byte that completes the READY record — \
         not a byte early, and not once history has arrived"
    );
    assert!(
        fragmented.pages > 1,
        "the fixture must arrive as several history pages, got {}",
        fragmented.pages
    );
    assert!(
        fragmented.need_more_bytes > 0,
        "a byte-drip must spend most calls waiting for the next record"
    );

    assert_matches_source(&mut fragmented, &mut source, bytes.len());
}

/// The coarse counterpart: cuts landing *exactly* on the envelope
/// boundary and on each field boundary of the first history page's
/// header. Boundary-exact feeds are the case a byte-drip cannot single
/// out — a scanner that mistook "header buffered" for "record buffered"
/// would fail precisely here.
#[test]
fn boundary_exact_chunks_decode_identically() {
    let mut source = fixture(FRAGMENT_LINES, false);
    let bytes = source.snapshot().expect("snapshot");
    let page = first_history_page(&bytes);

    let mut cuts = vec![
        ENVELOPE_LEN,
        page.start,
        page.start + 2,     // after the tag
        page.start + 6,     // after payload_len
        page.payload_start, // after the crc: the whole header, no payload
        page.payload_start + 1,
        page.end,
        bytes.len(),
    ];
    cuts.sort_unstable();
    cuts.dedup();

    let mut fragmented = decode_in_chunks(&bytes, &cuts);
    assert_eq!(
        fragmented.ready_after_bytes, page.start,
        "READY lands on the chunk that completes the prefix before the first history page"
    );
    assert!(
        fragmented.need_more_bytes > 0,
        "the chunks that carry only part of a record header must report NeedMoreBytes"
    );
    assert_matches_source(&mut fragmented, &mut source, bytes.len());
}

// ============================================================================
// 7. Corruption / truncation
// ============================================================================

#[test]
fn a_truncated_prefix_never_reaches_ready() {
    let bytes = fixture_bytes(HEADROOM_LINES);
    let cut = first_record(&bytes, TAG_READY).start - 32;

    // Streaming surface: the wrapper never calls the C decoder without a
    // complete prefix, so truncation is "not yet", not an error. The
    // consumer notices that READY never lands.
    let mut decode = decoder();
    decode
        .feed(&bytes[..cut])
        .expect("feed the truncated prefix");
    assert_eq!(
        decode.try_ready().expect("try_ready"),
        ReadyState::NeedMoreBytes
    );
    assert_eq!(
        decode.try_ready().expect("try_ready again"),
        ReadyState::NeedMoreBytes,
        "still not an error, and still not a wasted decoder_ready call"
    );

    // Buffered surface: everything the caller has is everything there
    // is, so the same truncation is a hard error.
    match SnapshotDecoder::decode_bytes(bytes[..cut].to_vec(), SnapshotDecodeOptions::default()) {
        Err(Error::InvalidValue) => {}
        other => panic!("expected InvalidValue for a truncated buffered decode, got {other:?}"),
    }
}

#[test]
fn a_corrupt_pre_ready_payload_fails_at_ready_and_poisons() {
    let mut bytes = fixture_bytes(HEADROOM_LINES);
    let terminal_record = first_record(&bytes, TAG_TERMINAL);
    bytes[terminal_record.payload_start] ^= 0xff;

    let mut decode = decoder();
    decode.feed(&bytes).expect("feed");
    let err = decode.try_ready().expect_err("a corrupt record must fail");
    assert!(
        matches!(err, Error::InvalidValue),
        "expected InvalidValue, got {err:?}"
    );
    // Poisoned: nothing but free/abandon is legal from here, and no
    // decoder getter is ever called again.
    assert!(matches!(decode.try_ready(), Err(Error::Lifecycle(_))));
    assert!(matches!(decode.try_next(), Err(Error::Lifecycle(_))));
    assert!(
        decode.abandon().is_none(),
        "no terminal exists before READY"
    );
}

#[test]
fn a_corrupt_history_page_errors_at_that_page_and_leaves_the_terminal() {
    let mut bytes = fixture_bytes(MULTI_PAGE_LINES);
    let page = first_history_page(&bytes);
    bytes[page.payload_start + 7] ^= 0xff;

    let mut decode = ready_decoder(&bytes);

    let mut err = None;
    for _ in 0..8 {
        match decode.try_next() {
            Ok(HistoryStep::Page { .. }) => {}
            Ok(other) => panic!("expected an error before {other:?}"),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    let err = err.expect("a corrupt history page must fail");
    assert!(
        matches!(err, Error::InvalidValue),
        "expected InvalidValue, got {err:?}"
    );
    assert!(
        matches!(decode.try_next(), Err(Error::Lifecycle(_))),
        "poisoned"
    );

    // The READY terminal survives the poison with the history restored
    // so far, exactly as the header promises.
    let mut partial = decode.abandon().expect("abandon returns the terminal");
    partial.vt_write(b"still usable\r\n");
    assert!(dump(&partial).iter().any(|cell| cell.text == "u"));
}

#[test]
fn a_cut_finish_record_never_finishes() {
    let bytes = fixture_bytes(HEADROOM_LINES);
    let finish = first_record(&bytes, TAG_FINISH);
    let cut = finish.start + 4;

    let mut decode = ready_decoder(&bytes[..cut]);
    loop {
        match decode.try_next().expect("try_next") {
            HistoryStep::Page { .. } => {}
            HistoryStep::NeedMoreBytes => break,
            HistoryStep::Finished => panic!("FINISH was cut off; it cannot validate"),
        }
    }
    // Without FINISH there is no source offset to hand out, so `finish`
    // is refused by the state machine rather than reporting a position
    // the decoder cannot vouch for.
    assert!(matches!(decode.finish(), Err(Error::Lifecycle(_))));
}

#[test]
fn a_bumped_envelope_version_is_rejected() {
    let mut bytes = fixture_bytes(HEADROOM_LINES);
    bytes[8] = 2;
    // The wrapper's scanner deliberately does not check the version, so
    // this pins libghostty's own check.
    match SnapshotDecoder::decode_bytes(bytes, SnapshotDecodeOptions::default()) {
        Err(Error::InvalidValue) => {}
        other => panic!("expected InvalidValue for an unknown version, got {other:?}"),
    }
}

// ============================================================================
// 8. Trailer
// ============================================================================

#[test]
fn trailing_transport_bytes_are_not_consumed() {
    let source = fixture(HEADROOM_LINES, false);
    let bytes = source.snapshot().expect("snapshot");
    let encoded_len = bytes.len();
    let mut with_junk = bytes;
    with_junk.extend_from_slice(b"{\"next\":\"frame\"}\x00\xff not snapshot bytes");

    let (decoded, _) = decode_streaming(&with_junk);
    assert_eq!(
        decoded.source_offset, encoded_len,
        "source offset must locate the first byte after FINISH"
    );
    assert_eq!(
        dump(&decoded.terminal),
        dump(&source),
        "grid despite trailer"
    );
}

// ============================================================================
// 9. Resize during decode
// ============================================================================

#[test]
fn a_resize_between_pages_still_reaches_finish() {
    let bytes = fixture_bytes(MULTI_PAGE_LINES);

    let mut decode = ready_decoder(&bytes);
    let first = decode.try_next().expect("first page");
    assert!(matches!(first, HistoryStep::Page { .. }));
    decode.resize(100, 30, 8, 16).expect("resize between pages");

    // Rows are recorded, not asserted: how much history survives a
    // mid-decode resize is a libghostty implementation detail. What this
    // pins is that every remaining page is still consumed and validated
    // and the stream still reaches FINISH.
    let mut rows_after_resize = 0usize;
    let mut pages_after_resize = 0usize;
    loop {
        match decode.try_next().expect("try_next") {
            HistoryStep::Finished => break,
            HistoryStep::NeedMoreBytes => panic!("the whole stream is buffered"),
            HistoryStep::Page { rows_prepended, .. } => {
                pages_after_resize += 1;
                rows_after_resize += rows_prepended;
            }
        }
    }
    eprintln!(
        "[resize-during-decode] {pages_after_resize} pages after resize, \
         {rows_after_resize} rows applied"
    );
    let decoded = decode.finish().expect("finish");
    assert_eq!(decoded.source_offset, bytes.len());
}

// ============================================================================
// 10. Caps
// ============================================================================

/// Overwrite a record's declared `payload_len` in place. Mutating a real
/// encode is the only honest way to build an oversized-record stream —
/// a hand-rolled one would prove the test's framing, not the wrapper's.
fn set_payload_len(bytes: &mut [u8], record: Record, len: u32) {
    bytes[record.start + 2..record.start + 6].copy_from_slice(&len.to_le_bytes());
}

fn largest_record_payload(bytes: &[u8]) -> usize {
    records(bytes)
        .iter()
        .map(|record| record.end - record.payload_start)
        .max()
        .expect("the stream carries records")
}

/// A header declaring more than `max_record_bytes` is refused as soon as
/// the *header* is buffered — the payload is never waited for, so the
/// cap actually bounds what a hostile stream can make roost hold.
#[test]
fn an_oversized_record_header_is_refused_before_any_ffi() {
    let mut bytes = fixture_bytes(HEADROOM_LINES);
    let page = first_history_page(&bytes);
    let opts = SnapshotDecodeOptions::default();
    let over = u32::try_from(opts.max_record_bytes + 1).expect("the cap is well under u32::MAX");
    set_payload_len(&mut bytes, page, over);

    let mut decode = SnapshotDecoder::new(opts);
    match decode.feed(&bytes) {
        Err(Error::LimitExceeded) => {}
        other => panic!("expected LimitExceeded for an oversized record, got {other:?}"),
    }
    // Sticky, not poisoning: the offending header is still in the buffer,
    // so every call that rescans reports the same cap error — never
    // `Lifecycle`, which is what a poisoned decoder answers. No decoder
    // was ever constructed, so libghostty never saw a byte.
    assert!(
        matches!(decode.try_ready(), Err(Error::LimitExceeded)),
        "the cap error must repeat rather than poison"
    );
    assert!(matches!(
        decode.feed(b"more bytes"),
        Err(Error::LimitExceeded)
    ));
    assert!(matches!(decode.try_ready(), Err(Error::LimitExceeded)));
    assert!(
        decode.terminal().is_none(),
        "the decode never started, so there is no terminal"
    );
    assert!(
        decode.history_rows_primary().is_none(),
        "…and no READY-window data either"
    );
}

/// The same gate on an untouched stream: a cap one byte below the
/// largest genuine record rejects, and the exact size is accepted. Pins
/// the rejection to the cap rather than to anything about the mutation
/// above.
#[test]
fn the_record_cap_boundary_is_the_largest_genuine_record() {
    let bytes = fixture_bytes(HEADROOM_LINES);
    let largest = largest_record_payload(&bytes);

    let opts = SnapshotDecodeOptions {
        max_record_bytes: largest - 1,
        ..Default::default()
    };
    match SnapshotDecoder::decode_bytes(bytes.clone(), opts) {
        Err(Error::LimitExceeded) => {}
        other => panic!("expected LimitExceeded one byte below the largest record, got {other:?}"),
    }

    let opts = SnapshotDecodeOptions {
        max_record_bytes: largest,
        ..Default::default()
    };
    SnapshotDecoder::decode_bytes(bytes, opts).expect("the exact size is within the cap");
}

/// The total cap is checked *before* the append, so a rejected feed
/// leaves the buffer exactly as it was.
#[test]
fn the_total_cap_rejects_before_the_buffer_grows() {
    let bytes = fixture_bytes(HEADROOM_LINES);

    let opts = SnapshotDecodeOptions {
        max_total_bytes: bytes.len() - 1,
        ..Default::default()
    };
    let mut decode = SnapshotDecoder::new(opts);
    match decode.feed(&bytes) {
        Err(Error::LimitExceeded) => {}
        other => panic!("expected LimitExceeded over the total cap, got {other:?}"),
    }
    // A whole snapshot was refused, so nothing of it landed: had the
    // append run first, the prefix through READY would be buffered here.
    assert_eq!(
        decode.try_ready().expect("try_ready"),
        ReadyState::NeedMoreBytes,
        "the refused feed must leave the decoder empty"
    );

    // Split feed: the cap counts cumulatively, and refusing the second
    // chunk does not disturb the first.
    let ready_end = first_record(&bytes, TAG_READY).end;
    let opts = SnapshotDecodeOptions {
        max_total_bytes: ready_end,
        ..Default::default()
    };
    let mut decode = SnapshotDecoder::new(opts);
    decode
        .feed(&bytes[..ready_end])
        .expect("the prefix fits the cap exactly");
    assert_eq!(decode.try_ready().expect("try_ready"), ReadyState::Ready);
    match decode.feed(&bytes[ready_end..]) {
        Err(Error::LimitExceeded) => {}
        other => panic!("expected LimitExceeded for the history chunk, got {other:?}"),
    }
    assert_eq!(
        decode.try_next().expect("try_next"),
        HistoryStep::NeedMoreBytes,
        "the refused history bytes never entered the buffer, and the decoder is not poisoned"
    );
    assert!(matches!(
        decode.feed(&bytes[ready_end..]),
        Err(Error::LimitExceeded)
    ));

    // The buffered convenience path reports the same cap error.
    let opts = SnapshotDecodeOptions {
        max_total_bytes: bytes.len() - 1,
        ..Default::default()
    };
    match SnapshotDecoder::decode_bytes(bytes, opts) {
        Err(Error::LimitExceeded) => {}
        other => panic!("expected LimitExceeded from decode_bytes, got {other:?}"),
    }
}

// ============================================================================
// 11. Misuse / lifecycle
// ============================================================================

#[test]
fn misuse_is_a_lifecycle_error_and_never_poisons() {
    let bytes = fixture_bytes(HEADROOM_LINES);

    // `finish` before READY.
    let mut decode = decoder();
    decode.feed(&bytes).expect("feed");
    assert!(matches!(decode.finish(), Err(Error::Lifecycle(_))));

    // `try_next` before READY, then a second `try_ready`, then `finish`
    // from the ready state — none of which disturb the decode.
    let mut decode = decoder();
    decode.feed(&bytes).expect("feed");
    assert!(matches!(decode.try_next(), Err(Error::Lifecycle(_))));
    assert_eq!(decode.try_ready().expect("try_ready"), ReadyState::Ready);
    assert!(matches!(decode.try_ready(), Err(Error::Lifecycle(_))));

    drain_history(&mut decode);
    // FINISH is idempotent, and feeding after it is refused.
    assert_eq!(decode.try_next().expect("try_next"), HistoryStep::Finished);
    assert_eq!(decode.try_next().expect("try_next"), HistoryStep::Finished);
    assert!(matches!(decode.feed(b"more"), Err(Error::Lifecycle(_))));
    // The misuse above cost nothing: the decode still completes.
    let decoded = decode.finish().expect("finish after the refused calls");
    assert_eq!(decoded.source_offset, bytes.len());
}

#[test]
fn terminal_forwarders_refuse_before_ready() {
    let mut decode = decoder();
    assert!(decode.terminal().is_none());
    assert!(matches!(decode.vt_write(b"x"), Err(Error::Lifecycle(_))));
    assert!(matches!(
        decode.resize(80, 24, 8, 16),
        Err(Error::Lifecycle(_))
    ));
}

// ============================================================================
// 12. Drop stress (not a leak proof — a crash proof)
// ============================================================================

#[test]
fn dropping_at_every_state_is_safe() {
    let mut bytes = fixture_bytes(MULTI_PAGE_LINES);

    // Fresh, never fed.
    drop(decoder());

    // Fed but never started — no C decoder exists yet.
    let mut decode = decoder();
    decode.feed(&bytes).expect("feed");
    drop(decode);

    // Ready, with the C decoder borrowing the terminal.
    drop(ready_decoder(&bytes));

    // Mid-history.
    let mut decode = ready_decoder(&bytes);
    decode.try_next().expect("one page");
    drop(decode);

    // Abandoned mid-history: the terminal outlives the decoder.
    let mut decode = ready_decoder(&bytes);
    decode.try_next().expect("one page");
    let mut partial = decode.abandon().expect("terminal");
    partial.vt_write(b"after abandon\r\n");
    drop(partial);

    // Finished, then dropped after `finish` moved the terminal out.
    let (decoded, _) = decode_streaming(&bytes);
    drop(decoded);

    // Poisoned. Bounded so a corruption that stopped erroring fails
    // the test instead of spinning on the idempotent `Finished`.
    let page = first_history_page(&bytes);
    bytes[page.payload_start + 7] ^= 0xff;
    let mut decode = ready_decoder(&bytes);
    let poisoned = (0..16).any(|_| decode.try_next().is_err());
    assert!(poisoned, "the flipped payload byte must poison the decode");
    drop(decode);
}

// ============================================================================
// 13. D3 — scrollback limits round-trip, history restores fully
// ============================================================================

#[test]
fn scrollback_limits_round_trip_from_the_source() {
    let bytes = fixture_bytes(HEADROOM_LINES);
    let (decoded, _) = decode_streaming(&bytes);

    // The format serializes the *source's* policy; the wrapper does not
    // editorialize. A roost-shaped terminal is lines = 2000 with the
    // default byte cap cleared.
    assert_eq!(
        configured_limit(
            &decoded.terminal,
            ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_LINES
        ),
        Some(SCROLLBACK),
        "scrollback line limit must come back from the stream"
    );
    assert_eq!(
        configured_limit(
            &decoded.terminal,
            ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBACK_MAX_BYTES
        ),
        None,
        "the cleared byte limit must stay cleared"
    );
}

#[test]
fn history_far_past_ten_kilobytes_restores_fully() {
    let mut source = fixture(MULTI_PAGE_LINES, false);
    let bytes = source.snapshot().expect("snapshot");

    // Guard the premise: this is a big-history fixture, not a toy.
    let source_history = history_rows(&source);
    assert!(
        source_history > 1000,
        "fixture must carry a deep history, got {source_history} rows"
    );
    let history_bytes = usize::try_from(source_history).expect("fits") * usize::from(COLS);
    assert!(
        history_bytes > 10 * 1024,
        "fixture history must exceed 10 KB, got ~{history_bytes} bytes"
    );

    let (decoded, steps) = decode_streaming(&bytes);
    assert!(
        steps.len() > 1,
        "a deep history must arrive as several pages, got {}",
        steps.len()
    );
    let mut restored = decoded.terminal;
    assert_eq!(
        history_rows(&restored),
        source_history,
        "every history row must come back — no hidden byte cap"
    );
    assert_eq!(
        dump_scrollback_top(&mut restored),
        dump_scrollback_top(&mut source),
        "the oldest restored rows must match the source"
    );
}

// ============================================================================
// Perf capture (recorded, never asserted)
// ============================================================================

fn ms(elapsed: std::time::Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

/// HS-1's §12 budget inputs for the 2000-row 80-column fixture. Ignored
/// by default because it is a measurement, not a check — timings vary by
/// machine and asserting on them would make the gate flaky.
///
/// Run it explicitly, in release:
/// `cargo test -p roost-vt --features ffi --release -- --ignored perf --nocapture`
#[test]
#[ignore = "perf capture: run explicitly with --ignored perf --nocapture"]
fn perf_snapshot_encode_and_decode() {
    let source = fixture(MULTI_PAGE_LINES, false);

    let started = std::time::Instant::now();
    let bytes = source.snapshot().expect("snapshot");
    let encode = started.elapsed();

    // Time to READY starts with the READY prefix already in hand — the
    // HS-1 question is "prefix received -> renderable terminal", so the
    // span covers only feeding that prefix and try_ready, not buffering
    // or scanning the post-READY history.
    let ready_end = first_record(&bytes, TAG_READY).end;
    let mut decode = SnapshotDecoder::new(SnapshotDecodeOptions::default());
    let started = std::time::Instant::now();
    decode.feed(&bytes[..ready_end]).expect("feed the prefix");
    assert_eq!(decode.try_ready().expect("try_ready"), ReadyState::Ready);
    let time_to_ready = started.elapsed();
    drop(decode);

    let full_input = bytes.clone();
    let started = std::time::Instant::now();
    let decoded = SnapshotDecoder::decode_bytes(full_input, SnapshotDecodeOptions::default())
        .expect("decode_bytes");
    let full_decode = started.elapsed();
    drop(decoded);

    println!("encode_bytes={}", bytes.len());
    println!("encode_ms={:.3}", ms(encode));
    println!("time_to_ready_ms={:.3}", ms(time_to_ready));
    println!("full_decode_ms={:.3}", ms(full_decode));
}
