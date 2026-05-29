//! Load every `tests/word-fixtures/*.txt` and assert byte-exact
//! agreement with the Rust `word_selection` port. The Swift loader in
//! `mac/Tests/RoostTests/WordFixtureRoundTripTests.swift` runs against
//! the same files; drift between the two ports surfaces as a failure
//! on whichever side regressed.
//!
//! Same shape as `roost-url`'s in-crate `fixtures` mod.
//!
//! Integration-test form (in `tests/`) because the path walk goes from
//! `CARGO_MANIFEST_DIR` up to the workspace `tests/` directory and
//! that's cleaner as an explicit fixture-loader test than a unit-test
//! mod inside `lib.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use roost_linux::word_selection::{expand_line, expand_word, WordSpan, DEFAULT_EXTRA_WORD_CHARS};

fn fixtures_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(p.pop()); // pop "roost-linux"
    assert!(p.pop()); // pop "crates"
    p.push("tests");
    p.push("word-fixtures");
    p
}

struct Fixture {
    name: String,
    row: String,
    col: u16,
    break_chars: String,
    click_count: u8,
    want: Option<(u16, u16, String)>,
}

/// Parse one fixture file. Format documented in
/// `tests/word-fixtures/README.md`. Trailing whitespace on `row:` and
/// `text:` lines is intentionally preserved — fixture 07 has 5 literal
/// trailing spaces on `row:` that the `expand_line` trim has to peel.
fn parse(path: &Path) -> Fixture {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let mut row: Option<String> = None;
    let mut col: Option<u16> = None;
    let mut break_chars: Option<String> = None;
    let mut click_count: Option<u8> = None;
    let mut col0: Option<u16> = None;
    let mut col1: Option<u16> = None;
    let mut text: Option<String> = None;
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
            "col" if !after_sep => col = Some(value.trim().parse().expect("col is u16")),
            "break_chars" if !after_sep => break_chars = Some(value.trim().to_string()),
            "click_count" if !after_sep => {
                click_count = Some(value.trim().parse().expect("click_count is u8"))
            }
            "col0" if after_sep => col0 = Some(value.trim().parse().expect("col0 is u16")),
            "col1" if after_sep => col1 = Some(value.trim().parse().expect("col1 is u16")),
            "text" if after_sep => text = Some(value.to_string()),
            _ => {}
        }
    }
    let want = match (col0, col1, text) {
        (Some(c0), Some(c1), Some(t)) => Some((c0, c1, t)),
        (None, None, None) => None,
        (col0_v, col1_v, text_v) => panic!(
            "fixture {path:?}: partial expected block (col0={col0_v:?}, col1={col1_v:?}, text={text_v:?}); \
             either supply all three fields or none"
        ),
    };
    Fixture {
        name,
        row: row.unwrap_or_default(),
        col: col.unwrap_or(0),
        break_chars: break_chars.unwrap_or_else(|| DEFAULT_EXTRA_WORD_CHARS.to_string()),
        click_count: click_count.unwrap_or(2),
        want,
    }
}

/// Slice chars `[c0, c1]` inclusive from `row`. The expected `text` in
/// a fixture is the substring the span covers, so we re-derive it here
/// for the diagnostic message + the equality assertion.
fn slice_chars(row: &str, c0: u16, c1: u16) -> String {
    let chars: Vec<char> = row.chars().collect();
    let (a, b) = (c0 as usize, c1 as usize);
    if a >= chars.len() || b >= chars.len() || a > b {
        return String::new();
    }
    chars[a..=b].iter().collect()
}

#[test]
fn every_word_fixture_round_trips() {
    let dir = fixtures_dir();
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"))
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.extension().and_then(|s| s.to_str()) == Some("txt") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "no fixtures in {dir:?} — did the repo layout change?"
    );
    let mut failures: Vec<String> = Vec::new();
    for path in &entries {
        let fix = parse(path);
        let (got, got_text) = match fix.click_count {
            2 => {
                let span = expand_word(&fix.row, fix.col, &fix.break_chars);
                let text = span.map(|s| slice_chars(&fix.row, s.col0, s.col1));
                (span, text)
            }
            3 => {
                let span = expand_line(&fix.row);
                let text = slice_chars(&fix.row, span.col0, span.col1);
                (Some(span), Some(text))
            }
            other => {
                failures.push(format!("[{}] invalid click_count {other}", fix.name));
                continue;
            }
        };
        match (&fix.want, got) {
            (None, None) => continue,
            (None, Some(g)) => failures.push(format!(
                "[{}] expected no match, got {:?} (text=\"{}\")",
                fix.name,
                g,
                got_text.unwrap_or_default()
            )),
            (Some(_), None) => failures.push(format!("[{}] expected match, got None", fix.name)),
            (Some((c0, c1, t)), Some(g)) => {
                if g.col0 != *c0 || g.col1 != *c1 {
                    failures.push(format!(
                        "[{}] span mismatch: got col0={} col1={} text=\"{}\", want col0={} col1={} text=\"{}\"",
                        fix.name,
                        g.col0,
                        g.col1,
                        got_text.clone().unwrap_or_default(),
                        c0,
                        c1,
                        t
                    ));
                } else if let Some(gt) = &got_text {
                    if gt != t {
                        failures.push(format!(
                            "[{}] text mismatch: got \"{}\", want \"{}\"",
                            fix.name, gt, t
                        ));
                    }
                }
            }
        }
        // Silence the unused-field warning — `WordSpan` derives Eq so
        // comparisons elsewhere keep it useful.
        let _ = WordSpan { col0: 0, col1: 0 };
    }
    assert!(
        failures.is_empty(),
        "word-fixture round-trip failures:\n{}",
        failures.join("\n")
    );
}
