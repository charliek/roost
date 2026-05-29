# URL detection golden fixtures

Canonical `(row, col) → UrlSpan` corpus for the URL detection logic in
`crates/roost-url/` (Rust) and `mac/Sources/Roost/UrlDetection.swift`
(Swift). Both ports load these files and assert byte-exact equality;
drift between the two implementations surfaces here.

Same pattern as [`tests/ipc-vectors/`](../ipc-vectors/README.md).

## Format

```
row: <row text>
col: <column number>
---
col0: <expected col0>
col1: <expected col1>
url: <expected url>
```

- The `---` separator splits input from expected output.
- An empty body (no lines after `---`) asserts "no match at that column".
- Lines beginning with `#` are comments.
- `col0` is inclusive, `col1` is inclusive — matching Go's `Span.Col0`
  / `Span.Col1` for byte-exact parity with the legacy binary.
- Columns are 0-indexed char (codepoint) positions into the row text,
  not byte offsets. Multi-byte runes count as one column each, since
  one column = one terminal cell in the renderer's `dumpText`-style
  row builds.

When adding a fixture, mirror it in the existing tests too:

- Rust unit test `find_url_at_matches_legacy_go_corpus` already covers
  the same scenarios for in-language coverage; the fixture-loader test
  is the cross-port pin.
- The Swift side picks up new fixtures automatically — the loader is
  schema-agnostic.
