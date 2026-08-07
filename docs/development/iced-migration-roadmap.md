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
* **3e. Polish parity batch 1 (complete — plan 016).** Shipped: Inter
  bundled as the chrome font (`third_party/inter/`, static Regular/
  Medium/SemiBold registered under one cosmic-text family, single
  `chrome_font()` seam); the Mac color system (`BAND #24292c` for the
  sidebar header/footer + tab band, `LIST #2d3235` for the scrollable
  project list, a `DIVIDER` hairline drawn *inside* the sidebar's own
  width so `terminal_grid` keeps every pixel at zero inset); metrics
  matched to the measured Mac reference (`BAND_HEIGHT` 32, `ROW_HEIGHT`
  32 — the plan's draft value of 24 was corrected to 32 against the
  frozen reference mid-implementation; `TERMINAL_PADDING` 0 on all
  sides); one unified project-row layout serving both the metrics and
  the notification dots (a 3px rollup rail at the row's leading edge
  gapped 5px top/bottom, a selection pill inset 6px horizontal/1px
  vertical with radius 6, label aligned with the agent rows' left
  edge); the header bell removed in favor of Mac-parity sidebar
  notification dots (any project with a notifying tab, active project
  included) alongside the existing pill badges — the palette/keybind
  remain the inbox entry; the ☰ collapsed-sidebar button removed with
  no replacement in-window affordance (Mac parity — reopen via
  keybind/palette); the in-strip `+` now scrolls with the pills and
  can go offscreen under overflow (Mac-parity accepted behavior — the
  Mac's own ＋ scrolls with the strip too); drag-to-collapse below
  half the 160pt floor (80px, unclamped) collapses the sidebar and
  reopens at its pre-drag committed width (recorded parity divergence:
  NSSplitView lets a drag continue past the floor and re-expand within
  the same gesture — here the grip leaves the tree at collapse, so
  drag-back-to-reopen within one gesture is impossible); a dynamic
  window title (`{project} – {abbreviated cwd}`, pure helper in
  `roost-ui-model/src/window_title.rs`) plus a transparent titlebar on
  macOS rendering the band color; and the tab-strip pixel guard
  updated for the new chrome plus a new `test_z_typography.py`, both
  wired into all three ci.yml iced lanes (closing a CI gap — they were
  in Makefile `ICED_E2E_TESTS` but no ci.yml list, so the scrollbar/
  band pixel guard never ran on CI for iced before this). Two items
  flagged for Charlie's review rather than resolved by the plan: the
  divider hairline shipped as `#1a1d1e`, sampled during implementation
  rather than matched to the Mac's literal black divider (a
  one-constant change if he wants it darker); and the zero
  left-terminal-inset against the divider may read cramped (his call,
  not reworked speculatively). The tab-strip artifact bug ([#281])
  that was folded into 3e's original scope is **fixed** (PR #291,
  merged 2026-08-04): the strip scrollbar is `Scrollbar::hidden()` and
  `tools/roosttest/test_tab_strip_pixels.py` guards it. The remainder
  of 3e's original scope — hover-close, offscreen-tab reveal, empty/
  loading/error states, cursor/selection/link pixel geometry, and the
  frosted/translucent spike — split out as **3h** below (plan 016,
  W8); do not read those as still-3e.
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
* **3h. Polish parity II (user-directed).** The remainder of 3e's
  original scope, split out by plan 016 (W8) once batch 1 (chrome
  parity) shipped: typography glyph-baseline comparison; cursor/
  selection/link pixel geometry; empty/loading/error states, including
  the empty-workspace state after the last project is deleted (today
  iced lands in the engine's empty workspace state while both shipped
  UIs close their window instead); confirm-overlay pointer modality
  tightening (it blocks presses but, like the palette, passes wheel/
  hover through); hover/focus/disabled state styles; the frosted/
  translucent window-vibrancy spike (likely `window-vibrancy` over a
  transparent iced window on macOS, compositor-dependent on Linux —
  decision is Charlie's); agent-row height/typography distinctness;
  tab status dot/label geometry; the hover-close decision (note: the
  shipped Mac shows × only on the active pill with no hover reveal —
  App.swift:4747 — so adding hover-close to iced would be a *product*
  decision, not a parity port); and offscreen-tab reveal after
  programmatic selection. **Plan this slice with the user in the
  loop — they want to give detailed direction here,** as with 3e.

Slice order is deliberate: 3b closed the honest `Err("… not available in
Iced yet")` stubs — the grep now has zero hits — and 3c closed the last
functional gap blocking M4; 3d closed the
architecture cleanup that everything real-time depends on; 3f is done and
3g is documentation-complete via [#302]; 3e closed chrome parity batch 1
(plan 016) — only 3h (polish parity II, user-directed) remains open on
this track.

### Engine track (E) — shared renderer + robustness

Deliberately **not** M3 slices. These span `roost-vt` / `roost-ui-model`
and land in all three UIs, so they are not part of "Iced functional
parity"; they run alongside M3/M4 rather than gating either. Lettered `E`
to avoid colliding with the parity inventory's P0/P1/P2 *priority* labels.

Recorded 2026-08-06 from a Ghostty-comparison discovery pass. The headline
finding: **libghostty-vt's render-state dirty tracking is exposed in the
pinned header and wrapped by nobody** — not Swift, not GTK, not Iced. All
three walk the full grid on every update.

The E-entries below were written as predictions before any of this
shipped; where the measured result corrected one, that correction is
recorded alongside it rather than quietly edited away.

#### Results scoreboard (E1–E3b, measured) and what's deferred

The per-entry prose below carries the full evidence; this is the
scoreboard. All numbers are release builds; workloads are E1's W1–W3.

| surface | metric | before | after | change |
|---|---|---|---|---|
| iced refresh, W1 pointer-motion storm | ns/refresh | 107,605 | 956 | **113×** |
| iced refresh, W2 in-place TUI redraw | ns/refresh | 110,025 | 6,163 | **17.9×** |
| iced refresh, W3 scroll (control) | ns/refresh | 116,609 | 57,473 | 2.0× (allocation removal only — see E3's limitation) |
| GTK refresh, idle/blink frames | per frame | 1.09 ms | 13.6 µs | **~80×**, zero rows rebuilt |
| GTK scroll burst (300 lines) | rebuilds | every frame | once | — |
| E4 run coalescing | — | — | — | **NO-GO**: release draw cost was already 172 µs/frame; floored-grid drift up to +50 px @ 60 cols |

Not perf but shipped in the same track: E7 crash robustness (panic
hooks + crash files in both Rust UIs; #299's malformed-font guards) and
E5 sprite parity in iced (shared `roost_ui_model::sprite` geometry).

**Consider-later items are tracked in [#309]** (renderer-performance
deferred considerations): the GTK *draw* phase (15–25 ms/frame Xvfb,
the one measured cost still standing — with the oracle warning), E8's
two-phase update, E4's exit conditions, the scroll full-rebuild
constraint, wgpu cache sensitivity, and iced release-profile CI. Pull
from there rather than re-deriving.

[#309]: https://github.com/charliek/roost/issues/309

* **E1. Renderer baseline measurement — done, prediction corrected.**
  The original plan was frame time under a full-screen scrolling TUI at
  max window size. That workload is precisely the one E3 cannot move:
  libghostty full-rebuilds its render state whenever the viewport pin
  changes ("If our viewport pin changed, we do a full rebuild" —
  `third_party/ghostty/src/src/terminal/render.zig:299-302`), so any
  output that scrolls is a full rebuild by construction, independent of
  our dirty-tracking wrapper. Measuring only that would have produced a
  near-flat number and fed a false "no win here" into E4's go/no-go.
  What actually shipped: deterministic per-tab counters
  (`refresh_calls` / `refresh_nanos` / `rows_rebuilt` / `cells_walked`,
  plus `draw_calls` / `draw_nanos` / `fill_text_calls` for the
  draw path) and CPU-side span timings, exercised by three workloads —
  W1 pointer-motion storm, W2 in-place TUI redraw, W3 scrolling stream
  as an explicit control expected to stay flat — readable three ways:
  CI unit tests, an `#[ignore]`d in-crate harness (`make perf-refresh`),
  and the `app.render_stats` IPC op via `roostctl render-stats`
  (`make perf-render-stats`), which is the only way to get draw-path
  numbers at all (no unit test constructs a live `iced::Renderer`).
  Locked-Mac caveat: presented frame rate is meaningless on a locked or
  occluded Mac (macOS throttles presentation), which is why this
  measures CPU spans and counters instead. See `tools/perf/README.md`
  for the harness.
* **E2. Render-state dirty coverage — done.** Shipped in
  `crates/roost-vt/src/render_state.rs`: `Dirty { Clean, Partial, Full }`,
  `dirty()`, `mark_full()`, `dirty_rows()`, `walk_dirty()`. The footgun
  (render.h's "extremely important detail" that clearing one dirty layer
  does not clear the other) is handled by making consumption imply
  reset — `walk_dirty` clears BOTH layers together, and there is
  deliberately no public way to lower the dirty state otherwise: a
  general `set_dirty` would have made `set_dirty(Clean)` the very
  footgun this wrapper exists to prevent. `walk` is unchanged, so E2 is
  purely additive and GTK is untouched by it. 14 tests in
  `crates/roost-vt/tests/render_dirty_test.rs` pin the measured
  semantics.
  **Two-phase `begin_update` / `end_update` is NOT available at our pin** —
  it landed upstream after `c74f6d5` (present at `../ghostty` tip and in
  `../libghostty-rs`, absent from our generated bindings). It is an E8
  follow-on, not part of E2; don't plan around it.
  `../libghostty-rs`'s `crates/libghostty-vt/src/render.rs` is MIT and a
  useful reference for the dirty accessors regardless.
* **E3. Dirty-row snapshot rebuild — done for `roost-iced`.** Consumes
  E2. `refresh_snapshot` (`app/terminal_tab.rs`) used to rebuild the
  whole grid per PTY update, allocating `vec![vec![String::new(); cols];
  rows]` — a `String` per cell including blanks — plus a `String` per
  `DrawCell`. Shipped shape: `TerminalSnapshot`'s `cells` + `rows_text`
  became `grid: Vec<Arc<RenderedRow>>`, `DrawCell` lost its `row` field
  so the row index has exactly one source of truth (the owning
  `RenderedRow`'s position), and three invalidation guards force a full
  rebuild outside `walk_dirty`'s own signal: grid size on both axes,
  the default fg/bg pair, and a theme generation counter.
  Measured (`refresh_snapshot`, `--release`, N=200/workload, before =
  `f3e2657` pre-E3, after = this branch's HEAD, both built fresh in a
  `git worktree` back-to-back):

  | workload | before ns/refresh | after ns/refresh | speedup | rows rebuilt/refresh |
  |---|---|---|---|---|
  | W1 pointer-motion storm | 107,605 | 956 | 113x | 32.00 → 0.16 |
  | W2 in-place TUI redraw | 110,025 | 6,163 | 17.9x | 32.00 → 2.15 |
  | W3 scrolling stream (control) | 116,609 | 57,473 | 2.0x | 32.00 → 27.50 |

  **Important limitation:** streaming/scrolling output gets **no
  dirty-tracking benefit** — W3 barely moves in rows rebuilt (32.00 →
  27.50) because libghostty full-rebuilds on any viewport-pin change, per
  E1's correction above. Its 2.0x clock gain is a *separate* effect: E3
  also deleted the dense `vec![vec![String::new(); cols]; rows]`
  allocation, which cost a `String` per cell including blanks, so even
  full rebuilds got cheaper. Judge W3 on rows/refresh staying high, not
  on its timing. The dirty-tracking wins are concentrated in in-place TUI
  redraws and the non-PTY refresh callers — above all mouse motion (W1),
  which previously rebuilt the entire grid on every pointer event for
  zero content change.
  **libghostty limitation worked around:** `OSC 10`/`OSC 11` and DECSCNM
  change the reported default colors while libghostty reports
  `dirty=Clean` with zero rows flagged — the dirty API alone can't see
  it — so the consumer must compare the default fg/bg pair itself
  (the `cached_defaults` guard above). Tripwire tests in
  `crates/roost-vt/tests/render_dirty_test.rs` will fail loudly if a
  future Ghostty bump changes this.
* **E3b. Dirty-row rebuild for GTK + real `render_stats` — done
  (plan 018).** Shipped, gated by a measurement the same way E4 was
  killed by one: C3 landed counters + a renderer-free `refresh_cache`
  seam first, and the cache proceeded only after the walk measured
  **1.09 ms/frame** (release, shed) on blink-driven idle frames — past
  the pre-stated go threshold. Shipped shape mirrors iced's E3 with
  GTK's inputs: `RenderedRow { bg, glyphs }` grid on `TerminalViewState`,
  guards on `(cols, rows)` + a `GuardKey` of the resolver's actual
  inputs (theme fg/bg/`bold_color` — the last never pushed into
  libghostty, so that guard alone catches a bold_color-only change —
  plus cell metrics) + a theme generation; cursor glyph looked up from
  the cache at paint time (a blink frame visits zero rows, so the old
  during-the-walk capture would have blanked the block cursor's glyph).
  `app.render_stats` on GTK returns real counters. Measured (release,
  shed): idle/blink refresh **1.09 ms → 13.6 µs/frame** (~80×), zero
  rows rebuilt; a 300-line scroll burst rebuilds once instead of every
  frame. **Follow-up recorded, not done:** the *draw* phase is GTK's
  real cost — 15–25 ms/frame under Xvfb, ~2,400 per-cell pango
  `set_text`+`show_layout` calls repeated in full on every blink frame.
  Routes: per-row damage clipping of the Cairo phase, and/or pango-side
  run drawing — which, unlike iced, has drift-free designs (negative
  `letter_spacing`, per-glyph x-positioning; see E4's corrected scope).
  Xvfb-software-rendering caveat recorded with the numbers in plan 018's
  artifacts.
* **E4. Run coalescing — measured NO-GO (2026-08-06, plan 018).** The
  entry below first recorded a GO from plan 017's numbers; further
  measurement the same day reversed it, and the reversal is exactly why
  the "do not start without the number" gate existed. Three
  disqualifiers, strongest first:
  1. **Grid drift.** Both UIs position cells at the **floored** font
     advance (`crates/roost-iced/src/terminal_widget.rs` `measured.width.floor()`;
     `crates/roost-linux/src/cell_metrics.rs:60-63`, floored against
     smearing) while a shaped run advances at the natural fractional
     width — measured 8.83 px vs 8 at 11 pt, so a coalesced 60-cell run
     lands up to **+50 px** off the grid. Shaping is exactly linear
     (60×"M" = 60 × one "M"; mixed ASCII likewise), so the drift is
     purely the floor. Precisely scoped: this forecloses **naive run
     drawing without per-glyph positioning** — pango offers two
     drift-free designs (a negative `letter_spacing` attribute forcing
     per-cell advance, or shaping once and overriding per-glyph
     x-positions via glyph-string draw), and iced offers none at its
     public `fill_text` layer. Un-flooring would fix it globally but is
     a user-visible geometry change (cols per window width, hit-testing,
     selection, every pixel test) and would break the sprite renderer,
     which tiles box-drawing glyphs by integer `cell_w`/`cell_h` —
     reintroducing the seams sprites exist to fix.
  2. **The motivating cost was a debug artifact.** The GO cited
     ~1.37 ms/draw; a **release** build on the same host and workload
     measures **172 µs/draw** for a worst-case full-screen dense redraw
     — ~1% of a 60 fps frame budget. (`fill_text_calls` ≈ 2,410/draw is
     build-independent; the cost it implied was not.)
  3. `iced_wgpu` keys shaped-text caching on the content string:
     per-cell single chars repeat massively (cache-friendly); unique
     coalesced run strings would miss every frame under scrolling.
     (Argument, not measurement — recorded as such.)
  Probe source + raw numbers archived with plan 018
  (`e4-nogo-evidence.txt`). The W1–W3 harness and `fill_text_calls`
  counter stay — they are what made this decision cheap. **Exit
  conditions: the grid stops flooring, OR a per-glyph-positioned draw
  path exists** (pango glyph strings today, an iced glyph-atlas path if
  E5-adjacent work ever builds one) — and in either case only with a
  release-build number showing a cost worth chasing, which today's
  172 µs is not.
* **E5. Sprite parity in Iced.** ✅ **Complete (plan 020).** The gap:
  `mac/Sources/Roost/Sprite.swift` and `crates/roost-linux/src/sprite.rs`
  draw U+2500–U+259F (box-drawing, block elements) geometrically because
  font glyphs don't tile pixel-perfectly across adjacent cells, and
  `roost-iced` had no sprite path at all — hairline seams in TUI chrome
  that neither shipped UI has. Shipped shape: the geometry moved to
  `roost_ui_model::sprite` as pure-data primitives (rects+alpha, corner
  arcs, diagonals), with `arc_path` as the single source of path geometry
  and `tessellate` flattening the stroked primitives into stamped rects
  for quad-only renderers. GTK's `sprite.rs` became a thin cairo adapter
  (2,715 → 569 lines) with behavior pinned three ways: the unchanged
  pixel-assertion suite, a committed golden-hash fixture (153 codepoints
  × 3 cell sizes, generated from the pre-refactor renderer; the 7 stroked
  glyphs excluded as cairo-version-sensitive), and byte-identical
  verification. iced draws sprites as integer-edge-snapped `fill_quad`s
  inside the glyph pass — iced has no per-quad AA switch, so edge
  snapping is the seam mechanism there — and sprite draws count into
  `fill_text_calls` on both UIs, making that counter apples-to-apples
  (the old cross-UI caveat is resolved). e2e guard:
  `tools/roosttest/test_sprite_pixels.py` (seam + internal-edge + counter
  assertions) in all three iced CI lanes. Cross-UI differences recorded,
  decided not discovered: wgpu blends the shade glyphs' (░▒▓) alpha in
  linear space so iced shades render lighter than cairo's sRGB blend;
  arcs/diagonals are stamped-quad staircases vs cairo's AA strokes; and
  iced's interleaved bg+glyph draw order can shave diagonal overshoot at
  explicit-bg boundaries where GTK's two-pass order preserves it. No CI
  cross-UI pixel gate, per [#284].
* **E6. IME input.** ✅ **Complete (plan 021).** Discovery corrected the
  premise: *neither shipped UI* had terminal IME either (GTK's terminal
  is a bare `EventControllerKey` with no `IMContext` — the deferral is
  documented at `crates/roost-linux/src/key_encoder.rs:21-24`; Swift's
  `TerminalView` never adopts `NSTextInputClient` — `KeyEncoder.swift:
  151-153`), so this slice made iced the **first** Roost UI with working
  terminal IME rather than a parity port. Shipped shape: the terminal
  widget consumes `Event::InputMethod` (the app-level subscription drops
  non-keyboard events by design) and re-requests the input method every
  `RedrawRequested` — `Enabled` with a caret-tracking cursor rect and
  `Purpose::Terminal` only while the terminal owns the keyboard route
  and the window is focused, `Disabled` otherwise, always with
  `preedit: None` so iced_winit's own overlay stays cleared. Commits
  encode through the libghostty encoder (`UNIDENTIFIED` + utf8, its
  documented composed-input path) into the per-tab write path, routed to
  the preedit-holding tab. The preedit is a draw-time overlay at the
  cursor cell (terminal font, selection background, underline,
  cell-width-aware, right-aligned on overflow, suppressed while the
  cursor is hidden) — never a grid mutation. Clearing is
  cancel-not-commit (window unfocus, tab switch, rename/palette/confirm);
  a one-shot discard latch drops the commit the OS may still deliver for
  a canceled composition, so canceled text cannot type into whichever
  tab now owns the route, while preedit-less commits (the emoji-picker
  path) still land. While composing, key presses carry `composing=true`
  (libghostty verifiably swallows them) and accelerator dispatch waits —
  a pinned decision (no Cmd+V mid-composition). Coverage: unit vectors
  model winit's actual delivery (dead key, candidate-selection Enter,
  cancel, discard-latch state machine, geometry incl. the caret slide);
  `tools/roosttest/test_ime.py` drives the `tab.feed_ime` test-mode op
  (three-layer sibling of `tab.feed_pty_bytes`; GTK rejects it as
  iced-only) through the same production App methods in all three iced
  CI lanes. Honest residuals: the widget's `InputMethod` arm/per-frame
  request wiring and real dead-key/CJK/emoji behavior are only provable
  with a live OS IME (human smoke — plan-021 checklist); the OS
  candidate window can linger after a tab-switch cancel (the latch keeps
  it off the PTY; abandoning it OS-side needs a one-frame `Disabled`
  pulse, deferred until real-IME verification); macOS press-and-hold vs
  terminal auto-repeat is unobservable by any automated gate (checklist).
  The parity-inventory Terminal IME row tracks the cross-UI picture.
* **E7. Crash robustness.** ✅ **Complete (plan 019).** Shipped shape:
  `roost-engine::crash::install_panic_hook` — one shared formatter/
  writer; both Rust UI mains install it right after logging init and
  before the single-instance lock. A panic on any thread writes
  `crash-<secs>-<pid>.txt` next to `roost.log` (payload, location,
  backtrace via `force_capture`, version, OS), copies the report to
  stderr, logs one correlating line, then **aborts** — no
  catch-and-continue, since both UIs hold unsafe FFI state.
  `ROOST_TEST_PANIC` (`=1` main thread, `=thread` named background
  thread) forces the path; spawn-based integration tests run the real
  binaries hermetically and assert the artifact end-to-end. [#299]
  closed alongside: five malformed-font guards in vendored swash
  (format-4 bitmap-index infinite loop — the release-build hang — plus
  three debug underflows in `var.rs` and a both-profiles OOB in
  `string.rs`), each with a crafted-font regression test; the hang
  test is timeout-capped so a regression fails instead of wedging CI.
  Swift Mac deliberately keeps no hook (parity deviation, tracked
  under the M6 decision). Both M6 entry-gate halves are done.
* **E8. Ghostty pin + zig bump** — *sequenced after the M6 direction
  resolves.* `third_party/ghostty/build.sh` pins `c74f6d5` (2026-04-25)
  against zig 0.15.2 (`.mise.toml`); `../libghostty-rs` pins `ab0b9da`
  (2026-07-22, **603 commits ahead**) and needs zig 0.16.x. Wanted
  eventually regardless. A large share of the cost is revalidating the
  **Swift** build — `mac/Package.swift` links the same static archive —
  so the price drops sharply if Swift is retiring. That is why this sits
  after the M6 decision, not before it. Carries E2's deferred half: the
  two-phase `begin_update` / `end_update` split arrives with the newer
  pin, letting the renderer drop the terminal lock before the deferred
  work — a latency win under heavy PTY output.
* **E9. `libghostty-rs` integration spike + library audit.** Depends on
  E8. Checked out at `../libghostty-rs` (tip `72ac98f`, 2026-07-29, MIT,
  not on crates.io — fine, `publish = false`). It is broader than
  `roost-vt` (osc, kitty graphics, paste, sgr, screen, unicode, focus)
  and more rigorous (borrow-checked lifetimes, typestate updates). Two
  integration notes: `GHOSTTY_SOURCE_DIR` can point its `build.rs` at our
  existing checkout, and a `pkg-config` feature can discover a
  pre-built archive — worth a spike so we don't run zig twice. Adopting
  it dissolves the `render_state.rs` ↔ `RenderState.swift` 1:1 parity
  correspondence, which is a *cost* while Swift lives and a non-issue
  after. Audit for further wins once integrated.

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
| ~~Vendored swash: three further malformed-font robustness findings (incl. a verified upstream format-4 bitmap-index infinite loop — release-build hang)~~ **fixed** (plan 019: five guards in `third_party/swash` — strike.rs bisection typo, var.rs ×3 debug underflows, string.rs OOB read — each with a crafted-font regression test; the format-4 record-offset off-by-one is documented residual, README.roost.md has the delta list) | [#299] |
| Terminal multi-click is wall-clock-only — **parked deliberately** (PR #301 triage): porting the strip's frame-grace would fuse deliberate slow clicks into word-select on idle terminals (a click schedules a redraw, so a 1-2 frame gap is the normal idle signature, not a stall); revisit only if it actually flakes | [#297] |
| ~~No CI gate for GTK↔Iced visual parity~~ **resolved (2026-08-07)**: no cross-UI gate ever; the parity capture tooling is convergence scaffolding, deleted when GTK retires (disposition in the M4 block) | [#284] |
| No real-input (CGEvent) harness on macOS — uinput tier is Linux-only | [#285] |
| `roost-engine::facade` has no consumer; prove it or delete it (blocks M5) | [#286] |
| `app/interactions.rs` at 2,960 lines — finer split when fixtures allow | [#288] |
| swift-testing runner SIGABRT on fast value-check swarms (XCTest workaround) | [#289] |
| **OSC consolidation — watch item, do not act.** `roost-osc` cannot fold into libghostty's OSC parser: `GhosttyOscCommandData` exposes exactly one payload accessor (`CHANGE_WINDOW_TITLE_STR`), identical in our pinned header and in `../ghostty` tip, so a pin bump does not help. libghostty discriminates 22 command *types* but hands back no data for OSC 7 / 9 / 10-12 / 4 / 133 / 52 / 22 — 7 of the 8 events `OscEvent` needs. The custom parts (percent-decode + `file://` extraction, ConEmu OSC 9 sub-command filtering, OSC 52 base64 decode + refuse-on-truncation, `MAX_BODY`, reply synthesis) are policy with no C-API counterpart and would survive anyway. **Exit condition:** libghostty-vt adds `GHOSTTY_OSC_DATA_*` accessors beyond window title — then re-evaluate. | — |

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
[#311]: https://github.com/charliek/roost/issues/311
[#303]: https://github.com/charliek/roost/issues/303

### M4 — ship Iced to Linux users

Release packaging + appcast/apt integration, then the GTK deprecation
decision. Entry criteria: M3 complete, parity inventory shows no open
P0/P1, and the real-input tier passes on Iced for the drag/clipboard
guards.

**Decisions (Charlie, 2026-08-07):**

* **State migration — adopt the GTK profile in place.** The release deb's
  iced build ships as `roost` and uses the existing GTK profile paths on
  Linux — same `state.json`, same socket
  (`$XDG_RUNTIME_DIR/roost/roost.sock`), same log dir — so existing users
  keep projects/tabs transparently and `roostctl`/Claude hooks keep
  working unchanged. Dev builds keep the separate `roost-iced` profile
  for side-by-side development.
* **Deb composition — clean swap.** The next deb release ships only the
  iced UI; GTK leaves the package (its source stays in-repo until the
  separate retirement decision). No beta/opt-in phase — the swap rides
  the next regular release, whenever Charlie cuts it (he is explicitly
  not ready to release yet; the packaging work proceeds now so the next
  release simply has it).

Entry-criteria status (2026-08-05): the real-input criterion is **met** —
PR #301 removed the last harness workaround (the seam-press dwell) and the
full Iced drag/clipboard guard passes in the shed and on CI.

**Refresh audit result (2026-08-07, plan 021, main@166d2d6).** Every
inventory row was re-verified with named evidence (suite runs on macOS +
shed Linux both renderers, hermetic parity captures, code refs — method
block in the inventory). Six rows were stale in iced's favor and closed
(the Sidebar-footer P0 and the create/delete/reorder/New-Project-command
functional rows all described the pre-plan-010 world; workspace-shortcut
adapters were already complete), several closed rows had drifted prose
corrected, and four rows were added under the "is there a row for this at
all?" question (terminal IME; window vibrancy; context menus; terminal
bell — the last a recorded all-three-UIs absence, not an iced gap). The
audited open set is:

* **Open P0: none.**
* **Open P1:** (a) the user-directed **3h polish cluster** (3h above
  names the authoritative full scope) — agent-row typography, tab-status
  geometry fixture, hover-close product decision, glyph-baseline
  comparison (typography + padding halves), cursor/selection/link pixel
  geometry, empty/loading/error states, remaining hover/focus states,
  offscreen-tab reveal, and confirm-overlay pointer modality; (b) **file/image drops**, upstream-blocked
  ([#302]); (c) **terminal IME** — closed by engine slice E6 (plan 021,
  merged with this entry's edit); (d) one objective one-constant fix: iced's tab notification badge
  is `#4e9af1` where GTK deliberately hardcodes the Mac's `#007aff` —
  found by the audit, filed as [#311].
* The subjective P1s in (a) are Charlie-directed by design; M4's
  "no open P0/P1" criterion therefore reduces to: land E6, fix the badge
  constant, and either complete or explicitly waive the 3h items and the
  #302-blocked remainder for the beta — his call, flagged in the plan-021
  checklist. To be precise about waiver semantics: the criterion itself is
  unchanged and this audit waives nothing — [#284]'s recommendation covers
  only the cross-toolkit CI-gate question, and any per-row waiver is an
  owner decision that must be recorded on the row in the inventory before
  M4 can be declared entered.

**[#284] — resolved (Charlie, 2026-08-07): no cross-UI visual-parity CI
gate, ever.** Cross-toolkit pixel identity is structurally unattainable
(per-platform text rasterization; wgpu's linear-space alpha blending vs
cairo sRGB — an accepted E5 divergence; per-capture native-chrome
ownership), and the plan-021 audit hit the fragility class directly: a
cairo/pango update shifted GTK's alpha-composited palette-selection color
and its AA text pixels enough to break `parity.py`'s exact matches on a
visually correct capture. The parity capture tooling
(`tools/screenshot/parity.py` + `tools/roosttest/parity_capture.py`) is
convergence-period scaffolding, never CI-wired: it gets **deleted when
GTK retires** (nothing cross-toolkit remains on Linux; the M6 evaluation
is the only other prospective consumer). The focused per-UI pixel guards
(sidebar, tab-strip, sprite, typography suites) are and remain the
CI-enforced regression layer.

### M5 — Rust under Swift (exploration, frozen)

Frozen 2026-08-05 pending M6 below: if a full Iced replacement is
evaluated, a Swift-facing FFI boundary is likely wasted investment —
[#286] holds at "don't invest, don't delete" until the direction
resolves. Original exploration plan kept below for when/if it resumes.

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

### M6 — macOS Iced (evaluation → parity)

**Runs parallel to M4, not after it.** The number follows M5 for document
order only; nothing here waits on shipping Iced to Linux users.

Status (2026-08-06): replacing the Swift app is now **likely** rather than
merely possible — but it is not committed, it is a ways off, and
**Swift remains the production daily driver throughout** (guardrail #1
unchanged). This section supersedes the earlier "Possible direction —
macOS side-by-side evaluation" note. Two standing consequences:

1. **Design Iced platform-clean now.** New roost-iced capability with a
   native surface gets a per-OS backend seam rather than a Linux-only
   shape — notifications (slice 3f) are the first instance: Linux D-Bus
   ships; the macOS backend is deferred, not designed out. No macOS
   backend gets half-shipped from an unbundled binary.
2. **M5 stays frozen** while this is live. M6 is the *opposite* direction
   from M5 (Iced replaces Swift vs. Rust under Swift); keep them separate
   so the frozen thing stays frozen.

**Entry gate:** E5 (plan 020) and E7 (plan 019) are both complete;
remaining: release-profile CI coverage for Iced, and the
parity-inventory audit clean. Note the reference bar here is the **Swift
app**, which is consistently stricter than M4's GTK bar — the inventory
tracks both, and M6 is the superset.

**Accepted regressions**, decided rather than discovered: accessibility.
AppKit gives the Swift sidebar and menus VoiceOver support for free; an
Iced canvas gives essentially none. Named here so it is a conscious trade.

Guardrail #3's "absent from release artifacts" is amended consciously at
6a: a separate opt-in artifact, never bundled into the Swift release.

Slices:

* **6a. Bundling + parallel install.** The foundation — Sparkle,
  notifications, and TCC testing all need real bundle identity, so
  nothing below starts until this lands. `mac/scripts/bundle.sh` is
  toolkit-agnostic apart from `swift build --show-bin-path`; fork or
  parameterize it over (binary, bundle id, plist). `make-dmg.sh` needs no
  change. `roost-iced` must also resolve its profile from its own bundle
  id rather than calling `BundleProfile::iced()` unconditionally.
  Decisions taken 2026-08-06:
    * **Display name `Roost-Iced`** (`CFBundleName` /
      `CFBundleDisplayName`; window title should match) — two apps called
      "Roost" in the Dock and Cmd-Tab is genuinely confusing.
      **Display-only.** `app_label` stays `Roost-iced` (lowercase `i`):
      it drives the socket dir, the log dir, and the `identify` wire
      response (`roost-ipc/src/messages.rs`), which roosttest asserts on,
      and case-changing it is a no-op on macOS but a breaking path change
      on Linux, where `roost-iced` also runs. Do not "fix" the
      inconsistency.
    * **Fresh, separate `state.json`** — no import, no migration. This is
      already what `BundleProfile::iced()` does, so it is *zero* work.
      Scoped to macOS side-by-side; it does **not** settle M4's Linux
      adopt-or-migrate question, where Iced eventually replaces GTK for
      the same users.
    * **Sparkle/appcast deferred to 6c.** 6a ships with no auto-update
      (`SUEnableAutomaticChecks` is already `false`).
  Side-by-side is mostly already solved and needs no new machinery:
  distinct bundle ids, per-profile socket/state/log paths with tests
  asserting distinctness (`roost-ipc/src/paths.rs`), per-profile
  single-instance locks. **Claude hooks need no per-app configuration** —
  `ROOST_SOCKET` is injected into every PTY child (`roost-engine/src/pty.rs`,
  `PtySupervisor.swift`) and sits at precedence #2 in target resolution,
  above `--target` and auto-detect, so a Claude session in an Iced tab
  dials the Iced socket automatically; `claude_settings_document()`
  (`roost-cli/src/main.rs`) bakes no socket or profile into
  `claude-settings.json`. One operational wrinkle to note in the install
  output, not fix in code: `claude install` writes `self_exe()`, so with
  two bundles the hook file points at whichever `roostctl` ran it last.
  Two bundle ids also mean two entries in System Settings › Notifications.
* **6b. Native shim seam.** One decision with three consumers (6c/6d/6e):
  call AppKit via `objc2` (already in the lockfile through winit) or
  build a small Swift static library behind a C ABI. Leaning Swift lib —
  it amortizes across Sparkle, menus, and notifications, and the Swift
  toolchain is already a build dependency. Decide with a spike, not in
  the abstract.
* **6c. Sparkle auto-update.** `mac/Package.swift` + `App.swift` use
  ~two API calls (`SPUStandardUpdaterController`, `checkForUpdates(_:)`);
  `bundle.sh` already embeds and inside-out-signs the framework, and
  `release.yml` already EdDSA-signs and publishes the appcast. The
  evaluation build needs a **separate feed or none** so the two apps do
  not offer each other's updates. Design note for the eventual cutover:
  Sparkle does not care what language wrote the app, so an Iced build
  shipping under the *same* bundle id with the same `SUPublicEDKey` and a
  higher `CFBundleVersion` upgrades existing Swift installs in place.
* **6d. Menu bar.** winit installs none, so Iced on macOS currently has
  no menus at all. `App.swift` builds ~35 items across App/File/View/
  Edit/Window plus a dynamic Window menu of tabs and projects. Options:
  `muda` (designed to sit alongside winit) or hand-rolled NSMenu via
  `objc2-app-kit`. The keybind story is *better* in Rust — the table
  already lives in `roost-ui-model`, so menu equivalents and the terminal
  key encoder read one source instead of Swift re-deriving them. Highest
  volume, lowest risk; also where "custom options later" becomes cheap
  once menu items are just `Message` variants.
* **6e. Desktop notifications, macOS backend.** Closes [#303]. The seam
  from slice 3f is backend-agnostic already (`notifications.rs`: worker,
  per-tab replace semantics, click routing); only `mod backend` is
  missing. `UNUserNotificationCenter` requires a bundled, signed app —
  hence the 6a dependency, and it cannot be validated from `make
  run-iced`. Match `mac/Sources/Roost/DesktopNotifications.swift`
  semantics. Replace-by-server-id becomes UN's stable per-tab identifier,
  which is simpler than the D-Bus version.
* **6f. Window vibrancy.** May partly land in 3h, which already owns the
  spike. Worth knowing before starting: there is **no `NSVisualEffectView`
  anywhere in the Swift source** — the sidebar's translucency is AppKit's
  implicit source-list material (`outline.style = .sourceList` plus
  `scrollView.drawsBackground = false` and no pane fill), and the Swift
  code only ever works *around* it. So there is nothing to port 1:1;
  Iced must build it explicitly. The seam is
  `iced::window::run(id, |w: &dyn Window| …)` — a main-thread
  `HasWindowHandle`, with `raw-window-handle` at 0.6.2, the version
  `window-vibrancy` wants. Same call is the hook for Sparkle init, NSMenu
  install, and dock badge. Do **not** use `window::Settings { blur }`:
  winit's macOS impl calls the private `CGSSetWindowBackgroundBlurRadius`
  SPI and blurs the whole window, not a region. The effect view sits
  behind the wgpu surface, so the terminal region must stay opaque.
* **6g. macOS platform hygiene.** Individually small, collectively a
  pass. Entitlements + purpose strings first: Roost is the TCC
  *responsible app* for every child process in a tab, so without
  `device.audio-input` / `device.camera` / `automation.apple-events` a
  `/voice` or `osascript` in a tab fails **silently** — no prompt, no
  error (see the rationale comment in `mac/Resources/Roost.entitlements`,
  including which entitlements we deliberately omit). Then: Dock badge +
  dock menu, `NSApplicationDelegate` lifecycle (reopen-from-Dock,
  open-file/URL, graceful terminate), activation policy, and Secure
  Keyboard Entry (`EnableSecureEventInput` — a terminal convention
  neither app has today).
* **6h. macOS verification tier.** What makes the parity claim honest
  instead of hand-verified: a CGEvent real-input harness ([#285] — the
  uinput tier is Linux-only, a gap that matters far more once Mac is a
  target), `e2e-iced-mac` as a required gate, and release-profile CI
  coverage for Iced. Remember the enumerated-list trap: new roosttest
  modules need `ICED_E2E_TESTS` in the `Makefile` *and* the `ci.yml`
  lists, or they never run.

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
