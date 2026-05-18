# Roost refactor plan

This directory tracks the multi-phase migration of Roost from a single Go + GTK4 binary toward a Rust core daemon (`roost-core`) plus native UIs (Swift + AppKit on macOS, Rust + gtk4-rs on Linux) communicating over a `.proto`-defined gRPC contract on a Unix domain socket.

See [`docs/development/vision.md`](../docs/development/vision.md) for the target architecture and the durable design choices (decision log). This index summarizes each phase; the per-phase files in this directory contain the detailed step lists, exit criteria, and commit log.

## Branch policy

Active refactor work lives on the long-lived `feature/rust-port` branch; the predecessor `claude/discuss-architecture-refactor-cjU3E` is frozen at `00b3d10`. Polish PRs ship from short-lived `polish/*` topic branches that squash-merge into `feature/rust-port` with auto-merge gated on the 3 required macOS CI checks (`test (macos-latest)`, `rust-build (macos-latest)`, `swift-mac`); Linux jobs run informationally. New Rust/Swift/proto code lands in new top-level directories (`proto/`, `crates/`, `mac/`, `linux/`, `third_party/ghostty/`); existing `cmd/` and `internal/` Go code stays in place until the Phase 9 cutover. Both the legacy CI workflow (`.github/workflows/ci.yml`) and the refactor CI workflow (`.github/workflows/refactor.yml`) must stay green on every commit.

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
| [6a](phase-6a-mac-structural.md) | Mac structural parity (multi-tab, sidebar, projects, persistence, menus) | ✅ done (M1–M7 + P1–P3 closed on `feature/rust-port`) | yes |
| [6b](phase-6b-mac-osc-notifications.md) | Mac OSC + notifications (the differentiator) | ✅ done (P4–P9 closed on `feature/rust-port`) | yes |
| [7](phase-7-linux-ui.md) | Linux UI (gtk4-rs + Cairo + Pango) | ✅ done (commits 0–11 + follow-up landed via PR #50, squash `421b384`, 2026-05-17) | yes |
| [7.5](phase-7-5-polish-and-gaps.md) | Linux/Mac polish + automation gaps (drag-reorder, CSS, icons, theme overrides, `tab snapshot`, etc.) | ⏳ scoped | yes |
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

## Closed goals (all on `feature/rust-port`)

* [`goal-rust-port-polish-2026-05-16.md`](goal-rust-port-polish-2026-05-16.md) — M1–M8 (chrome foundation, native sidebar, tab strip + resize, headless CLI, selection + copy, themes + config, mac `.app` bundling, gtk4-rs Identify spike). [UX assessment](ux-assessment-2026-05-16.md) captures the pre-goal snapshot.
* [`goal-phase-6-complete-2026-05-16.md`](goal-phase-6-complete-2026-05-16.md) — P1–P9 (keybind config, font zoom, palette FFI, OSC scanner port, daemon OSC routing, UI OSC detect, notification badges, desktop notifications, Claude hook end-to-end). Phase 6 is closed on `feature/rust-port`.
* [`goal-mac-polish-cursor-keys-2026-05-17.md`](goal-mac-polish-cursor-keys-2026-05-17.md) — M1–M6 (libghostty key-encoder bridge, cursor rendering, sidebar toggle, cycle/rename tab, PTY-exit cascade, scrollback) + the post-goal fixes for orphan-tab purge, cursor-on-focus override, OSC UTF-8.
* **Phase 7 (Linux UI)** — see [`phase-7-linux-ui.md`](phase-7-linux-ui.md). Landed via PR #50 squash `421b384`. Carries: `roost-vt` safe API, `roost-osc` shared crate, daemon `ReorderTabs`/`ReorderProjects` RPCs, Cairo+Pango cell renderer, StreamPty round-trip, full key encoder, scrollback + selection + clipboard, sidebar + AdwTabView + WatchEvents, keybind config, OSC + notifications, themes + config + focus-tab action.

Active goal: [`phase-7-5-polish-and-gaps.md`](phase-7-5-polish-and-gaps.md) — Linux + Mac visual polish, drag-to-reorder UI, automation API gaps, deferred cross-platform items. Optional; can be skipped if the user wants to jump straight to Phase 8.

Next phase after that: **Phase 8 (bundling)** — notarized Mac `.app` + DMG + Linux AppImage. The user's stated gate for `feature/rust-port → main` is "Phase 8 first so users pulling `main` get an installable artifact rather than source-only" (decision 2026-05-17).

## Status snapshot (2026-05-17, end of day)

* Phases 0–7 landed and merged-ready on `feature/rust-port`.
* Phase 7 closure: gtk4-rs Linux UI builds + runs on Mac Homebrew GTK4 + libadwaita with full feature surface; cross-client convergence with the Swift Mac UI verified end-to-end via `roost-cli-rs`. Daemon orphan-tab purge (`234378e`), Mac UI cursor-on-focus override (`266dea7`), OSC UTF-8 multibyte fix (`aebd408`), Swift OscScanner+KeyEncoder regression tests (`b5b7838`) all merged before Phase 7 PR.
* Local Phase 7 worktree torn down. Local branch refs cleaned up. `feature/rust-port` is at `421b384`.
* macOS 26 arm64e-only SDK workaround is in both `build/build.sh` (from `f6e0d64` on main) and `third_party/ghostty/build.sh` — both Zig 0.15.2 + Ghostty SHA toolchains build on macOS 26 hosts.
* Two ghostty builds (`build/build.sh` for Go cgo, `third_party/ghostty/build.sh` for Rust bindgen + Swift) coexist and must pin the same SHA. They collapse in Phase 9.
* `feature/rust-port → main` deferred until Phase 8 lands (user decision 2026-05-17): merging now would put source-only Rust+Swift code on `main` without an installable artifact.

## How to use these documents

* **For the human**: each phase doc tells you what's done, what's left, the rough order of operations, the exit criteria, and which Go files (if any) the phase touches. Use them to scope PRs, decide merge-to-main vs. keep-on-branch, and brief reviewers.
* **For a future agent picking up the work**: each phase doc is self-contained enough to be loaded into context and worked from. Cite it in commit messages so the audit trail stays intact.
* **For commits**: every refactor commit should reference a phase (e.g. "Phase 6a step 2c:" prefix in the subject) so `git log` reads as a phase narrative.

## Open questions tracked outside any single phase

* Whether to keep `roost-cli-rs` as the transitional binary name through Phase 8 or rename to `roost-cli` earlier with a Go-side compatibility shim. See [DL-9 in vision.md](../docs/development/vision.md#dl-9-new-rust-cli-lands-under-a-transitional-binary-name).
* Whether the SQLite database file should keep its current path across the cutover. The schema ports byte-for-byte (DL-7) but a user's existing `roost.db` will continue to be read by both binaries until Phase 9 deletes the Go reader. No issues anticipated.
* CodeRabbit reviews each refactor commit; small follow-up commits to address actionable items are part of every phase's working budget.
