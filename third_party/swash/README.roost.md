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

Wired in via `[patch.crates-io]` in the workspace root `Cargo.toml`.
Authoritative rationale: `CLAUDE.md` § Library preferences.

**Removal condition.** Delete `third_party/swash/` and the
`[patch.crates-io]` entry once a published swash release ships equivalent
guards for every delta listed above. Cargo enforces re-evaluation on its own:
the patch is pinned to 0.2.10, so any future dependency bump that requires a
newer swash fails resolution with a patch version mismatch.

Regression tests: `crates/roost-iced/tests/swash_zero_metrics_test.rs` and
`crates/roost-iced/tests/swash_malformed_font_test.rs`.
