// In-language unit tests for the WordSelection algorithm. The
// shared `tests/word-fixtures/` corpus is the cross-port parity pin
// (see `WordFixtureRoundTripTests.swift`); these cases mirror the
// fixtures one-for-one so a failure here points at the same
// scenario with a sharper line + file location.
//
// Same shape as `UrlDetectionTests.swift` (paired with
// `UrlFixtureRoundTripTests.swift`).

import Testing

@testable import Roost

@Test
func expandWord_midWordSelectsWord() {
    let span = WordSelection.expandWord(in: "hello world here", at: 8)
    #expect(span == WordSpan(col0: 6, col1: 10))
}

@Test
func expandWord_onWhitespaceReturnsNil() {
    // Clicking on a blank cell falls through to single-cell select
    // rather than "select previous word" (the iTerm2 alternative we
    // don't take).
    let span = WordSelection.expandWord(in: "hello world here", at: 5)
    #expect(span == nil)
}

@Test
func expandWord_insidePathSelectsWholePath() {
    let span = WordSelection.expandWord(in: "see /tmp/foo.txt today", at: 7)
    #expect(span == WordSpan(col0: 4, col1: 15))
}

@Test
func expandWord_insideUrlSelectsWholeUrl() {
    let span = WordSelection.expandWord(in: "visit https://example.com today", at: 10)
    #expect(span == WordSpan(col0: 6, col1: 24))
}

@Test
func expandWord_customBreakCharsSplitsPath() {
    // Drop `/` and `.` from the extras — file paths now split into
    // their segments on double-click. Documents the lever a user can
    // pull in their config.
    let span = WordSelection.expandWord(
        in: "see /tmp/foo.txt today",
        at: 7,
        extraWordChars: "_-+~:@%"
    )
    #expect(span == WordSpan(col0: 5, col1: 7))
}

@Test
func expandWord_unicodeWordPinsScalarIndexing() {
    // `ï` is U+00EF (one scalar), so `naïve` is 5 scalars / cells.
    // The renderer's row build is per-scalar.
    let span = WordSelection.expandWord(in: "naïve approach", at: 1)
    #expect(span == WordSpan(col0: 0, col1: 4))
}

@Test
func expandWord_boundaryClicksWordSideWins() {
    // Click on the last word char — algorithm walks left into the
    // word and returns the word, not nil.
    let span = WordSelection.expandWord(in: "foo bar baz", at: 2)
    #expect(span == WordSpan(col0: 0, col1: 2))
}

@Test
func expandWord_identifierWithUnderscoreStaysWhole() {
    let span = WordSelection.expandWord(in: "result = my_var_name.field", at: 12)
    #expect(span == WordSpan(col0: 9, col1: 25))
}

@Test
func expandWord_outOfRangeReturnsNil() {
    #expect(WordSelection.expandWord(in: "hi", at: -1) == nil)
    #expect(WordSelection.expandWord(in: "hi", at: 2) == nil)
    #expect(WordSelection.expandWord(in: "", at: 0) == nil)
}

@Test
func expandLine_fullRowWithTrailingBlanks() {
    // 5 trailing spaces — expandLine trims them off.
    let span = WordSelection.expandLine(in: "hello world here     ")
    #expect(span == WordSpan(col0: 0, col1: 15))
}

@Test
func expandLine_singleWordRow() {
    let span = WordSelection.expandLine(in: "hello")
    #expect(span == WordSpan(col0: 0, col1: 4))
}

@Test
func expandLine_fullyBlankRowDegeneratesTo00() {
    // Trailing-blank trim falls back to (0, 0) on a row that's all
    // spaces. The caller clamps to a single-cell highlight.
    let span = WordSelection.expandLine(in: "      ")
    #expect(span == WordSpan(col0: 0, col1: 0))
}

@Test
func expandLine_emptyRow() {
    let span = WordSelection.expandLine(in: "")
    #expect(span == WordSpan(col0: 0, col1: 0))
}

@Test
func expandWord_unicodeDigitsCountAsWord() {
    // Arabic-Indic digits are Unicode digit (category Nd) → word
    // chars. Pins that we don't accidentally only accept ASCII.
    let span = WordSelection.expandWord(in: "foo ١٢٣ bar", at: 4)
    #expect(span == WordSpan(col0: 4, col1: 6))
}
