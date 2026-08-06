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
