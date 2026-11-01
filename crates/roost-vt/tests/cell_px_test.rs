//! `Terminal::cell_px`: the cell size libghostty derives a Kitty image's
//! cell span from, read back as the last resize set it.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use roost_vt::{Terminal, TerminalOptions};

#[test]
fn cell_px_reads_back_the_last_resize_including_zero() {
    let mut t = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
        ..Default::default()
    })
    .expect("Terminal::new");
    t.resize(100, 30, 9, 18).expect("resize");
    assert_eq!(t.cell_px().expect("cell_px"), (9, 18));
    t.resize(100, 30, 11, 23).expect("a pixel-only resize");
    assert_eq!(t.cell_px().expect("cell_px"), (11, 23));
    t.resize(60, 20, 0, 0).expect("zero cells are accepted");
    assert_eq!(t.cell_px().expect("cell_px"), (0, 0));
}
