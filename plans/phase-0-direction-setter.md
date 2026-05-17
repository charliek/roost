# Phase 0: Direction-setter

**Status**: ✅ done
**Exit criteria** (all met):
* Vision doc landed at `docs/development/vision.md` capturing the target architecture + decision log.
* `CLAUDE.md` and `README.md` updated to point at the vision doc as the durable north star.
* Skeleton top-level directories scaffolded (`proto/`, `crates/`, `mac/`, `third_party/ghostty/`).
* Refactor CI scaffold (`.github/workflows/refactor.yml`) added with a stub `rust-lint` job.
* Existing Go CI (`.github/workflows/ci.yml`) untouched and still green on macOS + Linux.
* Branch `claude/discuss-architecture-refactor-cjU3E` created off `main`.

## Goal

Get the refactor commitments written down before any code lands. The vision doc becomes the document every future PR cites; the decision log freezes the answers to the contentious "why this, not that" questions (Swift + AppKit on Mac, two languages, UDS not TCP, gRPC not JSON-RPC, etc.). With the doc in place, every subsequent phase has somewhere to anchor.

## Scope

In:
* Vision doc, decision log entries DL-1..DL-10.
* Skeleton directories so subsequent phases don't argue about layout.
* Refactor CI workflow file (stub jobs allowed).

Out:
* Any actual Rust, Swift, or proto code.
* Any change to the running Go binary or its CI.

## Touches Go code?

No. Purely additive.

## Commits

* `56b9880` — Lay refactor scaffold: vision, proto, Rust workspace, Mac SwiftPM, CI.

(This phase landed as a single commit because the scaffold was indivisible — the CI file's path coordinates with the directory layout.)

## Risks / known gaps

None. The phase is closed.

## Follow-ups

* Future commits append decision log entries as new "why" questions surface. DL-1..DL-10 are the original set; more were appended during Phases 5–6 (e.g. the grpc-swift v2 `:authority` choice — see Phase 5 doc).
