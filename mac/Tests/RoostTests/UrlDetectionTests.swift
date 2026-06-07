// Swift companion to the Rust suite in `crates/roost-url/src/lib.rs`.
// Every case here mirrors one in that suite so a future drift between
// the two UIs surfaces as a test failure on the Mac side. The shared
// wire vectors in `tests/url-fixtures/` are loaded by a separate test
// file (UrlFixtureRoundTripTests.swift) — this file is the
// in-language readability layer.

import Testing

@testable import Roost

@Test
func findAt_bareHttpsInMidString() {
    let row = "see https://example.com for details"
    let span = UrlDetection.find(in: row, at: 12)
    #expect(span?.url == "https://example.com")
    #expect(span?.col0 == 4)
    #expect(span?.col1 == 22)
}

@Test
func findAt_urlAfterMultibytePrefix() {
    // "naïve " — `ï` is a 2-byte UTF-8 codepoint. The byte→char index
    // table must keep the URL's column anchored at the right column.
    let row = "naïve https://example.com"
    let span = UrlDetection.find(in: row, at: 10)
    #expect(span?.url == "https://example.com")
    #expect(span?.col0 == 6)
    #expect(span?.col1 == 24)
}

@Test
func findAt_githubPRUrl() {
    let row = "Created PR https://github.com/charliek/roost/pull/42"
    let span = UrlDetection.find(in: row, at: 30)
    #expect(span?.url == "https://github.com/charliek/roost/pull/42")
    #expect(span?.col0 == 11)
    #expect(span?.col1 == 51)
}

@Test
func findAt_trailingPeriodStripped() {
    let row = "Visit https://example.com."
    let span = UrlDetection.find(in: row, at: 10)
    #expect(span?.url == "https://example.com")
}

@Test
func findAt_wikipediaParenthesizedUrlKeptWhole() {
    let row = "see https://en.wikipedia.org/wiki/Rust_(programming_language) here"
    let span = UrlDetection.find(in: row, at: 20)
    #expect(span?.url == "https://en.wikipedia.org/wiki/Rust_(programming_language)")
}

@Test
func findAt_urlInsideParensDropsTrailingClose() {
    let row = "see (https://example.com) here"
    let span = UrlDetection.find(in: row, at: 10)
    #expect(span?.url == "https://example.com")
}

@Test
func findAt_mailtoScheme() {
    let row = "mail mailto:a@b.com please"
    let span = UrlDetection.find(in: row, at: 8)
    #expect(span?.url == "mailto:a@b.com")
}

@Test
func findAt_fileUri() {
    let row = "open file:///tmp/foo.txt now"
    let span = UrlDetection.find(in: row, at: 10)
    #expect(span?.url == "file:///tmp/foo.txt")
}

@Test
func findAt_noSchemeNoMatch() {
    let row = "this is not.a.url at all"
    #expect(UrlDetection.find(in: row, at: 10) == nil)
}

@Test
func findAt_scpGitRemoteNoMatch() {
    let row = "remote git@github.com:x/y.git origin"
    #expect(UrlDetection.find(in: row, at: 14) == nil)
}

@Test
func findAt_colOutsideAnyMatchNoResult() {
    let row = "see https://a.test here"
    #expect(UrlDetection.find(in: row, at: 20) == nil)
}

@Test
func trimURL_byteExactWithLegacyGo() {
    let cases: [(String, String)] = [
        ("https://x.test",             "https://x.test"),
        ("https://x.test.",            "https://x.test"),
        ("https://x.test,",            "https://x.test"),
        ("https://x.test);",           "https://x.test"),
        ("https://w.org/Rust_(lang)",  "https://w.org/Rust_(lang)"),
        ("https://w.org/Rust_(lang).", "https://w.org/Rust_(lang)"),
        ("https://x.test])",           "https://x.test"),
        ("https://x.test/(a)b",        "https://x.test/(a)b"),
    ]
    for (input, want) in cases {
        #expect(UrlDetection.trimURL(input) == want, "trimURL(\(input))")
    }
}

@Test
func unicodeUrlBodyMatchesCodepoints() {
    let row = "open https://例え.テスト/path here"
    let span = UrlDetection.find(in: row, at: 7)
    #expect(span?.url == "https://例え.テスト/path")
}

@Test
func fullwidthTrailingPunctuationNotStripped() {
    // U+3002 IDEOGRAPHIC FULL STOP — not in the ASCII strip set, so
    // it stays attached. Both UIs behave the same way so neither
    // surprises users with locale-dependent cutoff.
    let row = "see https://例え.テスト。 next"
    let span = UrlDetection.find(in: row, at: 7)
    #expect(span?.url.hasSuffix("。") == true)
}

@Test
func findAll_returnsLeftToRight() {
    let row = "a https://one.test b https://two.test"
    let all = UrlDetection.findAll(in: row)
    #expect(all.count == 2)
    #expect(all[0].url == "https://one.test")
    #expect(all[1].url == "https://two.test")
    #expect(all[0].col0 < all[1].col0)
}
