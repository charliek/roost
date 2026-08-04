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

### M0 — color regression investigation — **closed (not reproducible)**

Independent of the merge; kept as the reproduction recipe if it returns.

* Symptom: TUIs (`strix`, `prox`) reported single-color in builds of all
  three UIs from `poc/iced` tip; absent in the released v0.0.15.
* Investigation state (2026-08-02):
    * **Code window `v0.0.15..poc/iced` is cleared.** The config-parser
      suspects (`6219bd3`/`1721b10`) were disproven by direct reproduction —
      both parsers resolve `Gruvbox Dark Hard` + a full 256-entry palette;
      theme-file parsing never touches the changed value path. The entire
      Mac color path (`TerminalView.swift`, `RenderState.swift`, themes,
      shell-integration, libghostty pin, PTY env) is byte-identical to
      v0.0.15. GTK/roost-vt color paths in the window are ±0 lines.
    * **Live repro FAILED on the branch Swift build** (this machine): strix
      renders 6 distinct foregrounds via `tab.dump_resolved` under all three
      spawn paths — direct argv with the app terminal-launched, direct argv
      with the app `open`-launched (launchd env), and `strix` typed into the
      interactive shell. Screenshot + dumps in the session scratchpad.
    * Known env quirk (unconfirmed as cause, strix unaffected): Roost PTY
      children inherit `TERMINFO=/Applications/Ghostty.app/...` (only
      ghostty entries) while Roost forces `TERM=xterm-256color`; ncurses
      falls back to `/usr/share/terminfo`, strict `$TERMINFO` readers would
      not.
* **Outcome (2026-08-02): DOWNGRADED TO WATCH ITEM — not reproducible.**
  The strix probe (tab with `--hold -- strix` in a git cwd, then
  `tab.dump_resolved`, count distinct foregrounds) renders the identical
  6-fg/2-bg palette on all three UIs on this machine — Swift (terminal- and
  `open`-launched, plus typed into the interactive shell), GTK
  (`Roost-gtk`), and iced — and screenshots confirm the paint, not just the
  resolver. strix 0.0.7 is unchanged since Jul 26, so the observed binary
  is the tested binary. If the symptom reappears, re-run this probe first
  and capture the tab env (`env | sort`) before anything else.
* **Actionable remainder: TERMINFO env hygiene — done (PR #276, merged
  2026-08-02).** `TERMINFO` is stripped from PTY child env in both
  `PtySupervisor.swift` (`childEnvironment`) and the Rust PTY spawn, with
  env-assertion tests on both sides.

### M1 — pre-merge hardening (on `poc/iced`) — **complete (PR #277)**

1. ~~Facade decision~~ **done:** `roost-engine::facade` is gated behind
   `feature = "facade"` (off by default, tested explicitly in CI),
   `shared-rust-engine.md` now frames it as experimental. Adoption by a
   Rust UI is deliberately deferred to the M5 spike.
2. ~~Iced lag handler~~ **done:** log + comment now state that recovery
   comes from the per-tick snapshot reconcile; the receiver resubscribes
   past the stale backlog.
3. ~~CI boundary guard~~ **done:** `roost-ui-model` added to the
   toolkit-dependency check; PRs targeting `poc/iced` now trigger CI
   (previously only pushes did).
4. ~~Stale GTK doc sweep~~ **done:** toolkit-specific language reworded
   toolkit-neutrally across `roost-engine`/`roost-ui-model` sources.
5. ~~Merge `main` → branch~~ **done:** picks up the M0 TERMINFO fix
   (rename-detection carried it into `roost-engine/src/pty.rs`; its test
   moved to `roost-engine/tests/`).

### M2 — merge `poc/iced` to `main` — **complete (PR #278, merged 2026-08-02)**

All gates green on the merge: `e2e-mac`, the three GTK tiers, and the
full iced 2×2 matrix. `poc/iced` is retired (merged, branch pointer kept).

* **Authorized by the user (2026-08-02)** contingent on M1 complete and
  full `ci-success` green on the PR — `e2e-mac` (Swift production-ready)
  and the GTK tiers (no Linux regression) are the goals the merge must
  meet.
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

* **3a. `App` decomposition (complete — plan 009).** Split the 8.9k-line
  `crates/roost-iced/src/app.rs` into `app/` submodules as shipped:
  `terminal_tab` (TerminalTab + geometry + pointer types), `palettes`
  (palette/typography/provider), `interactions` (rename, tab drag,
  pointer, clipboard/drop, screenshot), `servicing` (UiRequest/OSC/
  reconcile/metrics); parent keeps struct, orchestrators, keyboard, and
  projects/sidebar. Mechanical, behavior-preserving.
* **3b. Project lifecycle UI (complete — plan 010).** Shipped: create
  (keybind + palette + sidebar-footer `+ New Project`, name `""` + cwd
  `$HOME`, no directory picker — matching both shipped UIs), delete
  behind an in-app confirm overlay then engine cascade, and pointer
  reorder via the project instantiation of `strip_reorder.rs`. Both
  `Err("… not available in Iced yet")` stubs are gone. Covered by the
  real-input guard (`tools/input/linux/iced_clipboard_check.py`:
  XTEST project drag, sub-threshold select, keybind confirm
  cancel/confirm with no PTY leak) and functional e2e
  (`tools/roosttest/test_project_lifecycle.py`).
* **3c. Sidebar resize (complete — plan 011).** Shipped: iced seam drag via
  a zero-footprint `SidebarResizeGrip` (exclusive event ownership so a seam
  press never starts a terminal selection), 160–400 clamp, engine-persisted
  width in `state.json` with default 220 (replacing the hardcoded
  `SIDEBAR_WIDTH: f32 = 220.0`); GTK's drag-but-forgets-on-relaunch bug
  fixed as a ride-along; a test-mode `sidebar.set_width` op on all three
  UIs (finite out-of-band widths clamp, non-finite → `invalid-param`,
  without test mode → `not-enabled`); functional e2e
  (`tools/roosttest/test_sidebar_resize.py`, wired into `ICED_E2E_TESTS`
  and `ci.yml`); and a real-input grip segment in
  `tools/input/linux/iced_clipboard_check.py`.
* **3d. Tick → push subscriptions.** Replace the 16 ms full-snapshot poll
  and the UI-thread `block_on` calls with Iced stream subscriptions and
  event-driven reconcile. Do before more real-time features stack onto the
  poll.
* **3e. Polish parity:** notification bell/badge, hover-close, offscreen-tab
  reveal, empty/loading/error states, cursor/selection/link pixel geometry —
  per the P1 rows in the [parity inventory](iced-parity-inventory.md).
  Also sidebar/chrome visual polish (subtle color differences, separator
  lines, the footer chip's exact bezel, and a spike on the Mac app's
  frosted/translucent look — likely `window-vibrancy` over a transparent
  iced window on macOS; compositor-dependent on Linux). **Plan this slice
  with the user in the loop — they want to give detailed direction here.**
  The tab-strip artifact bug ([#281]) that was folded in here is **fixed**
  (PR #291, merged 2026-08-04): the strip scrollbar is `Scrollbar::hidden()`
  — live testing rejected both a resting and a hover-revealed sliver, so any
  visible indicator over the pills is a regression, and
  `tools/roosttest/test_tab_strip_pixels.py` guards it (its allowlist of
  wide-run band colors must be updated by any 3e band-background change).
  Two 3e items named during that testing: the band background under the
  tabs should match the Swift look, and tab-band metrics differ (iced
  34px band / 5px above pills vs Swift `tabBarHeight` 32 / centerY ≈4px).
  Carried from 3b (plan 010): designing the empty state after the last
  project is deleted — iced lands in the engine's empty workspace state
  while both shipped UIs close their window — and tightening the confirm
  overlay's pointer modality (it blocks presses but, like the palette,
  passes wheel/hover through) are 3e items.
* **3f. Native desktop notifications** (narrow platform port, per-OS).
* **3g. Wayland gaps** (native file drop, clipboard seat serial) are
  upstream Iced/winit limitations — track, document, don't block on them.

Slice order is deliberate: 3b closed the honest `Err("… not available in
Iced yet")` stubs — the grep now has zero hits — and 3c closed the last
functional gap blocking M4; 3d is the
architecture cleanup that everything real-time depends on; 3e/3f/3g are
polish and platform work that can interleave.

### Maintenance backlog (filed, not scheduled)

Work this migration surfaced that should not block a slice. Pull one in
when it touches the code you are already in:

| item | issue |
|---|---|
| ~~Iced tab-strip artifacts with several tabs open~~ **fixed** (PR #291) | [#281] |
| Iced SIGABRT: swash subtract-with-overflow panic during glyph shaping (debug build; trigger unknown — launch with `RUST_BACKTRACE=1` to capture) | [#292] |
| **Security-adjacent:** Swift's dragged-URL drop branch has no control-char rejection (mitigated by the paste-boundary wrap from #280, not closed); Rust/Swift filter predicates also diverge | [#282] |
| `roost-linux` clippy `type_complexity` debt keeping it out of the lint gate | [#283] |
| No CI gate for GTK↔Iced visual parity (capture tooling is human-reviewed) | [#284] |
| No real-input (CGEvent) harness on macOS — uinput tier is Linux-only | [#285] |
| `roost-engine::facade` has no consumer; prove it or delete it (blocks M5) | [#286] |
| Mac `app.window_metrics` omits `terminal_top` / `terminal_font_family` | [#287] |
| `app/interactions.rs` at 2,960 lines — finer split when fixtures allow | [#288] |
| swift-testing runner SIGABRT on fast value-check swarms (XCTest workaround) | [#289] |

[#281]: https://github.com/charliek/roost/issues/281
[#282]: https://github.com/charliek/roost/issues/282
[#283]: https://github.com/charliek/roost/issues/283
[#284]: https://github.com/charliek/roost/issues/284
[#285]: https://github.com/charliek/roost/issues/285
[#286]: https://github.com/charliek/roost/issues/286
[#287]: https://github.com/charliek/roost/issues/287
[#288]: https://github.com/charliek/roost/issues/288
[#289]: https://github.com/charliek/roost/issues/289
[#292]: https://github.com/charliek/roost/issues/292

### M4 — ship Iced to Linux users

Release packaging + appcast/apt integration, a state-migration decision
(does Iced adopt the GTK profile's `state.json` or migrate it), a beta
period behind an explicit opt-in, then the GTK deprecation decision. Entry
criteria: M3 complete, parity inventory shows no open P0/P1, and the
real-input tier passes on Iced for the drag/clipboard guards.

### M5 — Rust under Swift (exploration, not a commitment)

The Swift app's polish and daily use make replacement remote; the question
is where shared Rust reduces duplication *without* slowdowns.

1. Prove the facade ([#286]): port one Rust UI's workspace mutations onto
   `Engine::execute` + `EngineEventStream` so the Swift-facing boundary has a
   real consumer before Swift bets on it. It is feature-gated
   (`roost-engine/facade`, off by default) until then; if adoption never
   happens, delete it rather than carry a parallel API.
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
