//! Minimal sfnt construction shared by the vendored-swash regression tests.
//!
//! The tests here feed swash hand-built fonts that no real font tool would
//! emit, so the builder stays deliberately dumb: it lays tables out in tag
//! order and computes nothing but offsets and lengths.

const HEAD_LEN: usize = 54;
const MAXP_LEN: usize = 6;

/// `unitsPerEm` sits at offset 18, `indexToLocFormat` at offset 50 (0 =
/// short loca, unused by these tests).
pub fn head() -> Vec<u8> {
    let mut t = vec![0u8; HEAD_LEN];
    t[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    t[18..20].copy_from_slice(&1000u16.to_be_bytes());
    t[50..52].copy_from_slice(&0u16.to_be_bytes());
    t
}

pub fn maxp(glyph_count: u16) -> Vec<u8> {
    let mut t = vec![0u8; MAXP_LEN];
    t[0..4].copy_from_slice(&0x0000_5000u32.to_be_bytes());
    t[4..6].copy_from_slice(&glyph_count.to_be_bytes());
    t
}

/// swash resolves tables by binary search over the directory
/// (`internal/mod.rs::table_range`), so records must be sorted by tag.
pub fn build_font(mut tables: Vec<(&[u8; 4], Vec<u8>)>) -> Vec<u8> {
    tables.sort_by_key(|(tag, _)| *tag);

    let directory_len = 12 + tables.len() * 16;
    let mut font = Vec::with_capacity(directory_len);
    let mut body = Vec::new();

    font.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    font.extend_from_slice(&(tables.len() as u16).to_be_bytes());
    font.extend_from_slice(&[0u8; 6]);

    for (tag, data) in &tables {
        let offset = directory_len + body.len();
        font.extend_from_slice(*tag);
        font.extend_from_slice(&0u32.to_be_bytes());
        font.extend_from_slice(&(offset as u32).to_be_bytes());
        font.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
    }

    font.extend_from_slice(&body);
    font
}
