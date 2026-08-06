//! Regression coverage for issue #292: a font whose `hhea`/`vhea` declares
//! zero long metrics must not underflow swash's shared `xmtx::advance`.
//!
//! The guard lives in `third_party/swash/src/internal/xmtx.rs::advance`, which
//! serves both the `hmtx` and `vmtx` readers. The vertical assertion below
//! locks that behavior in case a future refactor splits the two readers.

mod sfnt;

use sfnt::{build_font, head, maxp};
use swash::FontRef;

const XHEA_LEN: usize = 36;
const XMTX_LEN: usize = 4;
const VORG_LEN: usize = 8;

/// `hhea`/`vhea` share a layout; `num_long_metrics` sits at offset 34.
fn xhea(long_metric_count: u16) -> Vec<u8> {
    let mut t = vec![0u8; XHEA_LEN];
    t[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    t[34..36].copy_from_slice(&long_metric_count.to_be_bytes());
    t
}

/// Version 1.0, `defaultVertOriginY` 0, no per-glyph records. Its presence is
/// what steers swash onto the `Vertical::VmtxVorg` path.
fn vorg() -> Vec<u8> {
    let mut t = vec![0u8; VORG_LEN];
    t[0..2].copy_from_slice(&1u16.to_be_bytes());
    t
}

fn zero_long_metrics_font() -> Vec<u8> {
    build_font(vec![
        (b"head", head()),
        (b"maxp", maxp(2)),
        (b"hhea", xhea(0)),
        (b"hmtx", vec![0u8; XMTX_LEN]),
        (b"vhea", xhea(0)),
        (b"vmtx", vec![0u8; XMTX_LEN]),
        (b"VORG", vorg()),
    ])
}

#[test]
fn zero_long_metrics_font_does_not_underflow() {
    let data = zero_long_metrics_font();
    let font = FontRef::from_index(&data, 0).expect("synthetic sfnt should parse");

    let metrics = font.glyph_metrics(&[]);
    assert_eq!(metrics.advance_width(0), 0.0);
    assert_eq!(metrics.advance_width(1), 0.0);
    assert!(metrics.has_vertical_metrics());
    assert_eq!(metrics.advance_height(1), 0.0);
}
