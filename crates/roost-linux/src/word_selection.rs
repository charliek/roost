//! Pure word- and line-expansion helpers for double-/triple-click
//! selection on the GTK side. Mirrors
//! `mac/Sources/Roost/WordSelection.swift` 1:1; the shared
//! `tests/word-fixtures/` corpus pins the two ports byte-equal on every
//! supported edge case (see `tests/word_fixtures.rs`).
//!
//! No GTK imports — same shape as `roost_url`, so the algorithm is
//! exercised from unit tests without spinning up a GTK widget.
//!
//! **Word-char definition.** A character is a "word char" if it's a
//! Unicode letter, a Unicode digit, OR appears in the configured
//! `extra_word_chars` set (default `_-.+~/:@%`, matching Ghostty).
//! Counter-intuitive consequence: `/` and `.` are word chars by
//! default, so `/tmp/foo.txt` and `https://example.com` select as a
//! single unit on double-click — what every other terminal does
//! (iTerm2, gnome-terminal, kitty, Terminal.app).
//!
//! **Indexing.** Inputs and outputs are indexed by `char` (Unicode
//! codepoint), NOT by grapheme cluster — matches `roost_url`'s
//! `find_url_at` and the Mac port's `unicodeScalars`. Combining
//! marks like `e\u{0301}` count as 2 columns; that's what keeps the
//! cross-port fixture corpus byte-equal.

/// One word- or line-span inside a terminal row. Both ends are
/// inclusive char-indexed columns. Mirrors Swift's `WordSpan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordSpan {
    pub col0: u16,
    pub col1: u16,
}

/// Default extra-word-char set: chars that count as word chars beyond
/// Unicode letters/digits, so file paths (`/tmp/foo.txt`), URLs
/// (`https://example.com`), and identifiers (`my_var-name`) select as
/// single units on double-click. Mirrors ghostty's default + the Mac
/// port's `WordSelection.defaultWordChars`.
pub const DEFAULT_EXTRA_WORD_CHARS: &str = "_-.+~/:@%";

/// Expand `col` outward to the surrounding word in `row`. Returns
/// `None` when the clicked cell itself is whitespace (the caller falls
/// through to single-cell selection — iTerm2 has a "select previous
/// word" alternative we deliberately don't implement; `None` is the
/// simpler, more predictable contract).
///
/// `col` is a 0-indexed `char` position; out-of-range values return
/// `None`.
pub fn expand_word(row: &str, col: u16, extra_word_chars: &str) -> Option<WordSpan> {
    let chars: Vec<char> = row.chars().collect();
    let idx = col as usize;
    if idx >= chars.len() {
        return None;
    }
    if !is_word_char(chars[idx], extra_word_chars) {
        return None;
    }
    let mut c0 = idx;
    while c0 > 0 && is_word_char(chars[c0 - 1], extra_word_chars) {
        c0 -= 1;
    }
    let mut c1 = idx;
    while c1 + 1 < chars.len() && is_word_char(chars[c1 + 1], extra_word_chars) {
        c1 += 1;
    }
    Some(WordSpan {
        col0: c0 as u16,
        col1: c1 as u16,
    })
}

/// Full visible-row span for triple-click. Returns `(0,
/// last_non_blank_col)` where blank = ASCII 0x20. A fully-blank row
/// returns `(0, 0)` — degenerate but well-defined; the caller clamps
/// it to a single-cell highlight, same as ghostty's triple-click on
/// an empty row.
pub fn expand_line(row: &str) -> WordSpan {
    let chars: Vec<char> = row.chars().collect();
    if chars.is_empty() {
        return WordSpan { col0: 0, col1: 0 };
    }
    let mut idx = chars.len() - 1;
    while idx > 0 && chars[idx] == ' ' {
        idx -= 1;
    }
    WordSpan {
        col0: 0,
        col1: idx as u16,
    }
}

/// A `char` is a "word char" if it's a Unicode letter, a Unicode
/// digit, or appears in `extras`. Matches the Swift port's
/// `isWordScalar` (Unicode.Scalar.properties.isAlphabetic +
/// generalCategory checks for Nd/Nl/No).
fn is_word_char(c: char, extras: &str) -> bool {
    if extras.contains(c) {
        return true;
    }
    c.is_alphabetic() || c.is_numeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mid_word_selects_word() {
        let span = expand_word("hello world here", 8, DEFAULT_EXTRA_WORD_CHARS);
        assert_eq!(span, Some(WordSpan { col0: 6, col1: 10 }));
    }

    #[test]
    fn whitespace_returns_none() {
        let span = expand_word("hello world here", 5, DEFAULT_EXTRA_WORD_CHARS);
        assert_eq!(span, None);
    }

    #[test]
    fn path_selects_whole_path() {
        let span = expand_word("see /tmp/foo.txt today", 7, DEFAULT_EXTRA_WORD_CHARS);
        assert_eq!(span, Some(WordSpan { col0: 4, col1: 15 }));
    }

    #[test]
    fn url_selects_whole_url() {
        let span = expand_word(
            "visit https://example.com today",
            10,
            DEFAULT_EXTRA_WORD_CHARS,
        );
        assert_eq!(span, Some(WordSpan { col0: 6, col1: 24 }));
    }

    #[test]
    fn custom_break_chars_splits_path() {
        let span = expand_word("see /tmp/foo.txt today", 7, "_-+~:@%");
        assert_eq!(span, Some(WordSpan { col0: 5, col1: 7 }));
    }

    #[test]
    fn unicode_word_pins_codepoint_indexing() {
        // `ï` is U+00EF (one codepoint), so `naïve` is 5 codepoints.
        let span = expand_word("naïve approach", 1, DEFAULT_EXTRA_WORD_CHARS);
        assert_eq!(span, Some(WordSpan { col0: 0, col1: 4 }));
    }

    #[test]
    fn boundary_click_word_side_wins() {
        // Click on the last word char — algorithm walks left into
        // the word.
        let span = expand_word("foo bar baz", 2, DEFAULT_EXTRA_WORD_CHARS);
        assert_eq!(span, Some(WordSpan { col0: 0, col1: 2 }));
    }

    #[test]
    fn identifier_with_underscore_stays_whole() {
        let span = expand_word("result = my_var_name.field", 12, DEFAULT_EXTRA_WORD_CHARS);
        assert_eq!(span, Some(WordSpan { col0: 9, col1: 25 }));
    }

    #[test]
    fn out_of_range_returns_none() {
        assert_eq!(expand_word("hi", 2, DEFAULT_EXTRA_WORD_CHARS), None);
        assert_eq!(expand_word("", 0, DEFAULT_EXTRA_WORD_CHARS), None);
    }

    #[test]
    fn full_row_with_trailing_blanks_trimmed() {
        // 5 trailing spaces — expand_line trims them off.
        let span = expand_line("hello world here     ");
        assert_eq!(span, WordSpan { col0: 0, col1: 15 });
    }

    #[test]
    fn single_word_row() {
        let span = expand_line("hello");
        assert_eq!(span, WordSpan { col0: 0, col1: 4 });
    }

    #[test]
    fn fully_blank_row_degenerates_to_0_0() {
        let span = expand_line("      ");
        assert_eq!(span, WordSpan { col0: 0, col1: 0 });
    }

    #[test]
    fn empty_row() {
        let span = expand_line("");
        assert_eq!(span, WordSpan { col0: 0, col1: 0 });
    }

    #[test]
    fn unicode_digits_count_as_word() {
        // Arabic-Indic digits are Unicode digit → word chars.
        let span = expand_word("foo ١٢٣ bar", 4, DEFAULT_EXTRA_WORD_CHARS);
        assert_eq!(span, Some(WordSpan { col0: 4, col1: 6 }));
    }
}
