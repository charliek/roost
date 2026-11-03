# Iced migration — history & status

**Status: completed at v0.1.0.** iced is the only desktop UI: Linux ships
it as `roost`, and macOS ships it as `Roost.app`, replacing the Swift app.
This page is the record of how that happened and what is still open.

It replaces four planning documents that governed the migration while it
was in flight — the POC plan, the POC development notes, the parity
inventory, and the migration roadmap. They were accurate to a world with
three UI implementations in the tree, and that world is gone; git
history preserves them.

## Why iced

The short version — the decision-log entries are the canonical form:

- **[DL-15](vision.md#dl-15-linux-ships-iced-gtk4-rs-is-retired-and-removed-2026-08-25)**
  — the renderer was always ours (we walk libghostty-vt's render state
  and draw cell-aligned rects + text either way), so Cairo + Pango
  bought the terminal grid nothing that iced + wgpu doesn't buy on the
  GPU; iced runs on Linux *and* macOS, so one Rust codebase covers both
  host OSes; and both Rust UIs already sat on the toolkit-neutral
  `roost-engine`, so retiring an adapter cost the core nothing.
- **[DL-16](vision.md#dl-16-an-experimental-mac-iced-build-ships-beside-swift-2026-08-25)**
  — the macOS build existed to test the convergence hypothesis (one
  codebase, both platforms) against the Swift app in daily use. The
  hypothesis held; [DL-33](vision.md#dl-33-the-swift-app-is-retired-iced-is-the-only-desktop-ui-2026-11-03)
  retires the Swift app.

## What shipped, by milestone

The migration ran as numbered milestones, each executed as topic
branches off `main` in slices sized for one pass.

| Milestone | What it was | Outcome |
|---|---|---|
| **M0** | Color-regression investigation | Closed, not reproducible |
| **M1** | Pre-merge hardening on `poc/iced` | Complete (#277) |
| **M2** | Merge `poc/iced` → `main` | Complete (#278, 2026-08-02) |
| **M3** | Functional parity with the GTK UI | Complete |
| **E** | Engine track — renderer + robustness | Complete except a deferred Ghostty pin bump |
| **M4** | Ship iced to Linux users | Shipped in v0.0.18 |
| **M5** | Rust under Swift (FFI exploration) | Dropped; the facade was deleted |
| **M6** | macOS iced (evaluation → parity) | Experimental build shipped in v0.0.18; became `Roost.app` in v0.1.0 |

### M1–M2 — the POC was merge-quality

`crates/roost-iced` started on the `poc/iced` branch alongside an
extraction of the toolkit-neutral `roost-engine` and `roost-ui-model`
crates, which both Rust UIs then consumed. The 2026-08-02 decision was
that the branch was **not** a throwaway: the engine extraction was
behavior-preserving, and the iced walking skeleton had proven terminal
integration, custom chrome, and a compile-enforced parity mechanism (the
exhaustive `UiRequest` match — never a wildcard arm). It merged with
every gate green and retired immediately, because most of the GTK crate
was re-export shims on that branch and every `main` hotfix diverged it
further.

### M3 — functional parity

Slices, each a plan of its own: `app.rs` decomposition into `app/`
submodules (plan 009), project lifecycle UI (010), sidebar resize (011),
tick → push subscriptions (012), native desktop notifications over
`org.freedesktop.Notifications` (015), and two chrome-polish batches
against the Mac app's visual language (016, 026) — Inter for chrome,
Mac-matched colors and metrics, bell dots, drag-collapse, dynamic
titles.

### The engine track

The work that made iced *viable* rather than merely complete: render
baseline measurement, render-state dirty coverage, dirty-row snapshot
rebuild, sprite and box-drawing parity (plan 020), IME and dead-key
input (021), and crash robustness — a panic hook that writes a crash
report instead of exiting silently (019). One measured **NO-GO** is
recorded here rather than quietly dropped: run coalescing (018) did not
pay for itself.

### M4 — the Linux swap

The `.deb` ships `crates/roost-iced` as `/usr/bin/roost`, built with the
`linux-package` Cargo feature. That feature is off by default and flips
only the *compiled-in default* profile — `ROOST_BUNDLE_PROFILE` still
wins, so every dev harness stays correct regardless of how the binary
was built. The packaged build therefore adopts the production profile
the GTK package already owned: same socket, same `state.json`, same log
directory, same window class and desktop entry. Existing installs
upgrade in place with **no migration step**, and `roostctl` plus the
Claude hooks keep working unchanged. GTK4 and libadwaita left the
runtime dependency set.

### M5 — dropped

`roost-engine`'s `facade` module (`Engine` / `EngineCommand` /
`EngineSnapshot` / `EngineEventStream`) was the Swift-facing boundary. It
had no production consumer, and with the Swift app retired in v0.1.0 it
was deleted ([#286](https://github.com/charliek/roost/issues/286)).

### M6 — macOS iced

`Roost-Iced-<version>.dmg`, opt-in and installed beside the Swift `Roost.app` until v0.1.0, when the iced build took over the `Roost.app` name and bundle id:

- **Bundling + parallel install** (plan 027) — `mac/scripts/bundle-lib.sh`
  carries the toolkit-agnostic stages out of `bundle.sh`;
  `bundle-iced.sh` assembles `Roost-Iced.app` from
  `cargo build -p roost-iced` with the same signing path. Distinct
  bundle id, distinct socket/state/log paths, distinct single-instance
  locks. Claude hooks need no per-app configuration — `ROOST_SOCKET` is
  injected into every PTY child and outranks `--target`, so a session in
  an iced tab dials the iced socket automatically.
- **The native seam is `objc2`** (decided 2026-08-16, after building and
  running *both* candidates live): `crates/roost-iced/src/macos/`,
  `cfg(target_os = "macos")`, on the `objc2` 0.6 generation already in
  the lockfile via `arboard` and `softbuffer`. The rejected alternative
  was a Swift static library behind a C ABI, which would have made Swift
  a hard `cargo check` requirement on every dev Mac and CI cell — in the
  crate whose premise is replacing Swift.
- **Sparkle, menu bar, notifications** (plans 028, 030) — user-invoked
  update checks against `docs/appcast-iced.xml` with its own signing key
  (`SUEnableAutomaticChecks` is `false`; the app does not self-update;
  that feed is frozen since v0.1.0, and `Roost.app` updates from
  `docs/appcast.xml`),
  a real `NSMenu`, a Dock badge, and `UNUserNotificationCenter` banners
  that focus the originating tab on click.
- **Developer-ID signed and notarized**, published as the fourth release
  asset.

Two things were **decided, not discovered**: accessibility regresses
(an iced canvas gives essentially no VoiceOver support where AppKit
gives it for free — see DL-16), and dock-menu / reopen-from-Dock /
open-file-URL handling is winit-blocked, with no Swift parity bar to
chase since `App.swift` implements none of them either.

## Current status

| | macOS | Linux |
|---|---|---|
| Product | `Roost.app` — Rust + iced (`.dmg`) | `roost` — Rust + iced (`.deb`) |
| CI gates | the macOS half of `iced-build-e2e` | `iced-build-e2e` (X11 + Wayland), `iced-release` |

## What remains

- **macOS rough edges** are tracked as issues; none block the single-UI
  architecture. Window vibrancy is the notable one deferred rather than
  dropped: the Swift sidebar's translucency was AppKit's implicit
  source-list material, so iced has to build it explicitly.
- **Accessibility** regresses against AppKit: an iced canvas gives
  essentially no VoiceOver support (see DL-16). That was decided, not
  discovered, and stays open.
- **A mobile client** of `roost-session` is planned; the toolkit-neutral
  `roost-engine` and `roost-ui-model` crates are what it would reuse.

See [`tools/README.md`](https://github.com/charliek/roost/blob/main/tools/README.md)
for the test-harness layer map and [test-automation.md](test-automation.md)
for the CI lanes.
