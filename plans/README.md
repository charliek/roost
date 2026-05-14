# Roost refactor plan

This directory tracks the multi-phase migration of Roost from a single Go + GTK4 binary toward a Rust core daemon (`roost-core`) plus native UIs (Swift + AppKit on macOS, Rust + gtk4-rs on Linux) communicating over a `.proto`-defined gRPC contract on a Unix domain socket.

See [`docs/development/vision.md`](../docs/development/vision.md) for the target architecture and the durable design choices (decision log). This index summarizes each phase; the per-phase files in this directory contain the detailed step lists, exit criteria, and commit log.

## Branch policy

All refactor work lives on `claude/discuss-architecture-refactor-cjU3E`. New Rust/Swift/proto code lands in new top-level directories (`proto/`, `crates/`, `mac/`, `linux/`, `third_party/ghostty/`); existing `cmd/` and `internal/` Go code stays in place until the Phase 9 cutover. Both the legacy CI workflow (`.github/workflows/ci.yml`) and the refactor CI workflow (`.github/workflows/refactor.yml`) must stay green on every commit.

**Mergeability into `main`.** Through Phase 8 the refactor is purely additive: every commit on the branch leaves the Go binary buildable and shippable on `main`. The branch can be merged to `main` at any phase boundary without breaking the live Go program. The Phase 9 commit deletes `cmd/` and `internal/` and is destructive — it must land separately, after the Rust/Swift surface has reached feature parity and bundled binaries are ready.

## Phase index

| # | Phase | Status | Mergeable to main? |
|---|---|---|---|
| [0](phase-0-direction-setter.md) | Direction-setter (vision, skeleton dirs, refactor CI) | ✅ done | yes |
| [1](phase-1-derisk-spikes.md) | De-risk spikes (tonic UDS, grpc-swift v2 UDS, libghostty-vt FFI) | ✅ done | yes |
| [2](phase-2-proto-workspace.md) | Proto + Cargo workspace + Xcode skeleton + vendored Ghostty | ✅ done | yes |
| [3](phase-3-rust-core-mvp.md) | Rust core MVP (`roost-core` daemon, StreamPty, SQLite) | ✅ done | yes |
| [4](phase-4-smoke-client.md) | Smoke client (`roost-smoke` pipes bash through the daemon) | ✅ done | yes |
| [5](phase-5-mac-ui-mvp.md) | Mac UI MVP (single-tab AppKit window over the daemon) | ✅ done | yes |
| [6a](phase-6a-mac-structural.md) | Mac structural parity (multi-tab, sidebar, projects, persistence, menus) | 🚧 in progress | yes |
| [6b](phase-6b-mac-osc-notifications.md) | Mac OSC + notifications (the differentiator) | ⏳ pending | yes |
| [7](phase-7-linux-ui.md) | Linux UI (gtk4-rs + Cairo + Pango) | ⏳ pending | yes |
| [8](phase-8-bundling.md) | Bundling (Mac `.app` + DMG + notarytool; Linux AppImage) | ⏳ pending | yes |
| [9](phase-9-cutover.md) | Cutover (delete `cmd/`, `internal/`, Go-specific make targets) | ⏳ pending | **destructive — separate PR** |

## High-level shape (target)

```
.
├── proto/                 # roost.proto — single source of truth for the wire contract
├── crates/                # Rust workspace
│   ├── roost-core/        # daemon: tonic gRPC server, PTY supervisor, SQLite
│   ├── roost-cli-rs/      # CLI (renamed to roost-cli in Phase 9)
│   ├── roost-vt/          # libghostty-vt Rust bindings (used by Linux UI)
│   ├── roost-common/      # shared paths, UDS connector
│   ├── roost-proto/       # tonic-generated proto bindings for Rust consumers
│   └── roost-smoke/       # end-to-end smoke binary
├── mac/                   # Swift package (Roost.app)
│   └── Sources/Roost/     # AppKit + libghostty-vt + grpc-swift v2
├── linux/                 # Rust crate (future — Phase 7)
│   └── ...                # gtk4-rs + Cairo + Pango + tonic client
├── third_party/ghostty/   # Vendored libghostty-vt build (Phase 9 collapses with build/)
└── docs/development/
    └── vision.md          # Target architecture (this dir + its kin describe the plan)
```

## Status snapshot (2026-05-14)

* Phases 0–5 landed and merged-ready.
* Phase 6a is roughly 70% done. Multi-tab, project sidebar, project lifecycle RPCs, shortcut alignment with the Go binary, and a live-daemon CI regression guard are in. Remaining: WatchEvents subscription, keybind override config, secondary shortcuts (cycle_tab, font sizing, toggle_sidebar, rename_tab), visual polish.
* Phases 6b, 7, 8, 9 not yet started.
* Two ghostty builds (`build/build.sh` for Go cgo, `third_party/ghostty/build.sh` for Rust bindgen + Swift) coexist and must pin the same SHA. They collapse in Phase 9.

## How to use these documents

* **For the human**: each phase doc tells you what's done, what's left, the rough order of operations, the exit criteria, and which Go files (if any) the phase touches. Use them to scope PRs, decide merge-to-main vs. keep-on-branch, and brief reviewers.
* **For a future agent picking up the work**: each phase doc is self-contained enough to be loaded into context and worked from. Cite it in commit messages so the audit trail stays intact.
* **For commits**: every refactor commit should reference a phase (e.g. "Phase 6a step 2c:" prefix in the subject) so `git log` reads as a phase narrative.

## Open questions tracked outside any single phase

* Whether to keep `roost-cli-rs` as the transitional binary name through Phase 8 or rename to `roost-cli` earlier with a Go-side compatibility shim. See [DL-9 in vision.md](../docs/development/vision.md#dl-9-new-rust-cli-lands-under-a-transitional-binary-name).
* Whether the SQLite database file should keep its current path across the cutover. The schema ports byte-for-byte (DL-7) but a user's existing `roost.db` will continue to be read by both binaries until Phase 9 deletes the Go reader. No issues anticipated.
* CodeRabbit reviews each refactor commit; small follow-up commits to address actionable items are part of every phase's working budget.
