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
* **3d. Tick → push subscriptions (complete — plan 012).** Shipped: the
  16 ms full-snapshot poll and all UI-thread `block_on` calls are gone; one
  `EngineFeed` channel (workspace events via the shared `events::subscribe`
  bridge — boot `Resync` + `Lagged`→`Resync`, GTK-parity lag recovery —
  plus per-tab PTY, IPC `UiRequest`s, metrics, provider results) drained by
  a `Notify`-backed wake subscription with stable identity; the only
  timers left are three state-conditional ones (status 500ms,
  palette-geometry 16ms, attach-retry 25ms — the last closing the
  `TabOpened`-before-spawn race with GTK-parity bounded budget); mutations
  are async engine ops with op-id-guarded rename/reorder state machines
  and op-id-keyed deferred `palette.activate` replies; idle CPU measured
  ~73% → 0.0% in the shed.
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
* **3f. Native desktop notifications (complete — plan 015).** Shipped: a
  Linux D-Bus adapter via notify-rust 4.18 (`z-with-tokio`: zbus 5 rides
  the app's existing tokio runtime), a per-OS backend seam (non-Linux
  targets log and no-op), GTK-parity per-tab replace-not-stack semantics,
  and click-to-focus through the freedesktop default action → focus tab +
  clear badge + reveal sidebar + best-effort raise. Adapter-only; engine-side
  suppression is untouched. macOS backend and .desktop shell-grouping/icon
  follow-ups tracked in [#303].
* **3g. Wayland/drop gaps (documentation-complete — plan 015, tracked in
  [#302]).** Four upstream iced/winit limitations, pinned discipline
  track and document only — no in-process workarounds, no upstream
  engagement: no drop coordinates (`window::Event::FileDropped(path)`
  carries no cursor position, so exact hit-testing is impossible
  in-process); no text/URI-list drop events (winit exposes no raw-text or
  URI-list drop event at all); no native Wayland DnD (winit delivers no
  file-drop events under Wayland); and a clipboard seat-serial gap
  (Wayland clipboard writes need a seat serial the stack doesn't thread,
  so the iced clipboard e2e tier skips under Wayland — `Makefile`
  `e2e-iced`/`e2e-iced-ci` gate on `WAYLAND_DISPLAY`, documented in
  `ci.yml`). Exit condition: an iced/winit release that delivers any of
  these becomes its own adoption slice.

Slice order is deliberate: 3b closed the honest `Err("… not available in
Iced yet")` stubs — the grep now has zero hits — and 3c closed the last
functional gap blocking M4; 3d closed the
architecture cleanup that everything real-time depends on; 3f is done and
3g is documentation-complete via [#302] — only 3e (polish, user-directed)
remains open on this track.

### Maintenance backlog (filed, not scheduled)

Work this migration surfaced that should not block a slice. Pull one in
when it touches the code you are already in:

| item | issue |
|---|---|
| ~~Iced tab-strip artifacts with several tabs open~~ **fixed** (PR #291) | [#281] |
| ~~Iced SIGABRT: swash subtract-with-overflow panic during glyph shaping~~ **fixed** (PR #298: vendored patched swash 0.2.10 under `third_party/swash` via `[patch.crates-io]` — trigger was a zero-long-metrics font; crafted-font regression test) | [#292] |
| ~~**Security-adjacent:** Swift's dragged-URL drop branch unfiltered; Rust/Swift filter predicates diverge~~ **fixed** (PR #301: converged 8-scalar predicate + URL branch both languages, cross-pinned vectors) | [#282] |
| ~~`roost-linux` clippy `type_complexity` debt keeping it out of the lint gate~~ **fixed** (PR #301: gtk-build now runs the full `-D warnings` gate) | [#283] |
| ~~Mac `app.window_metrics` omits `terminal_top` / `terminal_font_family`~~ **fixed** (PR #301) | [#287] |
| ~~Iced hit-tests positionless presses at the batch-newest cursor~~ **fixed for `SidebarResizeGrip`** (PR #301, harness dwell removed); the `ReorderStrip` half is blocked by scrollable event/cursor space mismatch → split to [#300] | [#295] |
| Iced `ReorderStrip` presses still batch-cursor-anchored: iced scrollables pass children a translated cursor but the raw untranslated event, so an event-position anchor is off by the scroll offset | [#300] |
| Vendored swash: three further malformed-font robustness findings (incl. a verified upstream format-4 bitmap-index infinite loop — release-build hang) | [#299] |
| Terminal multi-click is wall-clock-only — **parked deliberately** (PR #301 triage): porting the strip's frame-grace would fuse deliberate slow clicks into word-select on idle terminals (a click schedules a redraw, so a 1-2 frame gap is the normal idle signature, not a stall); revisit only if it actually flakes | [#297] |
| No CI gate for GTK↔Iced visual parity (capture tooling is human-reviewed) — decide "required or waived" as part of the M4 entry audit | [#284] |
| No real-input (CGEvent) harness on macOS — uinput tier is Linux-only | [#285] |
| `roost-engine::facade` has no consumer; prove it or delete it (blocks M5) | [#286] |
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
[#295]: https://github.com/charliek/roost/issues/295
[#297]: https://github.com/charliek/roost/issues/297
[#299]: https://github.com/charliek/roost/issues/299
[#300]: https://github.com/charliek/roost/issues/300
[#302]: https://github.com/charliek/roost/issues/302
[#303]: https://github.com/charliek/roost/issues/303

### M4 — ship Iced to Linux users

Release packaging + appcast/apt integration, a state-migration decision
(does Iced adopt the GTK profile's `state.json` or migrate it), a beta
period behind an explicit opt-in, then the GTK deprecation decision. Entry
criteria: M3 complete, parity inventory shows no open P0/P1, and the
real-input tier passes on Iced for the drag/clipboard guards.

Entry-criteria status (2026-08-05): the real-input criterion is **met** —
PR #301 removed the last harness workaround (the seam-press dwell) and the
full Iced drag/clipboard guard passes in the shed and on CI. The
no-open-P0/P1 criterion needs a **parity-inventory refresh audit** before
it can be evaluated honestly: rows can lag shipped slices (the
sidebar-resize row described the pre-3c fixed 220 pt width until the
plan 015 closeout caught it; others may hide the same drift). The audit
should re-verify every row against current behavior, close what shipped,
and decide [#284] (visual-parity CI gate: required for M4 or waived).
After plan 015, the open P1 set is expected to be exactly the 3e polish
scope plus the upstream-blocked drops row ([#302]) — the audit confirms
that expectation rather than assuming it.

### Possible direction — macOS side-by-side evaluation (not committed)

Recorded 2026-08-05: the Iced work has landed better than expected, and
replacing the Swift app is now a *possible* direction rather than a
non-goal — but it is not guaranteed, a lot of testing stands between here
and any commitment, and **Swift remains the production daily driver
regardless** (guardrail #1 unchanged). Two consequences today:

1. **Design Iced platform-clean now.** New roost-iced capability with a
   native surface gets a per-OS backend seam rather than a Linux-only
   shape — notifications (slice 3f) are the first instance: Linux D-Bus
   ships; the macOS backend (`UNUserNotificationCenter`, which needs a
   real .app bundle identity + code signature) is deferred, not
   designed out. No macOS backend gets half-shipped from an unbundled
   binary.
2. **M5 is frozen pending this direction** — see below.

If the direction firms up, the evaluation vehicle is a parallel-install
signed+notarized DMG (the `ai.stridelabs.Roost.iced` profile is already
fully isolated for side-by-side running), gated on a robustness pass:
release-profile CI coverage for Iced, the [#299] swash release-hang fix
(a Mac daily driver meets the full macOS font universe), a panic-hook
crash/feedback story, and a mac-UX gap audit (menu bar, cmd-key
conventions, dock badge — plus 3e's vibrancy spike). Guardrail #3's
"absent from release artifacts" would be amended consciously at that
point (separate opt-in artifact, never bundled into the Swift release).

### M5 — Rust under Swift (exploration, frozen)

Frozen 2026-08-05 pending the possible-direction note above: if a
full Iced replacement is evaluated, a Swift-facing FFI boundary is
likely wasted investment — [#286] holds at "don't invest, don't
delete" until the direction resolves. Original exploration plan kept
below for when/if it resumes.

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
