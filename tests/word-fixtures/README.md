# Word / line selection golden fixtures

Canonical `(row, col, click_count, break_chars) → WordSpan` corpus for
the double-/triple-click selection algorithm in
`mac/Sources/Roost/WordSelection.swift` (Swift) and
`crates/roost-linux/src/word_selection.rs` (Rust). Both ports load
these files and assert byte-exact equality — drift between the two
implementations surfaces here.

Same pattern as [`tests/url-fixtures/`](../url-fixtures/README.md) and
[`tests/ipc-vectors/`](../ipc-vectors/README.md).

## Format

```
row: <row text>
col: <column number>
break_chars: <extra word-char set>
click_count: <2 or 3>
---
col0: <expected col0>
col1: <expected col1>
text: <expected selected text>
```

- The `---` separator splits input from expected output.
- An empty body (no lines after `---`) asserts "no match at that
  column" (only meaningful for double-click; `expand_line` always
  returns a span).
- Lines beginning with `#` are comments.
- `col0` and `col1` are both inclusive 0-indexed scalar (codepoint)
  positions into `row`. Multi-byte runes count as one column each,
  since one column = one terminal cell in the renderer's
  `dump_text`-style row builds. Combining marks (`e\u{0301}`) count
  as 2 scalars — same as Rust's `chars()` and the URL fixture loader.
- `break_chars` lists the chars treated as word chars beyond Unicode
  letters/digits — Ghostty's default (`_-.+~/:@%`) is
  counter-intuitive in name but the right behavior: `/` and `.` STAY
  inside the word so file paths and URLs select as one unit.
- `click_count` is `2` (double-click → word) or `3` (triple-click →
  line). Triple-click ignores `col` and `break_chars`.

When adding a fixture, mirror it in the in-language tests too:

- Swift `WordSelectionTests.swift` and Rust `word_selection::tests`
  already cover the same scenarios for readability; the fixture
  loader on each side is the cross-port pin.
