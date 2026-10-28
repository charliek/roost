//! Unicode format characters (`General_Category=Cf`): the bidi overrides
//! and isolates, the zero-width characters and the rest of the category,
//! none of which `char` can classify.

/// `General_Category=Cf`, as ranges. `char` exposes `is_control` (Cc) and
/// nothing else, and a Unicode-table crate is a large dependency to carry
/// for one category, so it is spelled out here: Unicode 16.0's Cf,
/// coalesced from a full scalar-by-scalar walk of `UnicodeData.txt`.
///
/// Two ranges are widened past the real category to keep the table short.
/// U+2065 and U+E0002–U+E001F are unassigned holes inside a format block;
/// over-broad is the safe direction, since escaping or dropping an
/// unassigned scalar costs nothing while a missed range lets a reversal
/// through.
pub const FORMAT_RANGES: [(u32, u32); 19] = [
    (0x00ad, 0x00ad),           // SOFT HYPHEN
    (0x0600, 0x0605),           // ARABIC NUMBER SIGN … ARABIC NUMBER MARK ABOVE
    (0x061c, 0x061c),           // ARABIC LETTER MARK
    (0x06dd, 0x06dd),           // ARABIC END OF AYAH
    (0x070f, 0x070f),           // SYRIAC ABBREVIATION MARK
    (0x0890, 0x0891),           // ARABIC POUND / PIASTRE MARK ABOVE
    (0x08e2, 0x08e2),           // ARABIC DISPUTED END OF AYAH
    (0x180e, 0x180e),           // MONGOLIAN VOWEL SEPARATOR
    (0x200b, 0x200f),           // ZWSP, ZWNJ, ZWJ, LRM, RLM
    (0x202a, 0x202e),           // LRE, RLE, PDF, LRO, RLO — the bidi overrides
    (0x2060, 0x206f),           // WORD JOINER … the deprecated format controls
    (0xfeff, 0xfeff),           // ZERO WIDTH NO-BREAK SPACE (BOM)
    (0xfff9, 0xfffb),           // INTERLINEAR ANNOTATION ANCHOR/SEPARATOR/TERMINATOR
    (0x0001_10bd, 0x0001_10bd), // KAITHI NUMBER SIGN
    (0x0001_10cd, 0x0001_10cd), // KAITHI NUMBER SIGN ABOVE
    (0x0001_3430, 0x0001_343f), // EGYPTIAN HIEROGLYPH format controls
    (0x0001_bca0, 0x0001_bca3), // SHORTHAND FORMAT letter overlap … up step
    (0x0001_d173, 0x0001_d17a), // MUSICAL SYMBOL BEGIN BEAM … END PHRASE
    (0x000e_0001, 0x000e_007f), // LANGUAGE TAG + the TAG characters
];

pub fn is_format(c: char) -> bool {
    let cp = c as u32;
    FORMAT_RANGES
        .iter()
        .any(|(lo, hi)| (*lo..=*hi).contains(&cp))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sorted, non-empty and disjoint, so `is_format`'s linear scan is a
    /// table anyone can read against `UnicodeData.txt` rather than a pile.
    #[test]
    fn the_ranges_are_sorted_non_empty_and_disjoint() {
        for pair in FORMAT_RANGES.windows(2) {
            assert!(pair[0].0 <= pair[0].1, "{pair:?}");
            assert!(pair[0].1 < pair[1].0, "{pair:?}");
        }
    }
}
