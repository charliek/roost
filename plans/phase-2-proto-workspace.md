# Phase 2: Proto + Cargo workspace + Xcode skeleton + vendored Ghostty

**Status**: ✅ done
**Exit criteria** (all met):
* `proto/roost.proto` defines the v1 wire contract end to end (Identity, tab lifecycle, StreamPty, WatchEvents, control RPCs, OSC upcall).
* `proto/CHANGELOG.md` records the v1 schema.
* `cargo check --workspace` green on macOS + Linux. Workspace has `roost-core`, `roost-proto`, `roost-common`, `roost-cli-rs`, `roost-smoke`, `roost-vt`.
* `swift build` green on macOS — Mac SwiftPM package compiles an empty AppKit window app with a generated `Roost_V1` Swift module.
* `third_party/ghostty/build.sh` builds libghostty-vt from a pinned Ghostty SHA into a static archive at `third_party/ghostty/out/lib/libghostty-vt.a`.
* CI codegen-check job green: workspace builds and Swift package builds on every commit.

## Goal

Make the proto contract real and freeze the workspace layout. Once two UIs depend on the schema, the cost of breaking it goes up sharply; ratifying it before any UI code lands gets the contract right while the cost is still low.

## Scope

In:
* The `.proto` itself, with proto file numbering for every field set deliberately (no renumbers after this commit).
* `roost-proto`'s `build.rs` running `tonic-build` to generate Rust bindings checked into `OUT_DIR`.
* `mac/Package.swift` declaring the `GRPCProtobufGenerator` build plugin so Swift gets `Roost_V1_*` types regenerated at every `swift build`.
* The vendored Ghostty build script + SHA pin + CI cache plumbing.
* CodeRabbit follow-ups on the initial Mac SwiftPM coordinates (`b59644d`).

Out:
* Any service implementation (Phase 3 owns the daemon).
* Any UI work (Phase 5 owns the Mac UI; Phase 7 owns Linux).

## Touches Go code?

No. The Go `build/build.sh` keeps its own Ghostty SHA pin in lockstep with `third_party/ghostty/build.sh` (see [DL-10](../docs/development/vision.md#dl-10-ghostty-sha-pinned-in-two-places-during-the-transition)). Bumps move both — neither moves alone.

## Commits

* `56b9880` — Lay refactor scaffold (the same commit as Phase 0 — the scaffold + skeleton proto landed together).
* `b59644d` — Fix Mac SwiftPM coordinates + flip baseline CI to required-green.

## Risks / known gaps

* The `mac/Package.swift` SwiftPM build plugin uses `GRPCProtobufGenerator`, which `shells out to protoc`. CI exports `PROTOC_PATH` to whatever `brew install protobuf` put on PATH; local dev needs the same env var. Documented in `.github/workflows/refactor.yml`.
* Once Phase 9 lands, `proto/CHANGELOG.md` should pin a 1.0 entry. Until then schema changes can collapse into the "Pre-1.0 schema tightening" section.

## Follow-ups

* Phase 6a step 2a added the project lifecycle RPCs and the `Project*Event` variants — those are extensions of the v1 contract, not breaking changes. The schema is intentionally still pre-1.0 until cutover.
