//! Shared URL detection for clickable-link support in both Roost UIs.
//!
//! The Linux (GTK) UI consumes this directly; the Swift Mac UI
//! re-implements the same regex + trim pipeline by hand and pins parity
//! against the shared corpus in `tests/url-fixtures/`. One contract,
//! two implementations — same shape as `roost-osc` vs.
//! `mac/Sources/Roost/OscScanner.swift`.
//!
//! ## Detection is intentionally narrow
//!
//! Only well-known URL schemes match. Out of scope for v1:
//!
//! * Path detection (`./foo`, `~/bar`, `src/main.rs:42`).
//! * scp-style git remotes (`git@github.com:x/y`).
//! * IPv6 literal hosts (`[::1]:8080`).
//!
//! ghostty's URL regex (`../ghostty/src/config/url.zig`) covers all of
//! these and is the future widening target; the narrow scheme regex
//! here is the v1 baseline.

use regex::Regex;
use std::sync::OnceLock;

/// One URL match within a terminal row. `col0` is the column of the
/// URL's first char (inclusive); `col1` is the column of the URL's
/// last char (inclusive). Both are char-position indices into the row
/// string the caller passed in — terminal cells map to chars 1:1 for
/// the renderer's `dumpText`-style row builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlSpan {
    pub col0: u16,
    pub col1: u16,
    pub url: String,
}

/// Compiled regex. `OnceLock` so the first call pays the build cost
/// and every subsequent call reuses the same matcher. The body's
/// negation class spells out `[\t\n\x0c\r ]` instead of `\s` because
/// Rust's `regex` and Swift's `NSRegularExpression` treat `\s` as
/// Unicode whitespace — the explicit ASCII-only class is the only way
/// to keep the Mac and Linux matchers in lockstep.
fn pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:(?:https?|ftp|file|ssh|git\+ssh)://|(?:mailto|tel|news):)[^ \t\r\n\x0c<>"'`\\]+"#,
        )
        .expect("url regex compiles")
    })
}

/// Returns the URL straddling column `col` in `row`, or `None` if no
/// URL covers that column. `col` is a 0-indexed char position into
/// `row` — out-of-range values return `None`.
///
/// The cleaned URL has trailing punctuation and unmatched brackets
/// stripped via [`trim_url`], and the returned `col0`/`col1` reflect
/// the trimmed span (so an underline drawn on those cells doesn't
/// extend past the last cell of the URL into the trailing period).
pub fn find_url_at(row: &str, col: u16) -> Option<UrlSpan> {
    let total_chars = row.chars().count();
    let col = col as usize;
    if total_chars == 0 || col >= total_chars {
        return None;
    }
    find_all_urls(row)
        .into_iter()
        .find(|span| col >= span.col0 as usize && col <= span.col1 as usize)
}

/// Find every URL in `row`, sorted left-to-right by `col0`. Used by
/// the renderer to underline every URL when the modifier is held with
/// no specific hover yet. Same pipeline as [`find_url_at`]: each match
/// goes through [`trim_url`] before its column span is computed.
pub fn find_all_urls(row: &str) -> Vec<UrlSpan> {
    // Map byte offsets in `row` back to char (column) indices. The
    // table holds the byte offset where each successive char starts,
    // plus a sentinel for the end-of-string byte length so end-of-
    // match lookups don't need a special case.
    let mut byte_to_char: Vec<usize> = Vec::with_capacity(row.len() + 1);
    for (byte_off, _) in row.char_indices() {
        byte_to_char.push(byte_off);
    }
    byte_to_char.push(row.len());

    let mut out = Vec::new();
    for m in pattern().find_iter(row) {
        let raw = m.as_str();
        let trimmed = trim_url(raw);
        if trimmed.is_empty() {
            continue;
        }
        let start_byte = m.start();
        let end_byte = start_byte + trimmed.len();
        let start_col = byte_to_char_index(&byte_to_char, start_byte);
        let end_col_exclusive = byte_to_char_index(&byte_to_char, end_byte);
        // end_col is inclusive.
        if end_col_exclusive == 0 || end_col_exclusive <= start_col {
            continue;
        }
        let end_col = end_col_exclusive - 1;
        let Ok(col0) = u16::try_from(start_col) else {
            continue;
        };
        let Ok(col1) = u16::try_from(end_col) else {
            continue;
        };
        out.push(UrlSpan {
            col0,
            col1,
            url: trimmed.to_string(),
        });
    }
    out
}

/// Map a byte offset within `row` back to its char (column) index
/// using the pre-built table. Linear scan is fine — terminal rows are
/// short (max ~hundreds of columns).
fn byte_to_char_index(table: &[usize], byte_off: usize) -> usize {
    for (i, &off) in table.iter().enumerate() {
        if off >= byte_off {
            return i;
        }
    }
    table.len().saturating_sub(1)
}

/// Strip trailing sentence punctuation and unmatched closing brackets
/// from a URL match: sentence punctuation almost never belongs to a
/// URL, and a trailing `)` is part of the URL only if there's a
/// matching `(` inside it
/// (so Wikipedia's `_(disambiguation)` survives but `(see foo)` peels).
///
/// Returned slice points back into the input; the caller wraps it in
/// `String` if it needs ownership.
pub fn trim_url(mut u: &str) -> &str {
    // Pass 1: strip trailing plain punctuation that's never part of a URL.
    while let Some(last) = u.as_bytes().last() {
        match last {
            b'.' | b',' | b';' | b':' | b'!' | b'?' => {
                u = &u[..u.len() - 1];
            }
            _ => break,
        }
    }
    // Pass 2: balance trailing unmatched closing brackets.
    while let Some(last) = u.as_bytes().last() {
        let (open, close) = match last {
            b')' => (b'(', b')'),
            b']' => (b'[', b']'),
            b'}' => (b'{', b'}'),
            _ => return u,
        };
        let opens = u.as_bytes().iter().filter(|&&c| c == open).count();
        let closes = u.as_bytes().iter().filter(|&&c| c == close).count();
        if closes > opens {
            u = &u[..u.len() - 1];
        } else {
            return u;
        }
    }
    u
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `find_url_at` against the shared URL fixture corpus. Any drift
    /// in these cases would mean a user-visible regression in
    /// URL-click behavior — keep them in step with the Mac UI's
    /// matcher when widening either side.
    #[test]
    fn find_at_matches_reference_corpus() {
        struct Case {
            name: &'static str,
            row: &'static str,
            col: u16,
            want_url: Option<&'static str>,
            want_col0: Option<u16>,
            want_col1: Option<u16>,
        }
        let cases = [
            Case {
                name: "bare https mid-string",
                row: "see https://example.com for details",
                col: 12,
                want_url: Some("https://example.com"),
                want_col0: Some(4),
                want_col1: Some(22),
            },
            Case {
                // "naïve " is 6 chars / 7 bytes (`ï` is 2 bytes).
                // Without correct byte→char indexing, the regex offsets
                // slip by one column from char 4 onward.
                name: "url after multi-byte prefix",
                row: "naïve https://example.com",
                col: 10,
                want_url: Some("https://example.com"),
                want_col0: Some(6),
                want_col1: Some(24),
            },
            Case {
                name: "github PR url",
                row: "Created PR https://github.com/charliek/roost/pull/42",
                col: 30,
                want_url: Some("https://github.com/charliek/roost/pull/42"),
                want_col0: Some(11),
                want_col1: Some(51),
            },
            Case {
                name: "trailing period stripped",
                row: "Visit https://example.com.",
                col: 10,
                want_url: Some("https://example.com"),
                want_col0: None,
                want_col1: None,
            },
            Case {
                name: "trailing comma stripped",
                row: "links: https://a.test, https://b.test",
                col: 10,
                want_url: Some("https://a.test"),
                want_col0: None,
                want_col1: None,
            },
            Case {
                name: "wikipedia parenthesized url kept whole",
                row: "see https://en.wikipedia.org/wiki/Rust_(programming_language) here",
                col: 20,
                want_url: Some("https://en.wikipedia.org/wiki/Rust_(programming_language)"),
                want_col0: None,
                want_col1: None,
            },
            Case {
                name: "url inside parens drops trailing close-paren",
                row: "see (https://example.com) here",
                col: 10,
                want_url: Some("https://example.com"),
                want_col0: None,
                want_col1: None,
            },
            Case {
                name: "mailto scheme",
                row: "mail mailto:a@b.com please",
                col: 8,
                want_url: Some("mailto:a@b.com"),
                want_col0: None,
                want_col1: None,
            },
            Case {
                name: "file uri",
                row: "open file:///tmp/foo.txt now",
                col: 10,
                want_url: Some("file:///tmp/foo.txt"),
                want_col0: None,
                want_col1: None,
            },
            Case {
                name: "no scheme = no match",
                row: "this is not.a.url at all",
                col: 10,
                want_url: None,
                want_col0: None,
                want_col1: None,
            },
            Case {
                name: "scp git remote not matched",
                row: "remote git@github.com:x/y.git origin",
                col: 14,
                want_url: None,
                want_col0: None,
                want_col1: None,
            },
            Case {
                name: "col outside any match",
                row: "see https://a.test here",
                col: 20,
                want_url: None,
                want_col0: None,
                want_col1: None,
            },
        ];
        for c in &cases {
            let got = find_url_at(c.row, c.col);
            match (c.want_url, &got) {
                (None, None) => {}
                (None, Some(g)) => panic!("[{}] expected no match, got {g:?}", c.name),
                (Some(_), None) => panic!("[{}] expected match, got none", c.name),
                (Some(want), Some(g)) => {
                    assert_eq!(g.url, want, "[{}] url", c.name);
                    if let Some(want_col0) = c.want_col0 {
                        assert_eq!(g.col0, want_col0, "[{}] col0", c.name);
                    }
                    if let Some(want_col1) = c.want_col1 {
                        assert_eq!(g.col1, want_col1, "[{}] col1", c.name);
                    }
                }
            }
        }
    }

    /// `trim_url` against the shared URL fixture corpus. Keep in step
    /// with the Mac UI's trim pipeline.
    #[test]
    fn trim_url_matches_reference_corpus() {
        let cases: &[(&str, &str)] = &[
            ("https://x.test", "https://x.test"),
            ("https://x.test.", "https://x.test"),
            ("https://x.test,", "https://x.test"),
            ("https://x.test);", "https://x.test"),
            ("https://w.org/Rust_(lang)", "https://w.org/Rust_(lang)"),
            ("https://w.org/Rust_(lang).", "https://w.org/Rust_(lang)"),
            ("https://x.test])", "https://x.test"),
            ("https://x.test/(a)b", "https://x.test/(a)b"),
        ];
        for (input, want) in cases {
            assert_eq!(trim_url(input), *want, "trim_url({input:?})");
        }
    }

    /// Unicode codepoints in the URL body match as-is — we don't do
    /// IDNA, we just respect the regex's `[^\s<>"'`\\]+` body class.
    /// Trailing fullwidth period (U+3002) does NOT trim — the trim
    /// pass only strips ASCII punctuation, so neither UI surprises
    /// users with locale-dependent cutoff.
    #[test]
    fn unicode_url_body_matches_codepoints() {
        let row = "open https://例え.テスト/path here";
        let span = find_url_at(row, 7).expect("unicode URL match");
        assert_eq!(span.url, "https://例え.テスト/path");
    }

    #[test]
    fn fullwidth_trailing_punctuation_not_stripped() {
        // U+3002 IDEOGRAPHIC FULL STOP — not in the ASCII strip set,
        // so it stays attached.
        let row = "see https://例え.テスト。 next";
        let span = find_url_at(row, 7).expect("unicode trail match");
        assert!(
            span.url.ends_with("。"),
            "expected fullwidth period to stay attached, got {:?}",
            span.url
        );
    }

    #[test]
    fn find_all_urls_returns_left_to_right() {
        let row = "a https://one.test b https://two.test";
        let all = find_all_urls(row);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].url, "https://one.test");
        assert_eq!(all[1].url, "https://two.test");
        assert!(all[0].col0 < all[1].col0);
    }
}

// ============================================================================
// Wire-vector round-trip
// ============================================================================
//
// `tests/url-fixtures/*.txt` files at the workspace root carry the
// canonical (row, col) → (col0, col1, url) corpus. Both this crate
// (Rust) and `mac/Tests/RoostTests/UrlFixtureRoundTripTests.swift`
// (Swift) load the same files; drift between the two ports surfaces
// here. Same pattern as `tests/ipc-vectors/`.

#[cfg(test)]
mod fixtures {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixtures_dir() -> PathBuf {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(p.pop()); // pop "roost-url"
        assert!(p.pop()); // pop "crates"
        p.push("tests");
        p.push("url-fixtures");
        p
    }

    /// Parse one fixture file. Format:
    /// ```text
    /// row: <row text>
    /// col: <column number>
    /// ---
    /// col0: <expected col0>      (omit on negative cases)
    /// col1: <expected col1>
    /// url: <expected url>
    /// ```
    /// `---` separates the input header from the expected output. A
    /// fixture with no body after `---` asserts "no match" for that
    /// (row, col). Lines starting with `#` are comments and ignored.
    struct Fixture {
        name: String,
        row: String,
        col: u16,
        want: Option<(u16, u16, String)>,
    }

    fn parse(path: &Path) -> Fixture {
        let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let mut row: Option<String> = None;
        let mut col: Option<u16> = None;
        let mut col0: Option<u16> = None;
        let mut col1: Option<u16> = None;
        let mut url: Option<String> = None;
        let mut after_sep = false;
        for line in raw.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if line == "---" {
                after_sep = true;
                continue;
            }
            let (key, value) = match line.split_once(": ") {
                Some(p) => p,
                None => continue,
            };
            match key {
                "row" if !after_sep => row = Some(value.to_string()),
                "col" if !after_sep => col = Some(value.parse().expect("col is u16")),
                "col0" if after_sep => col0 = Some(value.parse().expect("col0 is u16")),
                "col1" if after_sep => col1 = Some(value.parse().expect("col1 is u16")),
                "url" if after_sep => url = Some(value.to_string()),
                _ => {}
            }
        }
        // All-three or none — partial blocks indicate a fixture typo,
        // not "no match", so refuse to interpret them silently. The
        // Swift loader does the same.
        let want = match (col0, col1, url) {
            (Some(c0), Some(c1), Some(u)) => Some((c0, c1, u)),
            (None, None, None) => None,
            (col0_v, col1_v, url_v) => panic!(
                "fixture {path:?}: partial expected block (col0={col0_v:?}, col1={col1_v:?}, url={url_v:?}); \
                 either supply all three fields or none"
            ),
        };
        Fixture {
            name,
            row: row.expect("row missing"),
            col: col.expect("col missing"),
            want,
        }
    }

    #[test]
    fn every_fixture_round_trips() {
        let dir = fixtures_dir();
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {dir:?}: {e}"))
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("txt"))
            .collect();
        entries.sort_by_key(|e| e.path());
        assert!(!entries.is_empty(), "no fixtures found in {dir:?}");
        let mut failures: Vec<String> = vec![];
        for entry in entries {
            let f = parse(&entry.path());
            let got = find_url_at(&f.row, f.col);
            match (&f.want, &got) {
                (None, None) => {}
                (None, Some(g)) => {
                    failures.push(format!("[{}] expected no match, got {g:?}", f.name))
                }
                (Some(_), None) => failures.push(format!("[{}] expected match, got none", f.name)),
                (Some((c0, c1, u)), Some(g)) => {
                    if g.col0 != *c0 || g.col1 != *c1 || g.url != *u {
                        failures.push(format!(
                            "[{}] mismatch: got col0={} col1={} url={:?}, want col0={} col1={} url={:?}",
                            f.name, g.col0, g.col1, g.url, c0, c1, u
                        ));
                    }
                }
            }
        }
        if !failures.is_empty() {
            panic!("fixture failures:\n{}", failures.join("\n"));
        }
    }
}
