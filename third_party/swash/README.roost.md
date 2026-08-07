# Vendored swash (roost)

Pristine [swash](https://crates.io/crates/swash) 0.2.10 from crates.io, plus
the deltas enumerated below. 0.2.10 is the latest published swash release as
of 2026-08-06 (checked via the crates.io API), so none of these have an
upstream version to move to.

* `src/internal/xmtx.rs::advance` — a guard returning `0` when
  `long_metric_count == 0`. Without it, a font whose `hhea`/`vhea` declares
  zero long metrics underflows `(long_metric_count - 1)` and SIGABRTs debug
  builds (issue #292).
* `src/strike.rs::get_location` — the format-4 bitmap-index bisection stepped
  with `l = i + i` instead of `l = i + 1`, so any probe at `i == 0` left `l`
  unchanged and spun the calling thread forever on a malformed or crafted
  EBLC/CBLC (issue #299). Left deliberately unfixed alongside it: the record
  offset in the same loop is off by one slot (`rec = base + i * 4`, where
  format 4's records begin at `base + 4`, past the u32 `numGlyphs`). These
  deltas are safety fixes only — lookup results stay as upstream computes
  them.
* `src/internal/var.rs` — three subtraction guards for malformed variation
  tables, each of which SIGABRTs debug builds and wraps harmlessly in release
  (issue #299): `Fvar::get_instance` underflows `inst_size - 2` when `fvar`
  declares an `instanceSize` below 2, `item_delta` underflows
  `count - short_count` when an item variation data subtable declares
  `shortDeltaCount` greater than `regionIndexCount`, and `metric_delta`
  underflows `count - 1` when an `HVAR`/`VVAR` delta set index map declares
  `mapCount == 0`.
* `src/string.rs::Chars::next` — a bounds-checked read in the MacRoman arm,
  which indexed its slice directly and so panicked in **both** profiles
  (issue #299). `chars()` takes its length from the name record's declared
  string length but falls back to an empty slice when that length overruns the
  table's storage area, so any font with such a record crashed on the first
  character. The iterator now ends there instead.

Wired in via `[patch.crates-io]` in the workspace root `Cargo.toml`.
Authoritative rationale: `CLAUDE.md` § Library preferences.

**Removal condition.** Delete `third_party/swash/` and the
`[patch.crates-io]` entry once a published swash release ships equivalent
guards for every delta listed above. Note cargo alone does NOT enforce this:
a `[patch.crates-io]` entry only shadows the version it matches, so a future
dependency bump requiring a newer swash would resolve the unpatched registry
release with just a warning. The enforcement is `make check-iced`'s
assertion (Makefile) that `cargo tree -p roost-iced` resolves swash 0.2.10
to `third_party/swash` — it fails the build (and CI) if the patch ever stops
applying.

Regression tests: `crates/roost-iced/tests/swash_zero_metrics_test.rs` and
`crates/roost-iced/tests/swash_malformed_font_test.rs`.
