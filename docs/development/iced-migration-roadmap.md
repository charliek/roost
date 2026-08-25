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
2. **~~The GTK app ships to Linux users.~~ Superseded by M4 (plan 022):**
   the `.deb` now ships Iced as `/usr/bin/roost` on the production GTK
   bundle profile. GTK stays in-repo as the development and parity
   implementation, still gated by `e2e-gtk` and the Wayland drag guard,
   still receiving fixes rather than new investment — but it is no longer
   what Linux users install. Retirement remains a separate decision.
3. **`roost-iced` may live on `main` incomplete** — with the M4 caveat that
   this no longer holds for the Linux package. Off Linux, and in every dev
   build, it stays isolated (own binary, `ai.stridelabs.Roost.iced` profile,
   own socket/state/log paths). What a *packaged* Linux build does is the
   opposite by design: it adopts the production profile, so incompleteness
   there reaches users. That is what the M4 entry criteria and the parity
   inventory exist to gate.
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
  Linux adapter on `org.freedesktop.Notifications` (Desktop Notifications
  spec; zbus 5 on the app tokio runtime), a per-OS backend seam (non-Linux
  targets log and no-op), GTK-parity per-tab replace-not-stack semantics,
  and click-to-focus through the spec `default` action → focus tab +
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
* **3h. Polish parity II — mostly complete (plan 026,
  `feature/plan-026-mac-polish`).** The remainder of 3e's original
  scope, split out by plan 016 (W8), was resolved item-by-item in a
  guided side-by-side walkthrough with Charlie on 2026-08-15 (Swift +
  iced dev builds, main@0a72119) that produced sixteen work items; the
  full record is
  `~/.claude/plans/roost/026-mac-polish/walkthrough-verdicts.md`.
  Shipped, all Charlie-directed: the cursor visual-style mapping was
  TRANSPOSED against the vendored header (bar/block swapped, so
  DECSCUSR 1/2 vs 5/6 rendered swapped too) — fixed at the roost-vt
  source, so GTK inherits the correction — plus a hollow-outline cursor
  on window-unfocus threaded into `TerminalWidget` (mac parity; cursor
  color stays theme/OSC-12-driven, no hardcoded constant); ctrl+letter
  and ctrl+[ \ ] _ chords recovered from winit's control-transformed
  `text` field via a `ControlChord` split, byte-identical to the Swift
  app for every chord it emits (ctrl+[ is the one exception — see
  below); same-cell drag reports suppressed via a `DragCellGate` beside
  `MotionEmitter` (fixes strix's double-click detection; iced-wired
  only, GTK/mac don't exhibit the bug); OSC 10/11/12/4 query replies
  moved to the PTY-drain path (0.1–0.5 ms, mac parity, was 1–12 ms —
  fixes a `prox status` reply leaking into the shell prompt as
  `11;rgb:…`); tab strip: active-tab reveal on every observed
  activation (keyboard, palette, click, or IPC) via `reconcile()`, pill
  truncation at mac's min 80 / max 220 with a real tail ellipsis, the
  bare-click accent-border flash removed (the visual drag state now
  waits for a real drag threshold instead of arming on press), pill
  hover recolor replaced with a red tint on the × glyph only (no
  pill-background hover), notification dot 8→9px; one `ACCENT`
  (`#007aff`) replacing the `NOTIFICATION`/`NOTIFICATION_BADGE` split
  (closes [#321]); the rename editor gets the mac near-black editing
  field plus an accent focus ring/selection; the confirm-delete overlay
  restyled to the mac `closeActiveProject` alert (bundled app icon,
  "Close \<name\>?" copy, a live tab count, Cancel/Close Project
  buttons) and now SURVIVES window focus loss (a `FocusTeardown` policy
  table keeps `confirm_delete` alive while rename/drag/IME still
  cancel); exit-on-empty-workspace (last tab closes its project via the
  existing engine cascade; last project close/delete — from the UI,
  palette, keybind, or IPC — exits the app, matching the Swift window-
  close policy; the check lives in `reconcile()` and an `UiTask::Exit`
  variant so App drops and flushes `state.json` on every path) with the
  "Starting terminal…" placeholder replaced by a plain background fill;
  U+23FA and similar default-text-presentation, no-VS16 codepoints now
  render monochrome instead of falling into Apple Color Emoji's
  cosmic-text cascade; and footer/label/palette measured against the
  live mac and matched (footer 8px/12px chip padding, active/inactive
  project-label color, palette row insets, scrollbar hidden, the
  filter-input divider removed, top-row scroll-clipping fixed).
  Typography glyph-baseline comparison, selection geometry, link hover,
  sidebar-row hover, and agent-row presentation all PASSED as-is in the
  walkthrough (byte/pixel evidence in the record) — no code needed.
  Decisions recorded: hover-close stays ×-on-the-active-pill-only with
  a red hover tint — no hover-reveal on inactive pills, since that
  would be a *product* change rather than a parity port, per the
  original 3e framing; click-outside-cancel on the confirm overlay is
  KEPT (deliberate, re-pinned); ctrl+[ now emits the fixterms-canonical
  `\x1b[91;5u` (libghostty's `ctrlSeq` deliberately excludes `[ i m`;
  the Swift app emits NOTHING for the chord) — whether iced should
  special-case bare ESC instead is an OPEN PRODUCT QUESTION for
  Charlie, filed as [#343] rather than decided here.
  Deferred, tracked: right-click context menus ([#338]); the tab-drag
  press-jump flicker — a timeboxed investigation (D13) found the root
  cause (an iced pill's width depends on `active`, so its reserved
  ×-slot shifts every following pill 24px the instant selection
  changes, before any pointer motion — the drag gesture just makes it
  obvious because it lands mid-gesture) but the fix is a visible
  pill-metrics change that wants Charlie's eyeball rather than a blind
  landing, so the diagnosis plus fix directions are filed as [#339]
  instead of landed; the DnD ghost-image + insertion-line redesign
  stays deferred by decision (move-the-pill ships for v0.0.18);
  copy-on-select plus a primary-selection-style paste buffer ([#340])
  and option-as-meta for ⌥-letter readline chords ([#341]) are both
  confirmed parity (neither app has either today) and are product
  questions, not gaps; a mac-only gap — selection drag doesn't
  auto-scroll at the window edge — is filed as [#342] (iced is
  explicitly not required to copy it).
  The frosted/translucent window-vibrancy spike is MOVED OUT of this
  slice to **M6 §6f** (below) — Charlie's call: it is not needed for
  the Linux release, and 6f already owns the AppKit-side groundwork
  (no `NSVisualEffectView` in the Swift source to port from) it would
  build on.
  Left genuinely open, low priority: D7's confirm-overlay wheel/hover
  pass-through was explicitly left untightened by decision (recorded,
  matching the palette's existing pass-through — W7 landed everything
  else); no keyboard focus ring or sidebar-row-hover-distinct-from-
  active state landed (neither was among the sixteen walkthrough items
  in the first place). All minor — pick up in a future pass if they
  surface again.

[#321]: https://github.com/charliek/roost/issues/321
[#338]: https://github.com/charliek/roost/issues/338
[#339]: https://github.com/charliek/roost/issues/339
[#340]: https://github.com/charliek/roost/issues/340
[#341]: https://github.com/charliek/roost/issues/341
[#342]: https://github.com/charliek/roost/issues/342
[#343]: https://github.com/charliek/roost/issues/343

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
constraint, and wgpu cache sensitivity. Pull from there rather than
re-deriving. (Iced release-profile CI, formerly listed here as
deferred, shipped as the `iced-release` job in `ci.yml` — plan 022 C2.)

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

**Packaging shipped against both decisions (2026-08-07, plan 022
commits 91b3c49/3b021fe/5733930).** `roost-iced` gained a
`linux-package` Cargo feature that is off by default and only flips
the *compiled-in default* profile — `ROOST_BUNDLE_PROFILE` still wins,
so every dev harness stays correct regardless of how the binary was
built. With it on, a Linux build resolves the `Gtk` profile kind
(same `roost` socket/`state.json`/log dir the GTK package owned), and
the window's `application_id` (WM_CLASS/app_id) and the desktop
notification's `desktop-entry` hint are now derived from that resolved
profile rather than hardcoded to `.iced`, so the packaged binary
matches the installed `ai.stridelabs.Roost.gtk.desktop` entry.
`linux/scripts/build-deb.sh` builds `roost-iced` with the feature and
stages it as `/usr/bin/roost`; the `Depends`/`Recommends` list in
`packaging/nfpm.yaml` was derived from an `strace -f -e trace=openat`
of a real launch (not `ldd`, which undercounts because winit/wgpu/ash
dlopen their stack) — Vulkan is `Recommends`, not `Depends`, since a
launch with `/usr/share/vulkan/icd.d` removed still opens a window on
the software fallback. Verified end-to-end in a pristine `ubuntu:24.04`
container via a real apt upgrade transaction: `desktop-file-validate`
passes and the installed entry's `StartupWMClass` matches the WM_CLASS
the packaged binary announces. Cutting an actual release remains a
separate, manual step — not part of this work and not yet done.

**Shipped identity rename (2026-08-10, plan 025, #320).** The first
iced release renamed the shipped Linux app id from
`ai.stridelabs.Roost.gtk` to `ai.stridelabs.Roost`: packaging now
installs the canonical `ai.stridelabs.Roost.desktop` plus a
`NoDisplay=true` alias at the old `ai.stridelabs.Roost.gtk.desktop`
name (`StartupWMClass` pointed at the new class) so upgraders with a
pre-rename pin (taskbar, `.desktop` override) still resolve. macOS dev
ids are unchanged — the `Mac`/`Gtk`/`Iced` side-by-side matrix still
needs three distinct ids there. The `gtk` CLI target string and the
`Gtk` `BundleProfileKind` variant name are kept deliberately, even
though the shipped id they resolve to on Linux is no longer `.gtk`.

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
  merged with this entry's edit); (d) fixed: the one-constant fix landed —
  `chrome::badge()` now renders a dedicated `NOTIFICATION_BADGE` (`#007aff`)
  instead of `#4e9af1`, pixel-verified by
  `test_tab_strip_pixels.py::test_notification_dots_paint_the_accent` — and
  it turned out to cover two dots, not one: the tab-pill badge and the
  sidebar project-row dot both style off `badge()` and both were corrected —
  found by the audit, filed and fixed as [#311].
* The subjective P1s in (a) are Charlie-directed by design; M4's
  "no open P0/P1" criterion reduces to: E6 landed (c) and the badge
  constant is fixed (d), so what remains is only to complete or explicitly
  waive the 3h items and the #302-blocked remainder for the beta — his
  call, flagged in the plan-021 checklist. To be precise about waiver
  semantics: the criterion itself is
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

**Entry gate: met.** E5 (plan 020) and E7 (plan 019) are both complete;
release-profile CI coverage for Iced shipped as the `iced-release` job
(plan 022 C2, ci.yml), and the parity-inventory audit is clean (refresh
audit 2026-08-07, plan 021, main@166d2d6 — see the M4 section above: open
P0 none, remaining P1s are Charlie-directed 3h polish + #302-blocked file
drops, not inventory gaps). Note the reference bar here is the **Swift
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

**Shipped (plan 027, 2026-08-16).** `mac/scripts/bundle-lib.sh` (new)
carries the toolkit-agnostic stages — version derivation, libghostty-vt
precondition, icon pipeline, roostctl build+embed, signing —
out of `bundle.sh`, verified behavior-preserving (byte-identical file
set/plist/entitlements before/after). `mac/scripts/bundle-iced.sh` (new)
sources the same lib and assembles `mac/build/Roost-Iced.app` from
`cargo build -p roost-iced` (`make bundle-iced`): ad-hoc/dev-id signing
(same `ROOST_DEVELOPER_ID_IDENTITY`/`ROOST_ALLOW_UNSIGNED` defaults as
the Swift bundle), no Sparkle keys and no `Contents/Frameworks/`, all
three TCC purpose strings kept (Roost-Iced is the TCC-responsible app
for its own tab children), `cs.disable-library-validation` omitted
(no embedded frameworks to need it), roostctl embedded, and the
existing AppIcon art reused as-is — a **distinct icon is recorded
future work**, not shipped; display name disambiguates the Dock for
now. Live parallel-install was verified on-machine: the bundle
launched via `open`, answered `identify`/`screenshot` on its own
`Roost-iced` socket while the production `Roost.app` kept answering on
its own, and quit was confirmed by pid.

The bundle-id-aware default profile (W3) lands as a macOS-only
`CFBundleGetMainBundle` probe via `objc2-core-foundation` (already
resolved in the lock through winit — no new toolchain), feeding a
pure, table-tested mapping: `ai.stridelabs.Roost.iced` → `Iced`, and
everything else — including the production id — also resolves `Iced`
today (the production-id cutover mapping is deliberately **not**
taken; that's 6c). `ROOST_BUNDLE_PROFILE` still wins over the probe,
and the detected identity is logged at startup regardless of which
arm fires, so the probe is behaviorally a no-op now but proven wired
for 6c.

Window title: **the earlier "window title should match" decision is
narrowed** to the *fallback* title only — the composed title stays
`project – cwd`; app identity comes from `CFBundleName`/
`CFBundleDisplayName`, not the titlebar. The fallback itself is keyed
off resolved profile kind, not OS: `Iced` → `"Roost-Iced"`, else
`"Roost"` — which means the Linux **dev** iced profile now also
titles `"Roost-Iced"` (a consistent dev identity; no harness asserts
on it). Packaged Linux is unaffected: it resolves the `Gtk` kind →
`"Roost"`, unchanged.

The harness gained a bundle-launch path: `ROOST_ICED_APP` in
`tools/roosttest/ui.py` drives the iced target through
LaunchServices (`open --env`) with an enumerated env allowlist
(deliberately **not** forwarding `ROOST_BUNDLE_PROFILE` — the
bundle-id path above is the thing under test), pid-based
teardown-with-proof-of-death, and a test-mode canary so a dropped
`ROOST_TEST_MODE` fails loudly. `make e2e-iced-bundle` assembles the
bundle and runs the curated smoke + walking-skeleton modules against
it. CI: a narrow `macbundle` path filter (bundle scripts, the iced
plist/entitlements, the shared icon + roostctl-entitlements inputs)
OR'd into `iced-build-e2e`'s condition, so Swift-only PRs never pay
the 2×2 iced matrix; the macOS cells gained assemble + mechanical
bundle-identity assertion (bundle id, executable name, stamped
version, absence of all `SU*` keys and `Contents/Frameworks/`, deep
codesign verify, entitlements present-and-true minus
disable-library-validation, hardened runtime, system-only `otool -L`
closure) + bundle-smoke steps, `ICED_BACKEND` forwarded from the
renderer matrix.

**Caveat carried forward, not fixed here**: ad-hoc signatures change
CDHash every rebuild, so any TCC grants made to `ai.stridelabs.Roost.iced`
reset on the next `make bundle-iced`. Not a 6a blocker (nothing here
exercises mic/camera), but it will bite dev-bundle TCC testing at 6e/6g.

* **6b. Native shim seam.** One decision with three consumers (6c/6d/6e):
  call AppKit via `objc2` (already in the lockfile through winit) or
  build a small Swift static library behind a C ABI. Decide with a
  spike, not in the abstract. (The earlier "leaning Swift lib" framing
  below was the pre-spike prior — the spike below reversed it.)

**6b decision — native macOS seam: objc2** (decided 2026-08-16)

The seam is `crates/roost-iced/src/macos/`, `cfg(target_os = "macos")`,
built on the `objc2` 0.6 generation (`objc2 0.6.4`, `objc2-app-kit 0.3.2`,
`objc2-foundation 0.3.2`) with **minimal feature sets** rather than the
crates' broad defaults. No Swift shim, no `build.rs`, no second toolchain.

Both routes were **built and run live** on macOS 26 / Xcode 26 (Apple
Swift 6.3.2), not one built and one argued. Full evidence — commands,
outputs, diffs — in the plan artifact folder (`c6-spike-evidence.md`,
`~/.claude/plans/roost/027-mac-iced-bundle/`).

| Criterion | objc2 | Swift static lib |
|---|---|---|
| Build/CI complexity | +3 `Cargo.lock` lines, **0 new packages**; no build.rs; no new toolchain | new `build.rs` invoking `swiftc`; Swift becomes a hard `cargo check` requirement on `rust-build` (macOS cell), `iced-build-e2e` ×2 and every dev Mac; new build-order dependency inside the cargo graph |
| Consumer coverage (6c–6g) | 6d ✔, 6e ✔ (`objc2-user-notifications 0.3.2`, same generation), 6f ✔, 6g ✔ proven; 6c Sparkle hand-written `msg_send!` | all ✔, but every callback needs a hand-matched `@_cdecl` + `@convention(c)` pair — cost scales with menu items / notification actions; 6c needs a vendored `Sparkle.framework` (SwiftPM cannot be consumed by a bare `swiftc` from build.rs) |
| Maintenance surface | 5 `unsafe` in a 163-line probe; **0 `unsafe` in the dock-badge consumer**; clippy clean under workspace lints | no Rust `unsafe`, but an unchecked C ABI wall maintained by hand in two languages, plus a toolchain to pin |
| Dependency-bar fit | pure Rust; the 0.6 generation is **already** in the graph via `arboard` + `softbuffer` | a second language toolchain in the crate whose premise is replacing Swift |
| Compile friction to first clean build | 2 iterations (1 error; the compiler named the fix) | 2 iterations (1 `rustc-check-cfg` placement warning) |

**objc2 probe results (8/8 pass, screen locked throughout):**
`iced::window::run` → `RawWindowHandle::AppKit` (non-null `ns_view`) →
`MainThreadMarker::new()` returns `Some` **inside** the callback →
retained `NSView` (class `WinitView`) → `view.window()` non-nil, title
read back. `NSApp.dockTile.badgeLabel` round-trips (`"3"` → `"3"`;
`None` → nil). A Rust class declared with `define_class!`, installed as
an `NSMenuItem` target/action and set as `NSApp.mainMenu`, had its
**Rust method body execute** under both `performActionForItemAtIndex:`
and `NSApp.sendAction:to:from:` — the retained-delegate-calls-back-into-
Rust mechanism 6d and 6e both depend on. No disqualifier was hit
(marker obtainable, handle lifetime scoped to the callback with
nothing retained escaping, no version conflict, callback proof passed
twice).

**Swift probe results:** `swiftc -emit-library -static` produced a
3.7 KB archive in 0.5 s (a realistic 57-line AppKit + UserNotifications
shim: 35.9 KB, 3.3 s); linked positionally via `cargo:rustc-link-arg`
following `roost-vt/build.rs`; `roost_shim_probe()` returned **42** at
runtime and a Swift `NSMenuItem` action called back into a Rust
`extern "C"` fn. No dyld issues — Swift's autolink records name
`/usr/lib/swift/*.dylib` absolutely, so **neither `-L /usr/lib/swift`
nor an rpath is needed**; `-static-stdlib` is a hard error on Apple
platforms now. Linux inertness was proven by running the build script
with `CARGO_CFG_TARGET_OS=linux` and `PATH=/nonexistent`: it exits 0
without spawning `swiftc` and without emitting `rustc-cfg`, so the
`extern "C"` block and its call sites vanish. The route is viable — it
just costs a toolchain to buy ergonomics that stop at the C ABI wall.

**Decision rationale.** Criterion (a) decides it: objc2 adds three
lockfile edge lines and nothing else, while the Swift route adds a
build script, a compiler, a CI requirement and a build-order dependency
— to the crate whose purpose in M6 is to stop depending on Swift.
Criterion (b) reinforces it: 6d and 6e are fine-grained callback APIs,
exactly the shape where objc2's one `define_class!` beats N hand-
matched C-ABI trampolines. The (c) worry that motivated the spike —
`msg_send!` unsafety — did not materialize: the first shipped consumer
needs **zero** `unsafe`. And (d) is settled by the graph itself:
`arboard` and `softbuffer` already compile this exact objc2 generation
into every macOS build.

**What 6c/6d/6e inherit.** The seam is a flat `macos` module with
`MainThreadMarker` acquired on the iced update loop (no background-
thread AppKit anywhere) — `window::run` is only needed where a real
`NSView`/`NSWindow` is (6f vibrancy), not for the badge or the menu
bar. 6d and 6e get a proven `define_class!` delegate pattern with typed
`Retained<_>` payloads. 6e adds `objc2-user-notifications 0.3.2` and
must stay on the 0.6 generation — the coupling policy is pinned in a
`Cargo.toml` comment: these versions move with `arboard`/`softbuffer`,
never independently, or the lock resolves a second copy. 6c (Sparkle)
remains an open question on either route: no generated bindings exist,
so it will be a small hand-written `extern_class!` + `msg_send!`
wrapper over `SPUStandardUpdaterController`.

**Shipped as the seam's first consumer: the dock badge**, pulled
forward from 6g (see 6g below — this piece of it is now done, the
rest is still open). `crates/roost-iced/src/macos/dock_badge.rs`
mirrors the notification-inbox count onto `NSApp.dockTile.badgeLabel`
exactly as `App.swift:1961-1968`'s `refreshDockBadge()` does (`nil` at
zero), synced after `WindowOpened` and after every
`reconcile_notification_inbox()` — all on the iced update loop via
`MainThreadMarker`, zero `unsafe` in the consumer itself. A test-mode
iced-only IPC op, `app.dock_badge` (`{"label": string|null}`, pinned
wire schema, documented in `docs/reference/ipc.md`), reads the live
AppKit badge without re-deriving it from the inbox; GTK rejects in its
exhaustive match, non-macOS iced rejects not-implemented. A new
`tools/roosttest/test_dock_badge.py` (darwin+iced only) drives
notification → badge count → clear → nil against a bundle launch and
is wired into `ICED_E2E_TESTS` and all three `ci.yml` iced lane lists.

* **6c. Sparkle auto-update.** `mac/Package.swift` + `App.swift` use
  ~two API calls (`SPUStandardUpdaterController`, `checkForUpdates(_:)`);
  `bundle.sh` already embeds and inside-out-signs the framework, and
  `release.yml` already EdDSA-signs and publishes the appcast. The
  evaluation build needs a **separate feed or none** so the two apps do
  not offer each other's updates. Design note for the eventual cutover:
  Sparkle does not care what language wrote the app, so an Iced build
  shipping under the *same* bundle id with the same `SUPublicEDKey` and a
  higher `CFBundleVersion` upgrades existing Swift installs in place.

**Mechanics shipped, feed deliberately absent (plan 028, 2026-08-16).**
`third_party/sparkle/fetch.sh` pins Sparkle 2.9.5 by SHA256 from the
official release artifact (gitignored `out/`, stamped, cached — and
`actions/cache`d in CI so a release-asset outage can't flake the lanes);
`bundle-iced.sh` embeds it and signs via `codesign_sparkle_or_die`'s
strict inner→outer per-component chain (Installer.xpc → Downloader.xpc
with `--preserve-metadata=entitlements`, Sparkle#2511 → Autoupdate →
Updater.app → framework; **no `--deep`** — shed's production evidence:
a `--deep`/wrong-order chain signs and notarizes clean but breaks at
update-apply; the Swift bundle's looser `--deep` function is left
untouched and noted as a future hygiene pass).
`Roost-Iced.entitlements` restored `cs.disable-library-validation` for
the ad-hoc framework (REMOVE-once-team-signed note mirrored from the
Swift app). The runtime side is `crates/roost-iced/src/macos/sparkle.rs`:
**dlopen** of the framework's top-level symlink at `window_opened` + a
hand-written `msg_send!` surface — deliberately NOT a link-time
`-framework` dependency, so cargo builds/CI matrices/`make run-iced`
never need the framework staged (bare binaries report
`unavailable`-with-reason). One deviation from the obvious API:
`initWithStartingUpdater:NO` + explicit `startUpdater:`, because the
controller's auto-start throws an unprompted modal alert on a feedless
app; direct start surfaces the same condition as an `NSError`. Feedless
`startUpdater:` succeeds, so the shipped keyless bundle runs with the
updater started and errors gracefully on a manual check. The shipped
plist carries `SUEnableAutomaticChecks=false` and **no
`SUFeedURL`/`SUPublicEDKey`**; **feed enablement is two env vars** at
bundle time (`ROOST_ICED_SPARKLE_FEED_URL` +
`ROOST_ICED_SPARKLE_ED_PUBLIC_KEY`, both-or-error →
PlistBuddy-inserted), proven live by the `e2e-iced-sparkle` lane, which
builds a TEST-ONLY-keyed bundle and drives
`checkForUpdateInformation` against a loopback http appcast to a real
`found` via the test-mode `app.update_check`/`app.update_status` ops
(delegate feed override gated on `ROOST_TEST_MODE`; only the TEST-ONLY
*public* key is committed — Sparkle does not filter unsigned appcast
items at parse time, verified). When a real iced feed happens: generate
a fresh keypair (never the Swift app's), host an iced-specific appcast,
set the two env vars in the bundle job — no rework. The first
feed-carrying build necessarily reaches users out-of-band; every build
after it updates in place. Interactive panel flow + Gatekeeper behavior
remain morning-checklist/manual (locked-Mac constraint); DMG/release.yml
wiring deliberately untouched.

**Feed wired (plan 030, 2026-08-17).** The "when a real iced feed
happens" step above is now done: a separate iced keypair and
`docs/appcast-iced.xml`, plus `release.yml` `mac-iced` and
`appcast-iced` jobs, feed URL
`https://charliek.github.io/roost/appcast-iced.xml`. Production key
generation and installation stays
Charlie-gated — `mac/keys/README.md` documents the procedure — no key
was generated or installed in this session.

* **6d. Menu bar.** ~~winit installs none~~ (correction, verified
  against winit 0.30.13 source: winit installs a minimal default menu —
  About/Hide/Quit via `terminate:` — so the real gap was app commands
  and a Quit that provably runs the clean-exit flush). `App.swift`
  builds ~35 items across App/File/View/Edit/Window plus a dynamic
  Window menu of tabs and projects.

**Shipped (plan 028, 2026-08-16), hand-rolled NSMenu via
`objc2-app-kit`** (`muda` rejected: new dependency, duplicate accel
model, and the 6b spike had already proven the `define_class!`
target/action mechanism). `crates/roost-iced/src/macos/menu.rs` builds
the full parity menu set (30 static actionable items; Cut/Select All
present-but-disabled with **no key equivalents**; a dynamic Window menu
of project rows ⌘1-9 and active-project tab rows ⌃1-9 with stable-id
dispatch, rebuilt from `reconcile()` behind a plain-data model diff).
The keybind promise landed: menu key equivalents derive from the
canonicalized table via `menu_accel_for_action` (deterministic
inversion, in `roost-ui-model` beside the table — user rebinds show up
in the menu), and activation rides the engine feed into the SAME
`dispatch_keybind_action` path a keystroke takes. Quit is a custom item
through the graceful exit path (never `terminate:`, which skips
`Workspace::flush`); ⌘Q now actually quits cleanly, asserted by an
exit-lane e2e. Route gating without `validateMenuItem`
(`autoenablesItems=false`, direct mutation): palette-open disables all
but the four palette toggles (Swift parity), editor/confirm/IME-compose
disables everything, and Copy/Paste get their key equivalents
**blanked** whenever any text surface owns the keyboard — a blanked
equivalent provably falls through to iced's `text_input` regardless of
AppKit's (disputed) disabled-item chord behavior. Introspection for the
locked-Mac test posture: test-mode ops `app.menu_dump` (walks the LIVE
`NSApp.mainMenu`) and `app.menu_activate` (title-path resolution, its
own `isEnabled` check — `performActionForItemAtIndex:` runs no
validation), covered by `test_menu_bar.py` (20 e2e) + the
`e2e-iced-menu-quit` destructive lane. Accepted behavior change:
held-accel repeat is now AppKit's menu repeat, not
`dispatch_keybind_action`'s per-action suppression. Real-keypress
interception and OS menu rendering are morning-checklist items.
* **6e. Desktop notifications, macOS backend.** Closes [#303]. The seam
  from slice 3f is backend-agnostic already (`notifications.rs`: worker,
  per-tab replace semantics, click routing); only `mod backend` is
  missing. `UNUserNotificationCenter` requires a bundled, signed app —
  hence the 6a dependency, and it cannot be validated from `make
  run-iced`. Match `mac/Sources/Roost/DesktopNotifications.swift`
  semantics. Replace-by-server-id becomes UN's stable per-tab identifier,
  which is simpler than the D-Bus version.

**Shipped (plan 030, 2026-08-17).**
`crates/roost-iced/src/macos/notifications.rs` adds the
`UNUserNotificationCenter` backend behind the existing seam. Two
conscious divergences from the Swift app, recorded in plan 030 and its
PR: (a) **replace, not stack** — the seam's replace contract is
honored via a stable per-tab identifier `roost-tab-{tab_id}`, so a new
event for a tab replaces its own live banner, where Swift stacks under a
unique id per event; (b) **no cold-launch banner-click routing** — the
delegate installs at `window_opened` and its activation futures are
process-local oneshots keyed by identifier, so a banner clicked after
quit/relaunch is not routed back to a tab, unlike Swift's userInfo-based
routing.

**Testing 6e needs a Developer-ID bundle — an ad-hoc one cannot work**
(learned the hard way, 2026-08-24). macOS refuses notification
authorization outright to an ad-hoc-signed app: `requestAuthorization`
answers `granted=false` with "Notifications are not allowed for this
application", and no prompt is ever shown. That is the CDHash caveat
above in its sharpest form. Two traps follow:

* `make e2e-iced-sparkle` re-bundles `mac/build/Roost-Iced.app`
  ad-hoc-signed with the TEST-ONLY key, **clobbering** any Developer-ID
  build sitting there. After running it, `mac/build/Roost-Iced.app` can
  never show a banner. Test from a Developer-ID build — in practice,
  install the notarized DMG's app to `/Applications` and drive that.
* Notification permission is **not** TCC data, so `tccutil reset
  Notifications <bundle-id>` does not exist and always fails; the
  record lives in `usernoted`'s SIP-protected store. A denial recorded
  against `ai.stridelabs.Roost.iced` by earlier ad-hoc builds survives
  the upgrade to a signed build, and the only practical reset is
  **System Settings → Notifications → Roost-Iced → Allow
  notifications**. The grant is read once at startup, so the app must
  be relaunched afterward before banners appear.
* **6f. Window vibrancy.** Moved here from 3h by plan 026 (2026-08-15,
  Charlie's call, Q9): the frosted/translucent spike is an M6 mac-parity
  exploration item, not something the Linux release needs, so it no
  longer lives in the M3 3h polish-parity slice. Worth knowing before
  starting: there is **no `NSVisualEffectView` anywhere in the Swift
  source** — the sidebar's translucency is AppKit's
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
  including which entitlements we deliberately omit). **Dock badge:
  done** — pulled forward as 6b's seam proof-consumer (2026-08-16, see
  6b above for the implementation + test coverage). Dispositions for the
  rest (plan 030, 2026-08-17):
    * **Dock menu, reopen-from-Dock, open-file/URL: winit-blocked.**
      winit 0.30.13's private app delegate implements only
      `applicationDidFinishLaunching` and `applicationWillTerminate`
      (`app_state.rs:47-70`ish) — no extension point for the rest — and
      there is no Swift parity bar to chase either: `App.swift`
      implements none of these. Not planned unless winit grows the API
      (or a deliberate future vendored-winit decision — Charlie's).
    * **Graceful terminate: covered.** `applicationWillTerminate` →
      `LoopExiting` → `Drop for App` → `workspace.flush()`
      (`app.rs:3200-3210`). An OS-initiated terminate blocks on the
      delegate, so `Drop` still runs; only `kill -9` bypasses it. A
      cancelable `applicationShouldTerminate:` is winit-blocked the same
      way as the dock/reopen/open-file gap above.
    * **Activation policy: fine by manifest.** The bundled app is
      `Regular` (`LSUIElement` false + a real icon); the bare-binary
      generic Dock icon is the known non-bug. `iced_winit` 0.14 exposes
      no policy hook, so there is nothing to do.
    * **Secure Keyboard Entry: feasible, DEFERRED.** Carbon
      `EnableSecureEventInput` FFI at the window-focus edges would work
      (the Ghostty reference lands with the pinned checkout at
      `third_party/ghostty/.../SecureInput.swift` once `build.sh`
      runs), but it's deferred
      pending a product decision on the toggle surface — always-on
      breaks text expanders — Charlie's call.
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
