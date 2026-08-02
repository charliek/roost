# Iced migration roadmap

Status: **active** — this is the governing sequencing document for the
Iced/GTK migration and the shared Rust engine. The
[Iced POC plan](iced-poc-plan.md) remains the archived design record and
acceptance-matrix reference; where the two disagree on sequencing, this
document wins.

Decision (2026-08-02): the `poc/iced` branch is **not** a throwaway. The
shared-engine extraction is behavior-preserving and merge-quality, the Iced
walking skeleton has proven terminal integration, custom chrome, and
compile-enforced parity. The branch merges to `main` after M0+M1 below, then
retires; all further work happens as topic branches off `main`, executed as
gauntlet passes sized one milestone slice at a time.

## Product guardrails (fixed)

1. **The Swift/AppKit app is the production daily driver.** It must stay
   release-ready on `main` at all times. No Swift behavior change lands
   without its own tests, and `e2e-mac` stays a required gate.
2. **The GTK app ships to Linux users.** No regressions — `e2e-gtk` and the
   Wayland drag guard are its no-regression gates — but GTK is the UI Iced
   eventually replaces, so it receives fixes, not new investment.
3. **`roost-iced` may live on `main` incomplete.** It is fully isolated (own
   binary, `ai.stridelabs.Roost.iced` profile, own socket/state/log paths,
   absent from release artifacts), so incompleteness cannot leak into either
   shipped app.
4. **One op set.** All Rust UI capability routes through
   `roost-engine`/`roost-ui-model`; the exhaustive `UiRequest` match in both
   Rust UIs is the parity mechanism. Never add a wildcard arm.
5. **CI rides along with the merge.** `iced-build-e2e` (2×2 OS×renderer
   matrix) and `harness-unit` are required `ci-success` gates from the first
   merge to `main` onward.

## Milestones

### M0 — hotfix the unreleased color regression on `main`

Independent of the merge; blocks the next Swift/GTK release regardless.

* Symptom: TUIs render in a single color in builds of **all three** UIs from
  post-v0.0.17 `main`/`poc/iced`; absent in the released v0.0.15.
* Prime suspects: PR #274's config-parser convergence, commits `6219bd3`
  (one value semantic across Swift+Rust parsers) and `1721b10` — these are on
  `main` but in **no tagged release**, matching the observed window exactly.
  The configured theme value (`Gruvbox Dark Hard`, spaces) and Ghostty theme
  lines (`palette = 16=#hex` — values containing `=` and `#`) are the likely
  casualties of a value-semantic change.
* Root-cause confirmation is in flight; update this section with the
  confirmed cause before executing the fix.
* Deliverable: topic branch off `main`, fix in **both** parsers with the
  failing case added to the shared fixture corpus (`7a02d34` pattern), PR to
  `main`, CI green.

### M1 — pre-merge hardening (on `poc/iced`)

1. **Facade decision (decided):** gate `roost-engine::facade` behind
   `feature = "facade"`, mark it experimental in `shared-rust-engine.md`, and
   correct that doc's framing — the crate boundary is Swift-ready; the facade
   is unproven until M5 adopts it. (Adoption by a Rust UI is deliberately
   deferred to the M5 spike, not done here.)
2. Fix the Iced lag handler (`crates/roost-iced/src/app.rs` ~5079): it logs
   "resyncing" but only breaks — either resync via `roost_engine::reconcile`
   or make log + comment state that recovery comes from the per-tick
   snapshot.
3. Add `roost-ui-model` to the CI dependency-boundary guard
   (`.github/workflows/ci.yml` — currently checks only `roost-engine` and
   `roost-iced`).
4. Sweep the ~67 stale GTK/glib/Pango doc references out of
   `roost-engine`/`roost-ui-model` sources.
5. Merge `main` → `poc/iced` to pick up M0.

### M2 — merge `poc/iced` to `main`

* PR into `main`; merge is manual after `ci-success` (repo policy).
* Review focus (highest-residual-risk GTK deltas): `preferred_tab` project
  activation semantics, `terminal_view.rs` extraction (−426 lines: wheel /
  scrollback / OSC reply ordering), `mouse_routing.rs` pointer rename, and
  the additive `app.window_metrics` wire fields (optional; Mac does not
  implement them).
* After merge the branch retires. Do not let it live past M2 — most of
  `roost-linux` is re-export shims on this branch, so every `main` hotfix
  diverges it further.

### M3 — Iced functional parity (the GTK-replacement track)

Slices, each sized for one gauntlet pass:

* **3a. `App` decomposition first.** Split the 8.8k-line
  `crates/roost-iced/src/app.rs` single `impl` (111 methods) into
  `app/` submodules (projects, palette, rename, notifications, clipboard,
  drag, osc). Mechanical; do it *before* adding surface area.
* **3b. Project lifecycle UI:** create (directory picker), delete
  (confirmation + cascade), reorder (adapt `tab_reorder.rs` pattern). Engine
  ops already exist and are unit-tested; this is UI wiring. Removes the
  `Err("… not available in Iced yet")` stubs.
* **3c. Sidebar resize** with persisted width (replaces the hardcoded
  `SIDEBAR_WIDTH: f32 = 220.0`).
* **3d. Tick → push subscriptions.** Replace the 16 ms full-snapshot poll
  and the UI-thread `block_on` calls with Iced stream subscriptions and
  event-driven reconcile. Do before more real-time features stack onto the
  poll.
* **3e. Polish parity:** notification bell/badge, hover-close, offscreen-tab
  reveal, empty/loading/error states, cursor/selection/link pixel geometry —
  per the P1 rows in the [parity inventory](iced-parity-inventory.md).
* **3f. Native desktop notifications** (narrow platform port, per-OS).
* **3g. Wayland gaps** (native file drop, clipboard seat serial) are
  upstream Iced/winit limitations — track, document, don't block on them.

### M4 — ship Iced to Linux users

Release packaging + appcast/apt integration, a state-migration decision
(does Iced adopt the GTK profile's `state.json` or migrate it), a beta
period behind an explicit opt-in, then the GTK deprecation decision. Entry
criteria: M3 complete, parity inventory shows no open P0/P1, and the
real-input tier passes on Iced for the drag/clipboard guards.

### M5 — Rust under Swift (exploration, not a commitment)

The Swift app's polish and daily use make replacement remote; the question
is where shared Rust reduces duplication *without* slowdowns.

1. Prove the facade: port one Rust UI's workspace mutations onto
   `Engine::execute` + `EngineEventStream` so the Swift-facing boundary has a
   real consumer before Swift bets on it.
2. FFI spike: `roost-engine-ffi` staticlib + cbindgen header, outbound-only
   (snapshot/events polling per the POC plan's ABI v1), measured for call
   overhead and allocator churn.
3. Candidate ranking for Swift adoption, by dup-value vs. FFI risk:
   `roost-ui-model` parsing/derivation first (config, themes, palette
   filtering, agent derivation — pure functions, low call rate, and the
   source of real dual-parser bugs like M0); engine workspace ops second
   (needs the undesigned bidirectional `UiRequest` reply-port ABI);
   per-frame render paths **never**.
4. Decision gate with data; until then `IPCHandlerImpl.swift` /
   `Workspace.swift` remain authoritative on Mac.

## Gauntlet operating notes

* One milestone slice per pass; every pass ends in PR(s) watched to green
  (`ci-success` required; merges manual).
* Mac verify constraint: the Swift app is single-instance and `make e2e-mac`
  kills the running `Roost.app` — check `$ROOST_TAB_ID` before running it
  from inside a Roost-hosted session.
* Linux real-input tiers run in the shed (`linux-test` skill) or CI, not on
  a Mac host.
* Theme/config changes must update the shared fixture corpus so both parsers
  stay pinned to one semantic.
