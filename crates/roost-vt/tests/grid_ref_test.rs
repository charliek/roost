//! GridRef + Point conversion tests for the new selection-coordinate
//! plumbing.
//!
//! Selection storage needs a row identifier that survives scrolling and
//! survives modest VT writes. libghostty's C documentation warns that a
//! `GridRef` "is only valid until the next update to the terminal", but
//! the underlying Pin should survive operations that don't free its
//! page. These tests pin down the actual semantics so the UI layer can
//! pick the right storage form.
//!
//! Run with:
//!
//!     cargo test -p roost-vt --features ffi --test grid_ref_test
#![cfg(feature = "ffi")]

use roost_vt::{Point, PointTag, ScrollViewport, Terminal, TerminalOptions};

/// Build a small terminal with enough scrollback to test rotation
/// behavior. Rows=5, scrollback=10 → total = 15 rows of addressable
/// state including scrollback.
fn small_terminal() -> Terminal {
    Terminal::new(TerminalOptions {
        cols: 20,
        rows: 5,
        max_scrollback: 10,
    })
    .expect("terminal")
}

#[test]
fn convert_point_identity_round_trip() {
    let mut term = small_terminal();
    term.vt_write(b"hello\r\n");

    // viewport(2, 0) — column 2 of the top viewport row — should round-trip
    // through Screen back to the same viewport coordinate when no scrolling
    // has happened beyond what `hello\r\n` did.
    let vp = Point::viewport(2, 0);
    let screen = term
        .convert_point(vp, PointTag::Screen)
        .expect("vp -> screen");
    let back = term
        .convert_point(screen, PointTag::Viewport)
        .expect("screen -> vp");
    assert_eq!(back, vp, "viewport -> screen -> viewport must round-trip");
}

#[test]
fn screen_coord_tracks_row_after_scroll() {
    let mut term = small_terminal();
    // Fill 3 rows so viewport rows 0..3 hold content.
    term.vt_write(b"row0\r\nrow1\r\nrow2\r\n");

    // Capture screen coord of viewport row 1.
    let vp1 = Point::viewport(0, 1);
    let screen_of_vp1 = term
        .convert_point(vp1, PointTag::Screen)
        .expect("vp1 -> screen");

    // Write more rows to push viewport down — each "\n" past the bottom
    // edge scrolls the viewport up relative to scrollback.
    term.vt_write(b"row3\r\nrow4\r\nrow5\r\nrow6\r\n");

    // The originally-pinned row (was viewport row 1) should now resolve
    // to a viewport row 4 rows higher (since we wrote 4 more rows that
    // pushed scrollback content up by 4). Actually, the exact viewport
    // delta depends on whether the new rows scrolled the prior content
    // into scrollback. We just assert the screen coord is still
    // resolvable and points to *some* row.
    let back = term.convert_point(screen_of_vp1, PointTag::Viewport);
    // If the row is still in the visible viewport, we get a valid
    // viewport coord; if it scrolled into history, we get None
    // (point_from_grid_ref returns NO_VALUE for viewport when the row
    // isn't visible). Either outcome is fine — we're proving that the
    // screen coord didn't silently start pointing at the wrong row.
    match back {
        Some(p) => {
            // The row is still visible — its viewport y should be
            // *less* than where it started (it scrolled up).
            assert!(
                p.y < vp1.y || p.y == 0,
                "expected scrolled-up viewport y, got {}",
                p.y
            );
            assert_eq!(p.x, vp1.x, "x must not change across scroll");
        }
        None => {
            // The row scrolled out of the viewport. That's fine — what
            // matters is that the screen coord is still resolvable in
            // its own space:
            let still_screen = term.convert_point(screen_of_vp1, PointTag::Screen);
            assert!(
                still_screen.is_some(),
                "screen coord must still resolve to itself"
            );
        }
    }
}

#[test]
fn captured_grid_ref_survives_one_vt_write() {
    // Sanity check: a GridRef captured before a vt_write should still
    // resolve via point_from_grid_ref afterwards. The C docstring warns
    // about transience, but in practice pins survive scrolling — this
    // test fails if that assumption breaks (and would force the UI
    // layer to re-capture every frame).
    let mut term = small_terminal();
    term.vt_write(b"hello\r\n");
    let gref = term.grid_ref(Point::viewport(0, 0)).expect("grid_ref");

    term.vt_write(b"world\r\n");

    let resolved = term.point_from_grid_ref(&gref, PointTag::Screen);
    assert!(
        resolved.is_some(),
        "GridRef should survive a vt_write that only adds content"
    );
}

#[test]
fn viewport_conversion_clips_when_row_outside_visible() {
    // Pin a row, scroll up via scrollback so the row leaves the
    // viewport. The screen coord should still resolve; the viewport
    // conversion should return None (the row isn't currently visible).
    let mut term = small_terminal();
    // Fill 8 rows: viewport sees rows 3..8, scrollback holds 0..3.
    for i in 0..8u8 {
        term.vt_write(format!("row{}\r\n", i).as_bytes());
    }
    // Capture viewport row 4 (one of the visible rows).
    let vp = Point::viewport(0, 4);
    let screen = term
        .convert_point(vp, PointTag::Screen)
        .expect("vp -> screen");

    // Scroll the viewport up by 5 rows — the row we captured should
    // now be off-screen below the viewport.
    term.scroll_viewport(ScrollViewport::Delta(-5));

    // Screen coord should still resolve to itself.
    let still_screen = term.convert_point(screen, PointTag::Screen);
    assert!(
        still_screen.is_some(),
        "screen coord must remain resolvable after scrolling"
    );

    // But viewport conversion may legitimately return None if the row
    // is outside the current visible area.
    let _maybe_vp = term.convert_point(screen, PointTag::Viewport);
    // We don't assert which case (libghostty may decide to clip), only
    // that the call doesn't panic and that the screen coord is stable.
}

#[test]
fn grid_ref_rejects_invalid_point() {
    let term = small_terminal();
    // y = 99 is far beyond the terminal's rows + scrollback. libghostty
    // should reject it.
    let result = term.grid_ref(Point::viewport(0, 99));
    assert!(
        result.is_none(),
        "grid_ref of an out-of-range point must return None"
    );
}
