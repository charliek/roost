# Roost refactor plan

This directory tracks the multi-phase migration of Roost from a single Go + GTK4 binary to two native UIs (Swift + AppKit on macOS, Rust + gtk4-rs on Linux) that each embed the workspace + PTY supervisor in-process and serve a JSON IPC socket for external tooling. The pre-2026-05-23 portion of the migration went through a Rust gRPC daemon (`roost-core`); the post-2026-05-23 [`refactor/inline-core` branch](humming-inventing-flame.md) collapsed the daemon back into each UI process and replaced the gRPC contract with newline-delimited JSON.

**Current head**: `main`. The daemon-removal refactor (M0–M9, PR #78), Phase 8 bundling (v0.0.1 — DMG + `.deb` via apt), and the Phase 9 **cutover to main** have all landed; `feature/rust-port` is retired. `roost-core`, `roost-proto`, `roost-common`, `roost-smoke` and `proto/` no longer exist. The daemon-era phase files below stay as historical context. The only remaining migration step is [`GODELETE`](GODELETE.md) — removing the legacy Go code, deferred until parity is confirmed.

See [`docs/development/vision.md`](../docs/development/vision.md) for the target architecture and the durable design choices (decision log). This index summarizes each phase; the per-phase files in this directory contain the detailed step lists, exit criteria, and commit log.

## Branch policy

`main` is the primary branch (post-cutover). Topic branches (`polish/*`,
`refactor/*`, feature branches) open PRs into `main`. **Merges are manual**: CI
must be green, then the committer merges (no auto-merge — the repo's
`allow_auto_merge` is off). The single required check is **`ci-success`** from
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) (rust/swift/gtk,
path-filtered). The legacy Go CI ([`go-legacy.yml`](../.github/workflows/go-legacy.yml))
runs only on Go-file changes and is not required. The legacy `cmd/` + `internal/`
Go code stays in place until [`GODELETE`](GODELETE.md). The predecessor
`claude/discuss-architecture-refactor-cjU3E` is frozen at `00b3d10`.

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
| [7.5](phase-7-5-polish-and-gaps.md) | Linux/Mac polish + automation gaps (drag-reorder, CSS, icons, theme overrides, `tab snapshot`, etc.) | 🪦 superseded by [Linux GTK parity goal](goal-linux-gtk-parity-2026-05-17.md) (carried tracks A/B1/D); B2/C/E filed below | n/a |
| [7.5b](goal-linux-gtk-parity-2026-05-17.md) | Linux GTK parity (port chrome + UX from Go GTK binary to gtk4-rs Rust UI) | ✅ closed — M1–M10 + M9.5 landed by 2026-05-18 via PRs #51–#64, #66 (M11 user-theme-overrides dropped after parity check) | yes |
| [7.5c](goal-mac-parity-2026-05-18.md) | Mac UI parity (drag-reorder, inline rename, live cwd subtitle, sidebar rollup, tab pill right-click; headerbar deferred to user-eval at M7) | ✅ closed 2026-05-22 — M1–M6 + R1–R7 polish landed via PRs #67–#76; M7 headerbar dropped after user-eval | yes |
| [8](phase-8-bundling.md) | Bundling (Mac `.app` + DMG; Linux `.deb` via apt-charliek) | ✅ done — v0.0.1 shipped 2026-05-23 (`.deb` not AppImage; DMG ad-hoc-signed pending Apple creds, [#83](https://github.com/charliek/roost/issues/83)) | yes |
| [9](phase-9-cutover.md) | Cutover to main (merge `feature/rust-port` → `main`; rust-primary CI; docs reoriented) | ✅ done 2026-05-23 (merge commit) | done |
| [GODELETE](GODELETE.md) | Remove the legacy Go code (`cmd/`, `internal/`, `go.mod`, `build/`, `go-legacy.yml`) | ⏳ pending — after Rust/Swift parity + sign-off | **destructive — own PR** |

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

Both UI parity goals are closed: [Linux GTK parity](goal-linux-gtk-parity-2026-05-17.md) on 2026-05-18 (M1–M10 + M9.5; M11 user-theme-overrides dropped) and [Mac UI parity](goal-mac-parity-2026-05-18.md) on 2026-05-22 (M1–M6 + R1–R7 polish rounds; M7 headerbar dropped after user-eval).

Phase 8 (bundling) shipped v0.0.1 on 2026-05-23 (Mac DMG + Linux `.deb` via apt-charliek), which was the user's stated gate for `feature/rust-port → main`. The cutover to `main` then landed (Phase 9). Next and final migration step: **[`GODELETE`](GODELETE.md)** — remove the legacy Go code once parity is confirmed.

## Status snapshot (2026-05-22)

* Phases 0–7 landed and merged-ready on `feature/rust-port`.
* Both UI parity goals closed: Linux GTK parity 2026-05-18 (PRs #51–#64, #66; M11 dropped); Mac UI parity 2026-05-22 (PRs #67–#76; M7 dropped). Cross-client convergence between the Swift Mac UI, Rust gtk4-rs Linux UI, and `roost-cli-rs` verified end-to-end.
* `feature/rust-port` is at `9c36ba0` (R7 sidebar collapse + drop indicator fixes).
* macOS 26 arm64e-only SDK workaround is in both `build/build.sh` (from `f6e0d64` on main) and `third_party/ghostty/build.sh` — both Zig 0.15.2 + Ghostty SHA toolchains build on macOS 26 hosts.
* Two libghostty-vt builds (`build/build.sh` for Go cgo, `third_party/ghostty/build.sh` for Rust bindgen + Swift) coexist and must pin the same SHA. They collapse in Phase 9.
* `feature/rust-port → main` landed 2026-05-23 once Phase 8 shipped v0.0.1 (DMG + apt `.deb`); `main` is now rust-primary. Remaining migration step: [`GODELETE`](GODELETE.md) (remove the legacy Go code, after parity).

## How to use these documents

* **For the human**: each phase doc tells you what's done, what's left, the rough order of operations, the exit criteria, and which Go files (if any) the phase touches. Use them to scope PRs, decide merge-to-main vs. keep-on-branch, and brief reviewers.
* **For a future agent picking up the work**: each phase doc is self-contained enough to be loaded into context and worked from. Cite it in commit messages so the audit trail stays intact.
* **For commits**: every refactor commit should reference a phase (e.g. "Phase 6a step 2c:" prefix in the subject) so `git log` reads as a phase narrative.

## Open questions tracked outside any single phase

* Whether to keep `roost-cli-rs` as the transitional binary name through Phase 8 or rename to `roost-cli` earlier with a Go-side compatibility shim. See [DL-9 in vision.md](../docs/development/vision.md#dl-9-new-rust-cli-lands-under-a-transitional-binary-name).
* Whether the SQLite database file should keep its current path across the cutover. The schema ports byte-for-byte (DL-7) but a user's existing `roost.db` will continue to be read by both binaries until Phase 9 deletes the Go reader. No issues anticipated.
* CodeRabbit reviews each refactor commit; small follow-up commits to address actionable items are part of every phase's working budget.

### Follow-up tracks from the superseded Phase 7.5

When the [Linux GTK parity goal](goal-linux-gtk-parity-2026-05-17.md) closes, these tracks from the old [Phase 7.5](phase-7-5-polish-and-gaps.md) doc still need homes:

* **B2 — Mac drag-reorder UI** — Mac is structurally complete; this is Mac-only polish. File as `polish/mac-reorder-drag` if/when the user wants it.
* **C1 — `tab snapshot` RPC** — for CI to verify renderer state headlessly. Architecture choice pending (daemon-side libghostty parse vs UI-side StreamPty extension vs ring buffer). Could fold into Phase 8 or be a separate goal.
* **C2 — `roost-cli-rs watch` for WatchEvents** — debug subcommand. Small; could land any time.
* **C3 — Surface daemon `NotFound` from `tab send`** — CLI swallows the error today.
* **C4 — Daemon-side `tracing::info!` for `fire_notification`** — one-liner.
* **E1 — Wide-char width** (CJK + emoji); affects both UIs.
* **E2 — IME composition** (`markedText` pattern); affects both UIs.
* **E3 — Mouse encoder** for xterm mouse-tracking apps (`htop -mouse`, `vim` with `set mouse=a`).
* **E4 — `macos-option-as-alt` config setting** — one-line plumbing through `mac/Sources/Roost/KeyEncoder.swift`.
