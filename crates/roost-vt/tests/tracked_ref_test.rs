#![cfg(feature = "ffi")]

//! Unit battery for [`roost_vt::TrackedRef`] (issue #334).
//!
//! Selection behavior lives in `selection_test.rs`; this file pins the
//! wrapper's own contract — what it rejects, what it survives, and what
//! it reports once the tracked content is gone.

use roost_vt::{
    ActiveScreen, Error, Point, PointTag, RenderState, Terminal, TerminalOptions, TerminalSelection,
};

const COLS: u16 = 20;
const ROWS: u16 = 5;

fn terminal() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback: 2000,
        ..Default::default()
    })
    .expect("terminal")
}

// ============================================================================
// Rejected inputs
// ============================================================================

#[test]
fn coordinates_outside_the_grid_are_rejected() {
    let mut terminal = terminal();
    terminal.vt_write(b"hello");
    for point in [
        Point::viewport(0, u32::from(u16::MAX)),
        Point::viewport(0, u32::from(ROWS)),
        Point::viewport(COLS, 0),
        Point::viewport(u16::MAX, 0),
    ] {
        assert!(
            matches!(terminal.track(point), Err(Error::InvalidValue)),
            "expected {point:?} to be rejected"
        );
    }
    // The last in-grid cell is accepted, so the rejections above are
    // about the coordinates and not about tracking being broken.
    assert!(terminal
        .track(Point::viewport(COLS - 1, u32::from(ROWS - 1)))
        .is_ok());
}

#[test]
fn a_ref_is_bound_to_the_terminal_that_created_it() {
    let mut owner = terminal();
    owner.vt_write(b"hello");
    let other = terminal();
    let mut tracked = owner.track(Point::viewport(0, 0)).expect("track");

    assert!(tracked.is_owned_by(&owner));
    assert!(!tracked.is_owned_by(&other));
    // libghostty does not check this pairing itself, so the wrapper has
    // to: `_set` against the wrong terminal is undefined behavior.
    assert!(matches!(
        tracked.set(&other, Point::viewport(1, 0)),
        Err(Error::InvalidValue)
    ));
    // The rejected call left the ref where it was.
    assert_eq!(tracked.point(PointTag::Viewport).map(|p| p.x), Some(0));
    assert!(tracked.set(&owner, Point::viewport(1, 0)).is_ok());
    assert_eq!(tracked.point(PointTag::Viewport).map(|p| p.x), Some(1));
}

// ============================================================================
// Losing the tracked cell
// ============================================================================

#[test]
fn a_reset_discards_the_tracked_cell() {
    let mut terminal = terminal();
    terminal.vt_write(b"hello");
    let tracked = terminal.track(Point::viewport(2, 0)).expect("track");
    assert!(tracked.has_value());

    terminal.reset();

    assert!(!tracked.has_value());
    assert_eq!(tracked.point(PointTag::Screen), None);
    // A reset terminal is still usable: `set` re-points the ref and
    // clears the no-value state.
    let mut tracked = tracked;
    terminal.vt_write(b"world");
    tracked
        .set(&terminal, Point::viewport(3, 0))
        .expect("re-point");
    assert!(tracked.has_value());
    assert_eq!(tracked.point(PointTag::Viewport).map(|p| p.x), Some(3));
}

#[test]
fn a_ref_outlives_its_terminal_and_reports_no_value() {
    let mut terminal = terminal();
    terminal.vt_write(b"hello");
    let tracked = terminal.track(Point::viewport(0, 0)).expect("track");

    drop(terminal);

    // libghostty documents this explicitly: the handle stays valid for
    // the tracked-ref APIs, reports no value, and can still be freed —
    // which is what makes `Drop` safe without any ordering rule.
    assert!(!tracked.has_value());
    assert_eq!(tracked.point(PointTag::Screen), None);
    drop(tracked);
}

// ============================================================================
// Following content
// ============================================================================

#[test]
fn a_ref_follows_its_row_into_scrollback() {
    let mut terminal = terminal();
    terminal.vt_write(b"hello\r\n");
    let tracked = terminal.track(Point::viewport(1, 0)).expect("track");
    assert_eq!(
        tracked.point(PointTag::Viewport),
        Some(Point::viewport(1, 0))
    );

    for i in 0..20 {
        terminal.vt_write(format!("line{i}\r\n").as_bytes());
    }

    // Off screen: no viewport representation, but the cell is still
    // there and its screen coordinate is unchanged (nothing has been
    // pruned at this depth).
    assert_eq!(tracked.point(PointTag::Viewport), None);
    assert_eq!(tracked.point(PointTag::Screen), Some(Point::screen(1, 0)));
    assert!(tracked.has_value());
}

/// Reflow is why the column has to come from the ref too: a resize moves
/// content on both axes, and a stored column would silently point at a
/// different character.
#[test]
fn a_reflow_moves_the_ref_on_both_axes() {
    let mut terminal = terminal();
    // 30 narrow cells at 20 columns: rows 0 and 1 are one logical line.
    terminal.vt_write(b"abcdefghijklmnopqrstuvwxyz0123");
    let tracked = terminal.track(Point::viewport(9, 1)).expect("track");
    assert_eq!(
        tracked.point(PointTag::Viewport),
        Some(Point::viewport(9, 1))
    );

    terminal.resize(40, ROWS, 8, 16).expect("resize");

    // The whole line now fits on one row, so the tracked cell moved up a
    // row and 20 columns right — same character, new coordinates.
    assert_eq!(
        tracked.point(PointTag::Viewport),
        Some(Point::viewport(29, 0))
    );
}

/// The selection-level consequence of the above, including a reversed
/// (anchor-after-cursor) drag: the copy is the same before and after a
/// reflow that moves both endpoints.
#[test]
fn a_reversed_selection_survives_a_reflow() {
    let mut terminal = terminal();
    let mut render_state = RenderState::new().expect("render state");
    terminal.vt_write(b"abcdefghijklmnopqrstuvwxyz0123");
    let mut selection = TerminalSelection::new();
    // Anchor on the wrapped continuation row, cursor above it.
    assert!(selection
        .set(&terminal, (9, 1), (0, 0))
        .expect("set selection"));
    let before = selection
        .selected_text(&terminal, &mut render_state, COLS, ROWS)
        .expect("selected text");
    assert_eq!(before.as_deref(), Some("abcdefghijklmnopqrstuvwxyz0123"));

    terminal.resize(40, ROWS, 8, 16).expect("resize");

    let after = selection
        .selected_text(&terminal, &mut render_state, 40, ROWS)
        .expect("selected text");
    assert_eq!(after, before, "the reflowed selection copied differently");
}

// ============================================================================
// Screens
// ============================================================================

/// A tracked ref keeps resolving against the screen it was created on,
/// even while the other screen is displayed — which is exactly why the
/// selection records the screen and hides itself until it is active
/// again. Without that gate the primary-screen selection would paint
/// over, and copy out of, an alt-screen app.
#[test]
fn a_selection_hides_while_its_screen_is_inactive_and_returns_after() {
    let mut terminal = terminal();
    let mut render_state = RenderState::new().expect("render state");
    terminal.vt_write(b"hello world");
    let mut selection = TerminalSelection::new();
    assert!(selection.set(&terminal, (0, 0), (4, 0)).expect("set"));
    let tracked = terminal.track(Point::viewport(0, 0)).expect("track");
    assert_eq!(terminal.active_screen(), ActiveScreen::Primary);
    assert_eq!(tracked.screen(), ActiveScreen::Primary);

    terminal.vt_write(b"\x1b[?1049h");
    assert_eq!(terminal.active_screen(), ActiveScreen::Alternate);
    // The ref itself is untouched — it still answers, against the
    // primary screen's page list.
    assert!(tracked.has_value());
    assert_eq!(tracked.point(PointTag::Screen), Some(Point::screen(0, 0)));
    // The selection is not drawn and copies nothing.
    assert!(selection.visible_spans(&terminal, COLS, ROWS).is_empty());
    assert_eq!(
        selection
            .selected_text(&terminal, &mut render_state, COLS, ROWS)
            .expect("selected text"),
        None
    );
    let snapshot = selection
        .snapshot(&terminal, &mut render_state, COLS, ROWS)
        .expect("snapshot")
        .expect("the selection is still held");
    assert_eq!(snapshot.text, None);
    assert!(!snapshot.anchor_visible);
    assert!(!snapshot.cursor_visible);
    // Nor can it be dragged onto the other screen's page list.
    assert!(!selection.update(&terminal, 6, 0).expect("update"));

    terminal.vt_write(b"\x1b[?1049l");
    assert_eq!(terminal.active_screen(), ActiveScreen::Primary);
    assert_eq!(
        selection
            .selected_text(&terminal, &mut render_state, COLS, ROWS)
            .expect("selected text"),
        Some("hello".into())
    );
    assert!(!selection.visible_spans(&terminal, COLS, ROWS).is_empty());
}

/// A selection made *on* the alternate screen behaves the same way in
/// reverse — it belongs to the alt screen and is gone once the app
/// exits, which is also when libghostty discards that screen's rows.
#[test]
fn an_alt_screen_selection_does_not_leak_onto_the_primary_screen() {
    let mut terminal = terminal();
    let mut render_state = RenderState::new().expect("render state");
    terminal.vt_write(b"primary text");
    // `\e[?1049h` keeps the cursor where it was, so home it before
    // writing or the alt-screen text starts mid-row and wraps.
    terminal.vt_write(b"\x1b[?1049h\x1b[H");
    terminal.vt_write(b"ALTERNATE");
    let mut selection = TerminalSelection::new();
    assert!(selection.set(&terminal, (0, 0), (8, 0)).expect("set"));
    assert_eq!(
        selection
            .selected_text(&terminal, &mut render_state, COLS, ROWS)
            .expect("selected text"),
        Some("ALTERNATE".into())
    );

    terminal.vt_write(b"\x1b[?1049l");

    assert_eq!(
        selection
            .selected_text(&terminal, &mut render_state, COLS, ROWS)
            .expect("selected text"),
        None,
        "an alt-screen selection copied against the primary screen"
    );
    assert!(selection.visible_spans(&terminal, COLS, ROWS).is_empty());
}
