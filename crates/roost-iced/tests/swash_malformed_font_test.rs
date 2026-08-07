//! Regression coverage for issue #299: malformed font tables must not wedge
//! or crash the vendored swash. Each test drives a hand-built sfnt through
//! public swash API and asserts only that the call *returns* — the deltas in
//! `third_party/swash/` are safety fixes, not correctness fixes, so the values
//! that come back are deliberately not asserted.

mod sfnt;

use sfnt::{build_font, head, maxp};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;
use swash::scale::{ScaleContext, StrikeWith};
use swash::FontRef;

const LOOKUP_GLYPH: u16 = 1;
const STRIKE_PPEM: u8 = 16;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// A nonzero normalized coordinate (1.0 in 2.14) so variation lookups do real
/// work instead of short-circuiting on an unvaried instance.
const COORD: i16 = 1 << 14;
const COORDS: [i16; 1] = [COORD];

/// EBLC carrying one strike whose single index subtable declares
/// `indexFormat = 4` (the sorted-glyph-id bisection).
///
/// The offsets below are the ones swash actually reads in
/// `strike.rs::get_coverage` / `get_location`: `numSizes` at 4, a 48-byte
/// bitmapSizeTable at 8 whose `indexSubTableArrayOffset` (+0), subtable count
/// (+8), glyph range (+40/+42) and ppem/depth/flags (+44..+47) all have to
/// admit `LOOKUP_GLYPH` for the search to be reached.
fn eblc() -> Vec<u8> {
    const ARRAY_OFFSET: u32 = 56;
    const SUBTABLE_OFFSET: u32 = 8;

    let mut t = vec![0u8; 84];
    t[0..4].copy_from_slice(&0x0002_0000u32.to_be_bytes());
    t[4..8].copy_from_slice(&1u32.to_be_bytes());

    // bitmapSizeTable
    t[8..12].copy_from_slice(&ARRAY_OFFSET.to_be_bytes());
    t[12..16].copy_from_slice(&28u32.to_be_bytes());
    t[16..20].copy_from_slice(&1u32.to_be_bytes());
    t[48..50].copy_from_slice(&0u16.to_be_bytes());
    t[50..52].copy_from_slice(&2u16.to_be_bytes());
    t[52] = STRIKE_PPEM;
    t[53] = STRIKE_PPEM;
    t[54] = 1;
    t[55] = 1;

    // indexSubTableArray record: first glyph, last glyph, subtable offset.
    t[56..58].copy_from_slice(&0u16.to_be_bytes());
    t[58..60].copy_from_slice(&2u16.to_be_bytes());
    t[60..64].copy_from_slice(&SUBTABLE_OFFSET.to_be_bytes());

    // Index subtable header: indexFormat, imageFormat, imageDataOffset.
    t[64..66].copy_from_slice(&4u16.to_be_bytes());
    t[66..68].copy_from_slice(&1u16.to_be_bytes());
    t[68..72].copy_from_slice(&0u32.to_be_bytes());

    // Format 4 body: `numGlyphs`, then the glyph/offset pairs. swash probes
    // `base + i * 4`, so the i = 0 read lands on the high half of this u32 —
    // 0 for any count below 65536, which is what makes the bisection step.
    // One pair plus the spec's trailing sentinel keeps every probe in bounds.
    t[72..76].copy_from_slice(&1u32.to_be_bytes());

    t
}

fn format4_bitmap_index_font() -> Vec<u8> {
    build_font(vec![
        (b"head", head()),
        (b"maxp", maxp(3)),
        (b"EBLC", eblc()),
        (b"EBDT", 0x0002_0000u32.to_be_bytes().to_vec()),
    ])
}

#[test]
fn format4_bitmap_index_lookup_terminates() {
    let data = format4_bitmap_index_font();
    let font = FontRef::from_index(&data, 0).expect("synthetic sfnt should parse");
    assert!(
        font.alpha_strikes()
            .find_by_largest_ppem(LOOKUP_GLYPH)
            .is_some(),
        "the crafted strike must cover the glyph, or the lookup below never \
         reaches the format-4 search"
    );

    // The pristine bisection spins forever, so the lookup runs on a worker the
    // test can outlive. A hung worker keeps burning a core until the process
    // exits; run this test alone when reproducing against unfixed swash.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let font = FontRef::from_index(&data, 0).expect("synthetic sfnt should parse");
        let mut context = ScaleContext::new();
        let mut scaler = context.builder(font).size(STRIKE_PPEM as f32).build();
        let _ = tx.send(scaler.scale_bitmap(LOOKUP_GLYPH, StrikeWith::LargestSize));
    });

    match rx.recv_timeout(LOOKUP_TIMEOUT) {
        Ok(_) => {}
        Err(RecvTimeoutError::Timeout) => {
            panic!("format-4 bitmap index lookup hung (issue #299)")
        }
        Err(RecvTimeoutError::Disconnected) => panic!("format-4 bitmap index lookup panicked"),
    }
}

/// `fvar` with one axis and one named instance whose declared `instanceSize`
/// is 0. Everything up to the postscript-name-id test has to read cleanly, so
/// the table still carries a full 20-byte axis record and enough bytes past the
/// instance's `subfamilyNameID` for the axis-value array.
fn zero_instance_size_fvar() -> Vec<u8> {
    const AXES_OFFSET: u16 = 16;
    const AXIS_SIZE: u16 = 20;

    let mut t = vec![0u8; 44];
    t[0..2].copy_from_slice(&1u16.to_be_bytes());
    t[4..6].copy_from_slice(&AXES_OFFSET.to_be_bytes());
    t[6..8].copy_from_slice(&2u16.to_be_bytes());
    t[8..10].copy_from_slice(&1u16.to_be_bytes());
    t[10..12].copy_from_slice(&AXIS_SIZE.to_be_bytes());
    t[12..14].copy_from_slice(&1u16.to_be_bytes());
    t[14..16].copy_from_slice(&0u16.to_be_bytes());

    // Axis record: tag, min/default/max in 16.16, flags, axisNameID.
    t[16..20].copy_from_slice(b"wght");
    t[20..24].copy_from_slice(&(100i32 << 16).to_be_bytes());
    t[24..28].copy_from_slice(&(400i32 << 16).to_be_bytes());
    t[28..32].copy_from_slice(&(900i32 << 16).to_be_bytes());
    t[34..36].copy_from_slice(&256u16.to_be_bytes());

    // Instance record: subfamilyNameID, flags, then one 16.16 axis value.
    t[36..38].copy_from_slice(&257u16.to_be_bytes());
    t[40..44].copy_from_slice(&(400i32 << 16).to_be_bytes());

    t
}

#[test]
fn zero_instance_size_fvar_instances_iterate() {
    let data = build_font(vec![
        (b"head", head()),
        (b"maxp", maxp(3)),
        (b"fvar", zero_instance_size_fvar()),
    ]);
    let font = FontRef::from_index(&data, 0).expect("synthetic sfnt should parse");

    let instances = font.instances();
    assert_eq!(
        instances.len(),
        1,
        "the crafted fvar must expose an instance, or the walk below never \
         reaches the postscript-name-id test"
    );
    for instance in instances {
        let _ = instance.postscript_name_id();
    }
}

/// `HVAR` header length: version (4) + store/advance/lsb/rsb map offsets (4
/// each). Item variation store and delta-set index map contents both start
/// right after it, so the two builders below key their offsets off this
/// instead of restating the length.
const HVAR_HEADER_LEN: usize = 20;

/// `HVAR` header: version, then the item variation store and the three
/// (advance / lsb / rsb) delta-set index map offsets, all relative to the
/// table's own start.
fn hvar_header(store_offset: u32, advance_map_offset: u32) -> Vec<u8> {
    let mut t = vec![0u8; HVAR_HEADER_LEN];
    t[0..2].copy_from_slice(&1u16.to_be_bytes());
    t[4..8].copy_from_slice(&store_offset.to_be_bytes());
    t[8..12].copy_from_slice(&advance_map_offset.to_be_bytes());
    t
}

/// Item variation store with one region and one item variation data subtable
/// whose `shortDeltaCount` exceeds its `regionIndexCount`.
fn short_delta_overrun_hvar() -> Vec<u8> {
    const STORE: usize = HVAR_HEADER_LEN;
    const REGION_LIST: u32 = 12;
    const ITEM_DATA: u32 = 22;

    let mut t = hvar_header(STORE as u32, 0);
    t.resize(STORE + 44, 0);

    // Item variation store header.
    t[STORE..STORE + 2].copy_from_slice(&1u16.to_be_bytes());
    t[STORE + 2..STORE + 6].copy_from_slice(&REGION_LIST.to_be_bytes());
    t[STORE + 6..STORE + 8].copy_from_slice(&1u16.to_be_bytes());
    t[STORE + 8..STORE + 12].copy_from_slice(&ITEM_DATA.to_be_bytes());

    // Region list: one axis, one region, peaking at the coordinate below.
    let regions = STORE + REGION_LIST as usize;
    t[regions..regions + 2].copy_from_slice(&1u16.to_be_bytes());
    t[regions + 2..regions + 4].copy_from_slice(&1u16.to_be_bytes());
    t[regions + 6..regions + 8].copy_from_slice(&COORD.to_be_bytes());
    t[regions + 8..regions + 10].copy_from_slice(&COORD.to_be_bytes());

    // Item variation data: itemCount, shortDeltaCount, regionIndexCount,
    // regionIndexes. shortDeltaCount > regionIndexCount is the malformation.
    let item = STORE + ITEM_DATA as usize;
    t[item..item + 2].copy_from_slice(&2u16.to_be_bytes());
    t[item + 2..item + 4].copy_from_slice(&3u16.to_be_bytes());
    t[item + 4..item + 6].copy_from_slice(&1u16.to_be_bytes());

    t
}

/// Delta-set index map declaring `mapCount == 0`, with a well-formed (if
/// unreachable) store behind it so the lookup fails on the map, not earlier.
fn empty_index_map_hvar() -> Vec<u8> {
    const MAP: usize = HVAR_HEADER_LEN;
    const STORE: usize = 32;

    let mut t = hvar_header(STORE as u32, MAP as u32);
    t.resize(STORE + 16, 0);

    // entryFormat 0 = one-byte entries, one-bit inner index.
    t[MAP..MAP + 2].copy_from_slice(&0u16.to_be_bytes());
    t[MAP + 2..MAP + 4].copy_from_slice(&0u16.to_be_bytes());

    t[STORE..STORE + 2].copy_from_slice(&1u16.to_be_bytes());
    t[STORE + 2..STORE + 6].copy_from_slice(&12u32.to_be_bytes());
    t[STORE + 6..STORE + 8].copy_from_slice(&1u16.to_be_bytes());

    t
}

fn advance_width_with_hvar(hvar: Vec<u8>) -> f32 {
    let data = build_font(vec![(b"head", head()), (b"maxp", maxp(3)), (b"HVAR", hvar)]);
    let font = FontRef::from_index(&data, 0).expect("synthetic sfnt should parse");
    let metrics = font.glyph_metrics(&COORDS);
    assert!(
        metrics.has_variations(),
        "the crafted HVAR must be picked up, or the lookup below never runs"
    );
    metrics.advance_width(LOOKUP_GLYPH)
}

#[test]
fn short_delta_overrun_hvar_advance_returns() {
    let _ = advance_width_with_hvar(short_delta_overrun_hvar());
}

#[test]
fn empty_index_map_hvar_advance_returns() {
    let _ = advance_width_with_hvar(empty_index_map_hvar());
}

/// `name` table whose single record claims a MacRoman string far longer than
/// the storage area that follows it.
fn overrunning_mac_roman_name() -> Vec<u8> {
    const STORAGE: u16 = 18;

    let mut t = vec![0u8; STORAGE as usize + 4];
    t[2..4].copy_from_slice(&1u16.to_be_bytes());
    t[4..6].copy_from_slice(&STORAGE.to_be_bytes());

    // Name record: platformID 1 / encodingID 0 is MacRoman, the arm that
    // indexes its slice unchecked.
    t[6..8].copy_from_slice(&1u16.to_be_bytes());
    t[12..14].copy_from_slice(&1u16.to_be_bytes());
    t[14..16].copy_from_slice(&64u16.to_be_bytes());
    t[16..18].copy_from_slice(&0u16.to_be_bytes());
    t[STORAGE as usize..].copy_from_slice(b"Test");

    t
}

#[test]
fn overrunning_mac_roman_name_decodes() {
    let data = build_font(vec![
        (b"head", head()),
        (b"maxp", maxp(3)),
        (b"name", overrunning_mac_roman_name()),
    ]);
    let font = FontRef::from_index(&data, 0).expect("synthetic sfnt should parse");

    let strings: Vec<_> = font.localized_strings().collect();
    assert_eq!(
        strings.len(),
        1,
        "the crafted name table must expose its record, or nothing is decoded"
    );
    for string in strings {
        assert!(string.is_decodable());
        let _ = string.chars().collect::<String>();
        let _ = string.to_string();
    }
}
