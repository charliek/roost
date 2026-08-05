//! Regression coverage for issue #292: a font whose `hhea`/`vhea` declares
//! zero long metrics must not underflow swash's shared `xmtx::advance`.
//!
//! The guard lives in `third_party/swash/src/internal/xmtx.rs::advance`, which
//! serves both the `hmtx` and `vmtx` readers. The vertical assertion below
//! locks that behavior in case a future refactor splits the two readers.

use swash::FontRef;

const HEAD_LEN: usize = 54;
const MAXP_LEN: usize = 6;
const XHEA_LEN: usize = 36;
const XMTX_LEN: usize = 4;
const VORG_LEN: usize = 8;

struct Table {
    tag: &'static [u8; 4],
    data: Vec<u8>,
}

/// `unitsPerEm` sits at offset 18, `indexToLocFormat` at offset 50 (0 =
/// short loca, unused by this test).
fn head() -> Vec<u8> {
    let mut t = vec![0u8; HEAD_LEN];
    t[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    t[18..20].copy_from_slice(&1000u16.to_be_bytes());
    t[50..52].copy_from_slice(&0u16.to_be_bytes());
    t
}

fn maxp(glyph_count: u16) -> Vec<u8> {
    let mut t = vec![0u8; MAXP_LEN];
    t[0..4].copy_from_slice(&0x0000_5000u32.to_be_bytes());
    t[4..6].copy_from_slice(&glyph_count.to_be_bytes());
    t
}

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

/// swash resolves tables by binary search over the directory
/// (`internal/mod.rs::table_range`), so records must be sorted by tag.
fn build_font(tables: Vec<Table>) -> Vec<u8> {
    let mut tables = tables;
    tables.sort_by(|a, b| a.tag.cmp(b.tag));

    let num_tables = tables.len();
    let directory_len = 12 + num_tables * 16;
    let mut directory = Vec::with_capacity(directory_len);
    let mut body = Vec::new();

    directory.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    directory.extend_from_slice(&(num_tables as u16).to_be_bytes());
    directory.extend_from_slice(&[0u8; 6]);

    for table in &tables {
        let offset = directory_len + body.len();
        directory.extend_from_slice(table.tag);
        directory.extend_from_slice(&0u32.to_be_bytes());
        directory.extend_from_slice(&(offset as u32).to_be_bytes());
        directory.extend_from_slice(&(table.data.len() as u32).to_be_bytes());
        body.extend_from_slice(&table.data);
    }

    directory.extend_from_slice(&body);
    directory
}

fn zero_long_metrics_font() -> Vec<u8> {
    build_font(vec![
        Table {
            tag: b"head",
            data: head(),
        },
        Table {
            tag: b"maxp",
            data: maxp(2),
        },
        Table {
            tag: b"hhea",
            data: xhea(0),
        },
        Table {
            tag: b"hmtx",
            data: vec![0u8; XMTX_LEN],
        },
        Table {
            tag: b"vhea",
            data: xhea(0),
        },
        Table {
            tag: b"vmtx",
            data: vec![0u8; XMTX_LEN],
        },
        Table {
            tag: b"VORG",
            data: vorg(),
        },
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
