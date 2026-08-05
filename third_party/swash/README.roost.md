# Vendored swash (roost)

Pristine [swash](https://crates.io/crates/swash) 0.2.10 from crates.io, plus
exactly one delta:

* `src/internal/xmtx.rs::advance` — a guard returning `0` when
  `long_metric_count == 0`. Without it, a font whose `hhea`/`vhea` declares
  zero long metrics underflows `(long_metric_count - 1)` and SIGABRTs debug
  builds (issue #292).

Wired in via `[patch.crates-io]` in the workspace root `Cargo.toml`.
Authoritative rationale: `CLAUDE.md` § Library preferences.

**Removal condition.** Delete `third_party/swash/` and the
`[patch.crates-io]` entry once a published swash release ships an equivalent
guard. Cargo enforces re-evaluation on its own: the patch is pinned to
0.2.10, so any future dependency bump that requires a newer swash fails
resolution with a patch version mismatch.

Regression test: `crates/roost-iced/tests/swash_zero_metrics_test.rs`.
