#![cfg(feature = "ffi")]
//! Pins the dirty-tracking semantics of libghostty's render state at our
//! pinned Ghostty SHA. Every expectation here was measured against the
//! real `libghostty-vt.a` before the wrapper was written (plan 017 §2.2);
//! a failure means libghostty's behavior moved, not that the wrapper is
//! merely mis-specified.

use roost_vt::{ColorRgb, Dirty, RenderState, ScrollViewport, Terminal, TerminalOptions};

/// A terminal + render state that have never been updated: the render
/// state still reports `Full` on its first `update`.
fn fresh(cols: u16, rows: u16) -> (Terminal, RenderState) {
    let t = Terminal::new(TerminalOptions {
        cols,
        rows,
        max_scrollback: 500,
    })
    .expect("Terminal::new");
    (t, RenderState::new().expect("RenderState::new"))
}

/// A terminal + render state already drained to `Clean`, so the next
/// change is the only damage the test sees.
fn settled(cols: u16, rows: u16) -> (Terminal, RenderState) {
    let (t, mut rs) = fresh(cols, rows);
    settle(&mut rs, &t);
    (t, rs)
}

/// Drain the current damage and confirm the terminal has settled to
/// `Clean` — i.e. both dirty layers really were cleared.
fn settle(rs: &mut RenderState, t: &Terminal) {
    rs.update(t).expect("update");
    rs.walk_dirty(t, |_, _| {}).expect("walk_dirty");
    rs.update(t).expect("update");
    assert_eq!(
        rs.dirty().expect("dirty"),
        Dirty::Clean,
        "terminal should settle to Clean after a walk_dirty + no-change update"
    );
}

/// Row indices visited by one `walk_dirty`, plus the state it reported.
fn visit(rs: &mut RenderState, t: &Terminal) -> (Dirty, Vec<u32>) {
    let mut rows = Vec::new();
    let state = rs
        .walk_dirty(t, |row, _| rows.push(row))
        .expect("walk_dirty");
    (state, rows)
}

fn row_text(cells: &[roost_vt::Cell]) -> String {
    cells
        .iter()
        .map(|c| if c.text.is_empty() { " " } else { &c.text })
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn fresh_update_reports_full_and_visits_every_row() {
    let (t, mut rs) = fresh(80, 24);
    rs.update(&t).expect("update");

    assert_eq!(rs.dirty().expect("dirty"), Dirty::Full);
    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Full, "walk_dirty returns the state on entry");
    assert_eq!(rows, (0..24).collect::<Vec<u32>>());
}

#[test]
fn settled_terminal_reports_clean_and_visits_nothing() {
    let (t, mut rs) = settled(80, 24);

    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Clean);
    // Zero rows visited is the proof that walk_dirty cleared BOTH layers:
    // had it cleared only the global one, every row flag would survive
    // and this walk would visit all 24.
    assert!(rows.is_empty(), "expected no rows visited, got {rows:?}");
}

#[test]
fn single_row_write_reports_partial_on_that_row_only() {
    let (mut t, mut rs) = settled(80, 24);

    t.vt_write(b"hello");
    rs.update(&t).expect("update");
    assert_eq!(rs.dirty().expect("dirty"), Dirty::Partial);

    let mut visited: Vec<(u32, String)> = Vec::new();
    let state = rs
        .walk_dirty(&t, |row, cells| visited.push((row, row_text(cells))))
        .expect("walk_dirty");

    assert_eq!(state, Dirty::Partial);
    assert_eq!(visited.len(), 1, "expected only row 0, got {visited:?}");
    assert_eq!(visited[0].0, 0);
    assert_eq!(visited[0].1, "hello");
}

#[test]
fn row_flags_are_cleared_alongside_the_global_layer() {
    // render.h's "extremely important detail": the global and per-row
    // dirty layers are independent, so clearing one does not clear the
    // other. If `walk_dirty` cleared only the global layer, row 0's flag
    // would survive its first walk and row 0 would be redrawn on every
    // subsequent frame forever. This test is what catches that.
    let (mut t, mut rs) = settled(80, 24);

    t.vt_write(b"hello");
    rs.update(&t).expect("update");
    let (_, first) = visit(&mut rs, &t);
    assert_eq!(first, vec![0]);

    // Park the cursor off row 0 and settle, so the next write's cursor
    // movement can't re-dirty row 0 for a reason unrelated to the
    // footgun (§2.2 finding 4: a cursor move dirties both the row it
    // leaves and the row it lands on).
    t.vt_write(b"\x1b[11;1H");
    rs.update(&t).expect("update");
    let _ = visit(&mut rs, &t);
    settle(&mut rs, &t);

    t.vt_write(b"\x1b[5;1HmarkerX");
    rs.update(&t).expect("update");
    let (state, second) = visit(&mut rs, &t);

    assert_eq!(state, Dirty::Partial);
    assert!(
        !second.contains(&0),
        "row 0 must not be revisited — its flag was consumed by the first \
         walk_dirty; got {second:?}"
    );
    // Row 4 is the write; row 10 is the cursor row it vacated.
    assert_eq!(second, vec![4, 10]);
}

#[test]
fn full_dirty_state_is_cleared_by_walk_dirty() {
    // Full-layer counterpart to `row_flags_are_cleared_alongside_the_global_layer`
    // above, which pins that the ROW layer would survive a walk that
    // cleared only the global one. This test pins the other half:
    // forcing the GLOBAL layer to Full and confirming `walk_dirty`
    // clears it too, so a subsequent no-change frame settles to Clean
    // instead of reporting Full forever.
    let (t, mut rs) = settled(80, 24);

    rs.mark_full().expect("mark_full");
    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Full);
    assert_eq!(rows, (0..24).collect::<Vec<u32>>());

    rs.update(&t).expect("update");
    assert_eq!(rs.dirty().expect("dirty"), Dirty::Clean);
    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Clean);
    assert!(rows.is_empty(), "expected no rows visited, got {rows:?}");
}

#[test]
fn dirty_rows_is_a_pure_read() {
    let (mut t, mut rs) = settled(80, 24);

    t.vt_write(b"hello");
    rs.update(&t).expect("update");

    let first = rs.dirty_rows(&t).expect("dirty_rows");
    let second = rs.dirty_rows(&t).expect("dirty_rows");
    assert_eq!(first, vec![0]);
    assert_eq!(first, second, "dirty_rows must not consume anything");
    assert_eq!(rs.dirty().expect("dirty"), Dirty::Partial);

    let (_, rows) = visit(&mut rs, &t);
    assert_eq!(
        rows, first,
        "walk_dirty still sees the rows dirty_rows read"
    );
}

#[test]
fn mark_full_forces_a_full_visit() {
    let (t, mut rs) = settled(80, 24);

    rs.mark_full().expect("mark_full");
    assert_eq!(rs.dirty().expect("dirty"), Dirty::Full);

    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Full);
    assert_eq!(rows, (0..24).collect::<Vec<u32>>());
}

#[test]
fn theme_color_changes_report_full() {
    let (mut t, mut rs) = settled(80, 24);

    // The Rust theme path — no vt bytes at all.
    t.set_color_foreground(ColorRgb::new(1, 2, 3))
        .expect("set_color_foreground");
    t.set_color_background(ColorRgb::new(4, 5, 6))
        .expect("set_color_background");
    rs.update(&t).expect("update");
    assert_eq!(rs.dirty().expect("dirty"), Dirty::Full);
    let (_, rows) = visit(&mut rs, &t);
    assert_eq!(rows.len(), 24);

    settle(&mut rs, &t);
    t.set_color_palette(&[ColorRgb::new(9, 9, 9); 256])
        .expect("set_color_palette");
    rs.update(&t).expect("update");
    assert_eq!(rs.dirty().expect("dirty"), Dirty::Full);
    let (_, rows) = visit(&mut rs, &t);
    assert_eq!(rows.len(), 24);
}

#[test]
fn resize_reports_full_over_the_new_row_count() {
    let (mut t, mut rs) = settled(80, 24);

    t.resize(100, 30, 8, 16).expect("resize");
    rs.update(&t).expect("update");

    assert_eq!(rs.dirty().expect("dirty"), Dirty::Full);
    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Full);
    assert_eq!(rows, (0..30).collect::<Vec<u32>>());
}

#[test]
fn viewport_scroll_reports_full() {
    let (mut t, mut rs) = fresh(20, 6);
    for i in 0..40 {
        t.vt_write(format!("line-{i:03}\r\n").as_bytes());
    }
    settle(&mut rs, &t);

    // A pure viewport move writes no cells, yet every visible row now
    // shows different content — libghostty reports FULL, which is what
    // lets a row cache trust it (§2.2 probe [S1]).
    t.scroll_viewport(ScrollViewport::Delta(-10));
    rs.update(&t).expect("update");

    assert_eq!(rs.dirty().expect("dirty"), Dirty::Full);
    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Full);
    assert_eq!(rows, (0..6).collect::<Vec<u32>>());
}

#[test]
fn output_driven_scroll_reports_full() {
    let (mut t, mut rs) = fresh(20, 6);
    for i in 0..6 {
        t.vt_write(format!("\x1b[{};1Hrow-{i}", i + 1).as_bytes());
    }
    // The marker loop's last write already leaves the cursor on row 6
    // (0-indexed 5), the last viewport row.
    settle(&mut rs, &t);

    // Writing past the last row scrolls the viewport by one line via
    // normal PTY output — no explicit `scroll_viewport` call. Per
    // third_party/ghostty/src/src/terminal/render.zig:299-302 ("If our
    // viewport pin changed, we do a full rebuild"), libghostty reports
    // this the same way as `viewport_scroll_reports_full` above: a full
    // rebuild, not an incremental per-row change. Streaming output that
    // scrolls the viewport therefore gets no incremental benefit from
    // dirty tracking. This test pins that expectation so a future
    // Ghostty bump that changes it is noticed.
    t.vt_write(b"\r\nSCROLLED");
    rs.update(&t).expect("update");

    assert_eq!(rs.dirty().expect("dirty"), Dirty::Full);
    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Full);
    assert_eq!(rows, (0..6).collect::<Vec<u32>>());
}

#[test]
fn in_place_write_without_scrolling_stays_partial() {
    let (mut t, mut rs) = fresh(20, 6);
    for i in 0..6 {
        t.vt_write(format!("\x1b[{};1Hrow-{i}", i + 1).as_bytes());
    }
    settle(&mut rs, &t);

    // No viewport-pin change here — the write lands in place on row 2,
    // moving the cursor off row 6 (where the marker loop parked it).
    t.vt_write(b"\x1b[2;1HINPLACE");
    rs.update(&t).expect("update");

    assert_eq!(rs.dirty().expect("dirty"), Dirty::Partial);
    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Partial);
    // Row 1 (0-indexed) is the write; row 5 is the cursor row it
    // vacated. This is the case where E3 actually saves work: only the
    // touched rows are marked, not a full rebuild.
    assert_eq!(rows, vec![1, 5]);
}

#[test]
fn repeated_update_without_walk_sheds_no_damage() {
    // `update` must never *lower* dirty state — only `walk_dirty`
    // consumes it. Both UIs' refresh paths call `update` before their
    // cache guards, and diagnostics (`tab.dump`, `tab.dump_resolved`)
    // call `update` again out of band between frames. If a second
    // `update` shed the pending damage, an IPC dump interleaved with
    // PTY output would silently drop the rows the next paint owed.
    let (mut t, mut rs) = settled(80, 24);

    t.vt_write(b"\x1b[7;1Hhello");
    rs.update(&t).expect("update");
    assert_eq!(rs.dirty().expect("dirty"), Dirty::Partial);
    rs.update(&t).expect("second update");
    assert_eq!(
        rs.dirty().expect("dirty"),
        Dirty::Partial,
        "a second update must not shed the pending damage"
    );

    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Partial);
    assert!(
        rows.contains(&6),
        "the written row must still be visited after two updates; got {rows:?}"
    );
}

#[test]
fn mark_full_survives_an_update() {
    // A cache guard that raises `Full` before the next `update` (or a
    // caller that gets the order wrong) must still get a full visit.
    // Plan 018 D2 pins update-before-guards as the ordering; this
    // property is what makes ANY ordering safe by construction — guards
    // only ever raise, and `update` never lowers.
    let (t, mut rs) = settled(80, 24);

    rs.mark_full().expect("mark_full");
    rs.update(&t).expect("update");
    assert_eq!(
        rs.dirty().expect("dirty"),
        Dirty::Full,
        "a no-change update must not lower a Full raised before it"
    );

    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Full);
    assert_eq!(rows, (0..24).collect::<Vec<u32>>());
}

#[test]
fn osc4_palette_bytes_report_full() {
    // Both UIs' row caches depend on the OSC 4 palette path reporting
    // Full: a palette entry change re-colors every cell that references
    // it, and neither UI compares the palette itself (measured probe
    // [S6], plans 017/018). Unlike OSC 10/11 — which libghostty reports
    // Clean for, and which the UIs' resolvers never read — OSC 4 *does*
    // reach the resolvers through per-cell indexed colors. If a Ghostty
    // pin bump regresses this, fail loudly here rather than as a
    // stale-colors rendering bug nobody can trace.
    let (mut t, mut rs) = settled(80, 24);

    t.vt_write(b"\x1b]4;1;#abcdef\x07");
    rs.update(&t).expect("update");

    assert_eq!(rs.dirty().expect("dirty"), Dirty::Full);
    let (state, rows) = visit(&mut rs, &t);
    assert_eq!(state, Dirty::Full);
    assert_eq!(rows, (0..24).collect::<Vec<u32>>());
}

#[test]
fn osc_default_color_change_reports_clean_known_limitation() {
    // KNOWN LIMITATION, pinned deliberately. At our Ghostty pin, changing
    // the default background via OSC 11 as PTY bytes marks NOTHING dirty
    // — not the global layer, not a single row — even though every cell
    // that inherits the default now renders differently. (The same change
    // made through the Rust API, `set_color_background`, does report
    // FULL; only the OSC-bytes path is silent.)
    //
    // This is why `roost-iced` must compare the default fg/bg itself and
    // call `mark_full` when they move (plan 017 D3b) instead of trusting
    // dirty tracking alone.
    //
    // If a future Ghostty bump starts reporting Full here, this test
    // fails loudly — that is the point. At that time it can be relaxed
    // and D3b's guard reconsidered.
    //
    // Both defaults are set before settling, mirroring what roost always
    // does at attach — libghostty's colors reporting (render.zig's
    // `orelse break :bg_fg`) only fully engages once both fg and bg
    // defaults are set, so a test that left one unset would not pin the
    // behavior roost actually experiences.
    let (mut t, mut rs) = fresh(80, 24);
    t.set_color_foreground(ColorRgb::new(0xAA, 0xAA, 0xAA))
        .expect("set_color_foreground");
    t.set_color_background(ColorRgb::new(0x11, 0x11, 0x11))
        .expect("set_color_background");
    settle(&mut rs, &t);

    t.vt_write(b"\x1b]11;#123456\x07");
    rs.update(&t).expect("update");

    assert_eq!(
        rs.dirty().expect("dirty"),
        Dirty::Clean,
        "OSC 11 reporting dirty would be an IMPROVEMENT — see the comment"
    );
    assert!(rs.dirty_rows(&t).expect("dirty_rows").is_empty());
    let (_, rows) = visit(&mut rs, &t);
    assert!(rows.is_empty(), "expected no rows visited, got {rows:?}");
}

#[test]
fn decscnm_swaps_reported_colors_without_marking_dirty() {
    // Same class of limitation as the OSC 10/11 test above: libghostty's
    // colors() swap on DECSCNM (reverse video) only applies when BOTH
    // defaults are set — render.zig:326-339 does `orelse break :bg_fg` —
    // and even then the swap itself marks nothing dirty. Since roost
    // always sets both defaults at attach, this is the transition the
    // iced consumer actually hits: it must compare the default fg/bg
    // pair itself on every frame rather than trusting dirty state for
    // this case.
    let (mut t, mut rs) = fresh(80, 24);
    t.set_color_foreground(ColorRgb::new(0xAA, 0xAA, 0xAA))
        .expect("set_color_foreground");
    t.set_color_background(ColorRgb::new(0x11, 0x11, 0x11))
        .expect("set_color_background");
    t.vt_write(b"hello");
    settle(&mut rs, &t);

    let before = rs.colors().expect("colors");
    assert_eq!(before.foreground, ColorRgb::new(0xAA, 0xAA, 0xAA));
    assert_eq!(before.background, ColorRgb::new(0x11, 0x11, 0x11));

    t.vt_write(b"\x1b[?5h");
    rs.update(&t).expect("update");

    let after = rs.colors().expect("colors");
    assert_eq!(
        after.foreground,
        ColorRgb::new(0x11, 0x11, 0x11),
        "DECSCNM should swap the reported foreground to the old background"
    );
    assert_eq!(
        after.background,
        ColorRgb::new(0xAA, 0xAA, 0xAA),
        "DECSCNM should swap the reported background to the old foreground"
    );

    assert_eq!(
        rs.dirty().expect("dirty"),
        Dirty::Clean,
        "the color swap itself must not mark anything dirty — see comment"
    );
    assert!(rs.dirty_rows(&t).expect("dirty_rows").is_empty());
}
