# Phase 9: Cutover to main

**Status**: ✅ complete (2026-05-23).
**Mergeability to main**: done — merged via a `--no-ff` merge commit.

This is the milestone where the Rust + Swift port became the official direction
on `main`. It is **not** the Go deletion — that is pulled out into a separate,
deliberately-not-numbered phase: [`GODELETE.md`](GODELETE.md), gated on parity
testing + sign-off.

## What landed

* **Merged `feature/rust-port` → `main`** (merge commit, full 110-commit refactor
  history preserved). `main` is now the primary branch; `feature/rust-port` is
  retired.
* **CI retooled rust-primary** ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)):
  one consolidated workflow (rust-lint, rust-build, swift-mac, gtk-build) gated by
  a `dorny/paths-filter` `changes` job, with a single `ci-success` aggregated
  required check. The legacy Go CI moved to
  [`go-legacy.yml`](../.github/workflows/go-legacy.yml), path-filtered to Go files
  (runs only when Go code changes; not a required check). `refactor.yml` deleted.
* **Actions un-pinned from SHAs → major version tags** (matching the shed/prox
  convention) and bumped to Node 24 — clears the Node 20 runner deprecation.
* **Docs reoriented** to lead with the Rust/Swift product; the Go implementation
  is retained under a "Legacy (Go)" section. Stale daemon / SQLite / gRPC /
  `roost-cli-rs` references removed.
* **Branch protection on `main`** requires `ci-success`; merges are manual
  (committer merges after green; no auto-merge — repo `allow_auto_merge` off).

## What did NOT happen here (by design)

The Go code (`cmd/`, `internal/`, `go.mod`, `build/`) is **still present and still
builds**. Removing it is destructive and waits until Rust/Swift parity is
confirmed — see [`GODELETE.md`](GODELETE.md).
