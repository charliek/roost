# Iced visual and interaction parity inventory

Status: active gap register, not a parity claim

Baseline: `poc/iced` at `dea73a5` (2026-08-01)
Refresh audit: `main` at `166d2d6` (2026-08-07) — every row re-verified;
see "Refresh audit method and evidence" below. Verdicts in the registers
cite that audit unless they name an earlier plan.

This inventory turns the Iced POC's remaining product work into named,
testable slices. GTK is the primary Linux visual reference. Swift/AppKit is a
second product reference, especially where GTK's client-side decorations or
desktop integration are toolkit-specific. A difference is acceptable only
when this document names the reference, reason, affected platforms, and
user-visible impact.

**GTK-divergence note (per Charlie, 2026-08-05):** GTK is expected to be
retired rather than restyled to match the Mac-parity chrome plan 016
introduced on iced — iced's chrome now leads, GTK does not chase it.
Rows below that name a GTK/iced difference record it as documented
divergence, not as GTK-restyle work to schedule.

## Baseline method and evidence

The first audit used the existing lifecycle sidebar fixture at a requested
1100×700 logical window size. Each target ran under the functional harness with
`ROOST_TEST_MODE=1`, `--roost-fresh`, the seeded test config, and a disposable
`ROOST_STATE_DIR`. No developer state was read or written.

```sh
for target in mac gtk iced; do
  ROOST_TEST_MODE=1 \
  ROOST_E2E_ARTIFACT_DIR="$PWD/target/visual-parity-baseline-dea73a5/$target" \
    uv run --group test pytest tools/roosttest/test_sidebar_pixels.py \
      --roost-target "$target" --roost-fresh -q
done
```

All three captures were 1100×700 and the focused lifecycle assertions passed.
The local comparison artifacts are under
`target/visual-parity-baseline-dea73a5/{mac,gtk,iced}/sidebar.png`.

The ignored PNGs are reproducible evidence rather than repository assets. Their
SHA-256 digests pin the audited baseline:

| Target | SHA-256 |
|---|---|
| Swift | `9cbbd193c3326927110e922a8c60818dac75a192699abc62f9b865bb28506d56` |
| GTK | `360cc2f3486e37d89231d810aeace7f7a08eadbfd28a5c78e25223ec4aeefe28` |
| Iced | `7c59ae0d2960cb105b1cc12abf32a18850c7e1e837faba0dd83600002619e598` |

The background samples below use `(5,350)` for the sidebar and `(500,350)`
for the terminal in 1× screenshot pixel coordinates. Band heights were read at
the sidebar/terminal transition and are deliberately approximate until the
named geometry fixture lands.

This is valid evidence for broad shell differences, but it is not yet the final
comparison fixture:

- the generated project names differ by target, even though the structure and
  four lifecycle states are equivalent;
- GTK's in-process capture includes its client-side `AdwHeaderBar`, while the
  native AppKit and Iced/winit title bars are outside their renderer captures;
- text rasterization and native control metrics differ across platforms.

The first tooling slice must add one deterministic, explicitly named workspace
scenario and a manifest that distinguishes application-owned content from
native window chrome. Full-window pixel equality remains inappropriate.

The resulting `make visual-parity` fixture was then captured locally for all
three macOS targets and in the Linux shed for GTK plus Iced under X11/Wayland
with both wgpu and tiny-skia. Visual review found a renderer correctness defect
in the original Canvas implementation: tiny-skia started its terminal body at
x=440/y=88 while wgpu started at the expected sidebar edge x=220/y=44. The
defect reproduced in both product and external-compositor captures on both
Linux display backends. Iced 0.14.0 is the latest release, while its official
fix landed after that release; the terminal therefore moved to a
renderer-neutral custom widget using core quads and text. Focused product
captures now guard the non-zero origin, full extent, collapsed-sidebar origin,
and glyph signal independently of visual parity. Captures are under the ignored
`target/visual-parity-shed/` tree; their manifests identify display backend and
renderer so results cannot be confused or overwritten.

Measured baseline facts:

| Surface | Swift | GTK | Iced at `dea73a5` |
|---|---:|---:|---:|
| Requested content capture | 1100×700 | 1100×700 | 1100×700 |
| Sidebar width | 220 pt (resizable 160–400, persisted in `RoostSidebarWidth` UserDefaults) | 220 pt (resizable 160–400, persisted in `state.json`) | 220 pt (resizable 160–400, persisted in `state.json`) |
| Sidebar body sample | `#3a3a3a` | `#282828` | `#111111` |
| Terminal body sample | `#1e1e1e` | `#1e1e1e` | `#1e1e1e` |
| Application-owned tab band | about 32 pt | about 34 pt below header | 44 pt |
| Agent lifecycle colors | exact shared palette | exact shared palette | exact shared palette |
| Agent dot left-edge guard | passed | passed | passed |

The most material baseline observation is qualitative but unambiguous: Iced
uses the stock indigo primary-button treatment for projects, agent rows, tabs,
the add-tab control, and notifications. GTK and Swift instead use compact,
mostly transparent dark chrome with selection applied only to the active row or
pill. Iced therefore reads as a generic widget demonstration despite having a
real terminal and shared state behind it.

## Refresh audit method and evidence (2026-08-07, main@166d2d6)

The plan-021 audit re-verified every row against current behavior. Evidence
classes, all reproducible from this commit:

- **Suite runs (primary evidence for functional rows).** Shed (real Linux,
  Xvfb): full GTK e2e `pytest tools/roosttest --roost-target gtk
  --roost-fresh` → **171 passed, 11 skipped**; the full 13-file iced list +
  clipboard pair under X11 → **80 passed, 2 benign skips** on *both*
  renderers (wgpu, tiny-skia). macOS: iced list → 54 passed, 27 platform
  skips (the one non-CI-parity failure is a dev-loop invocation artifact:
  `make e2e-iced` launches without `ROOST_TEST_MODE=1`, so the test-mode
  `window.resize` op is refused; the same test passes fresh + test-mode).
- **Hermetic parity captures** (`tools/screenshot/parity.py`): macOS iced
  (run `166d2d6-944d5d6354`), shed GTK + iced X11 (runs
  `166d2d6-4832defb2d`, `166d2d6-96dd344b53`). Load-bearing digests
  (SHA-256, shell captures):
  - iced-macOS (wgpu, native): `3b171ec502c0cbce908deda7339e70aa996e74a9b46f2891f33ba29d597a2afc`
  - iced-linux-x11 (default renderer): `9f26264e780ace35129e7e0732589929d05399c8dbc9061156953f2656d0c8ab`
  - gtk-linux-x11: `7d4b922ed97a54281b2a43a791cee56de48e1f950f442af52714d2478c617f6c`
  Each run directory carries its own `measurements.json` +
  `manifest.md` naming target, OS, display backend, renderer, and scale. The GTK run's agent-palette *measurement* failed on
  a one-bit rounding drift — see the harness-fragility note below; the
  captures themselves are visually correct.
- **Code refs** for rows whose truth is structural (file:line cited in the
  row), and the real-input guard `tools/input/linux/iced_clipboard_check.py`
  (CI-wired) for pointer-path rows.

Harness-fragility note: GTK's palette selection is `alpha(#ffffff, 0.13)`
over `#2d2d33` (`crates/roost-linux/src/resources/style.css`), whose blue
channel composites to 77.52; cairo's rounding of that value is
library-version-dependent, so `parity.py`'s previously exact `#48484E`
match now fails against `#48484D` on current shed cairo. Current pango
likewise no longer produces the exact text-pixel constants inside the
selected row (white name peaks at `#FBFBFB`, red status at `(221,82,82)`).
The `measure_agent_palette` measurement gained a scoped ±2 tolerance on
the alpha-composited selection (both its positive match and its
ink-exclusion use), and its two text *presence* scans now classify ink
semantically (bright-neutral / red-dominant predicates) instead of
matching fixed constants; geometry assertions and every solid-fill color
match elsewhere stay exact. This
fragility class — alpha compositing and text rasterization shifting under
library updates — is part of the [#284] evidence.

## Chrome foundation result

The first production-facing styling slice removes the stock-primary-button
look. Iced now uses an explicit Roost palette and density model rather than its
theme defaults: `#282828` sidebar chrome, a 34 pt common header/tab seam,
transparent inactive rows, `#13509d` only for the active project, `#3a3a3a`
for the active agent row, `#243751` for the active tab pill, and the existing
shared lifecycle colors for project stripes, agent dots, and tab dots. The
sidebar header/body/footer and tab overflow/control regions have the same
ownership structure as the references. Notification state is a compact badge,
and the active tab has a separate exact-ID close target.

Side-by-side product captures on macOS and real Linux X11 show that the former
purple/full-width-control concern is closed: the shell now reads as the same
Roost product rather than a generic Iced demonstration. Both wgpu and tiny-skia
render the structure consistently. This is a positive styling-feasibility
decision, not a final parity claim; the remaining open work below is mostly
missing interaction/polish rather than inability to express the look (the
last P0 closed with plan 010; re-verified by the 2026-08-07 audit).

Correctness stays in focused reusable gates. `app.window_metrics` now reports
Iced's optional `terminal_top`, constrained X11 pointer coverage reaches the
last vertical row and a horizontally scrolled active close button while fixed
controls remain available, and stale close IDs/fallback/cascade are unit-tested.
The repeatable comparison fixture remains the human visual gate and is not a
new permanent parity CI suite.

## Visual gap register

Priority meanings: P0 blocks a usable parity claim, P1 is required common
product polish, and P2 is an optional native/toolkit refinement.

| Area | Reference behavior | Current Iced behavior | Priority | Acceptance evidence |
|---|---|---|---:|---|
| Shell hierarchy | Sidebar and tab strip are compact chrome bands around a darker terminal | Closed in chrome slice, band height trued up to 32 pt in plan 016: compact seam and explicit dark surfaces | closed | Same named fixture; focused band-height/background assertions; side-by-side capture |
| Sidebar surface | 220 pt default, `#282828` GTK chrome; Swift material resolves near `#3a3a3a`; header reads `PROJECTS` | Closed: 220 pt `#282828` surface and `PROJECTS` header | closed | Metrics remain 220 pt; background sample and header-content assertion |
| Project rows | 28 pt compact rows; active project is an inset deep-blue rounded pill; lifecycle rollup is a narrow leading stripe | Closed: compact transparent rows, active `#13509d` pill, shared rollup stripe | closed | Selected/unselected geometry and color assertions; lifecycle stripe fixture; click E2E |
| Agent rows | Transparent compact nested rows; only active agent has a faint wash; lifecycle dot, name, status, and time have distinct roles | Audited 2026-08-07: shared lifecycle colors/alignment and the gray active wash hold; agent rows share the Mac-derived `ROW_HEIGHT` 32 (`chrome.rs:7`, plan 016) vs GTK's 20 px min-height rows (`style.css:148-152`) — but GTK no longer leads (see divergence note). The typographic gaps are concrete: agent name/detail render in the default font, not `chrome_font` (`app.rs:1649-1667`), and there is no active-name weight change where GTK bolds (`style.css:157-158`). **Charlie visually PASSED agent-row presentation side-by-side in the plan 026 live walkthrough (2026-08-15)** — no code change requested; the technical font-family/weight divergence above is still present in the code but is no longer treated as a blocking gap | closed (Charlie's call) | Typographic distinctness was his call and he passed it as-is (walkthrough record); the measurable half (row-height bound, active-wash color) is covered by audit-run tests. Click E2E retained |
| Sidebar footer | Centered compact `+ New Project` action in a separated footer | closed (plans 010 + 016): the footer is a fixed `+ New Project` chip; create ships **without a directory picker** (name `""` + cwd `$HOME` — matching both shipped UIs, roadmap 3b), reachable via footer, keybind, and palette | closed | Audit captures show the footer on macOS + Linux X11 (parity runs 2026-08-07); real-input fixed-footer-after-scroll click (`iced_clipboard_check.py::_chrome_overflow_navigation`); functional `test_project_lifecycle.py` in the iced CI lanes |
| Sidebar overflow | Project and agent lists scroll vertically without moving the header/footer | Closed: body scrolls independently and final row activates in a constrained real-pointer fixture | closed | Small-window many-row fixture, wheel/drag navigation, final-row activation |
| Sidebar collapse/resize | Both references expose a toolbar toggle. Swift persists a 160–400 pt user width; GTK uses a 160 pt minimum/default 220 pt `GtkPaned` without persisting a 400 pt cap | Resolved (plan 016): no in-window collapse affordance, matching the Mac — reopen via keybind/palette only, the ☰ button is removed. Dragging the grip past half the 160 pt floor (below 80 px, unclamped) collapses the sidebar and drops the live drag width without committing, so reopening restores the pre-drag committed width rather than the 160 pt floor. Recorded parity divergence: NSSplitView lets a drag continue past the floor and re-expand within the same gesture; here the grip leaves the tree at collapse, so drag-back-to-reopen within one gesture is impossible | closed | Resize: functional e2e + real-input grip segment (shipped, plan 011). Collapse: unit matrix (threshold crossing, no-Dragged-after-Collapse, released-event no-op, reopen-width invariant) plus capture (shipped, plan 016) |
| Tab strip | About 24 pt pills in a compact band with 6 pt gaps and horizontal overflow | Closed for active/manual reachability: 24 pt dark pills in a 32 pt band (trued up from 34 pt in plan 016) with independent horizontal overflow | closed | Band/pill geometry assertions under both renderers; overflow test |
| Tab status | Shared lifecycle dot at leading edge, white active label, muted inactive label | Narrowed by plan 026 C4 (2026-08-15): the notification dot grew 8→9px (both tab-pill and project-row sites, one shared constant), pinned by the existing `test_notification_dots_paint_the_accent` pixel test. The lifecycle-dot color logic remains unit-tested (`app.rs:3502-3529`) and active/inactive label color (`chrome::TEXT`/`MUTED_TEXT`) was already wired pre-026; what is still missing is a dedicated status-slot geometry/color capture (lifecycle dot across states, label weight) — `test_tab_strip_pixels.py` still doesn't pin that beyond the notification-dot case | P1 → 3h | Notification-dot size covered; add a focused lifecycle-dot/label-weight capture for the remainder |
| Tab close/badge | **Corrected 2026-08-07**: the shipped Mac shows × only on the *active* pill with no hover reveal (`App.swift:4747` — `closeButton.isHidden = !isActive`); the original "or hovered" framing was wrong. Inactive notification uses a blue trailing badge — GTK deliberately hardcodes Mac's `#007aff` (`style.css:277-286`) | Active exact-ID close implemented and covered (real-input `_direct_tab_close`: exact removal, survivor PTY, last-tab cascade; badge suppression + clear in `test_notifications.py`). **Hover-close DECIDED 2026-08-15 (plan 026, Q3): stays ×-active-only, no hover reveal** — that would be a *product* change, not a parity port; instead the close-button hover recolor (`#393939` background wash) is gone and the × glyph itself tints red on hover (C4). **Closed: [#311](https://github.com/charliek/roost/issues/311) is fixed** — `chrome::badge()` now renders a dedicated `NOTIFICATION_BADGE` (`#007aff`) instead of `NOTIFICATION` (`#4e9af1`); plan 026 C5 folded both into one `ACCENT` constant (closes [#321]) since the Mac uses `controlAccentColor` for both dots and drag indicators. The finding turned out to cover two surfaces, not one: `badge()` styles both the tab-pill badge and the sidebar project-row dot, and both have a Mac reference at `NSColor.controlAccentColor` (`App.swift:4772`, `:5207`), so both were corrected | closed | Badge fix covered by `test_tab_strip_pixels.py::test_notification_dots_paint_the_accent` (color, size, and position for both the tab badge and the sidebar dot, mutation-verified); close/cascade/clear coverage is done (audit runs 2026-08-07); × red-hover-tint shipped C4 |
| Tab rename | Inline rename through double-click or the configured command, with authoritative persistence | Closed: compact inline editor uses stable IDs, select-all focus, Enter commit, Escape/click-away cancel, and shared GTK/Iced trim/no-op policy | closed | X11 physical shortcut/double-click/Enter/Escape/click-away gate, zero PTY leakage, relaunch persistence, and named GTK/Iced captures |
| Tab reorder | Pointer drag reorder with visible insertion feedback | Closed: stable-ID drag preview, insertion feedback, exact authoritative commit, cancellation, overflow, and relaunch persistence work under both renderers | closed | Bidirectional physical X11/Wayland input, outside-release/palette cancellation, zero PTY leakage, and named product captures |
| New-tab affordance | Compact plus control following the pills | Closed (re-shaped by plan 016): the plus sits inside the scrolling strip 6px after the last pill and scrolls with overflow — Mac parity, where ＋ is an arranged strip subview. Under overflow it scrolls offscreen (accepted; keybind/palette still create tabs) | closed | Real-input click computed from the last pill's rendered right edge opens one PTY-backed tab (plan 016 harness rework) |
| Notification entry | Header bell with count badge opens the inbox palette (original framing) — the Mac reference actually has no bell at all | Resolved (plan 016): the bell is removed entirely — a deliberate divergence from this row's original framing, brought into line with the Mac reference, which has no bell. Shipped shape: sidebar project-row dots (any project with a notifying tab, active project included) plus the existing pill badges; the palette/keybind own the inbox entry | closed | Sidebar-dot + pill-badge capture; `roostctl notify` demo comparison against the Mac; inbox-via-palette/keybind e2e (shipped, plan 016) |
| Terminal padding | Compact consistent inset around the grid — the Mac reference is edge-pinned at zero | Resolved (plan 016): `TERMINAL_PADDING` 0 on all sides, Mac-parity — the measurement half of this row is done. Glyph-baseline comparison PASSED in the plan 026 live walkthrough (descenders/CJK/pipes side-by-side, 2026-08-15) — no code needed | closed | Cell-origin/viewport-edge assertions at padding 0 (shipped, plan 016); glyph-baseline PASS recorded in `~/.claude/plans/roost/026-mac-polish/walkthrough-verdicts.md` |
| Terminal scrollback | Wheel/page navigation scrolls retained history locally when mouse reporting is off; alternate-screen behavior follows terminal modes | Closed: wheel and bare PageUp/PageDown page navigation both route through the shared GTK/Iced `roost-vt` policy — retained history, exact bottom state, next-terminal-key snap, mouse-report precedence, alternate-screen arrows/forwarding, and a full-viewport local page move that preserves selection and bypasses snap only on the local route. Swift Mac has no PageUp/PageDown scrollback route (deliberate Rust-UI-first divergence; no prior reference behavior existed) | closed | Physical X11 wheel under both renderers; `roost-vt::route_page` unit/fixture coverage plus both-UI (GTK/Iced) adapter fixtures (selection preserved, zero local PTY bytes, byte-identical Forward path); physical PageUp/PageDown segment in `iced_clipboard_check.py` |
| Terminal typography | Configured family/size, baseline and cell metrics stable across styles and graphemes | Renderer-measured size and installed-family selection now reflow every live tab atomically, persist through the shared config policy, and reach new/restored tabs (audit-confirmed: `test_z_typography.py` pins reflow, persistence, preview/confirm across GTK/Iced). Glyph baseline/style comparison PASSED in the plan 026 live walkthrough (2026-08-15) — no code needed | closed | Latin/wide/combined/style fixture under wgpu and tiny-skia; shared GTK/Iced font selection E2E; glyph-baseline PASS recorded in the plan 026 walkthrough record |
| Terminal cursor/selection/link | Shared colors and modes with reference-like cursor, selection, and link feedback | Audit-confirmed 2026-08-07: functional coverage is real and CI-wired (multi-click word/line select, OSC 22 cursor shape, Alt-hover link pointer in `iced_clipboard_check.py`; selection round-trips in `test_selection.py`). **Correction (plan 026 C2, 2026-08-15): the cursor half of this row was not just untested, it was wrong** — `CursorVisualStyle::from_u32` in `roost-vt` mapped bar/block TRANSPOSED against the vendored header, so the shell's default block cursor rendered as a beam and DECSCUSR 1/2 vs 5/6 were swapped, in both Rust UIs; fixed at the roost-vt source (GTK inherits the correction) plus a hollow-outline cursor on window-unfocus (mac parity, color stays theme/OSC-12-driven). Pixel-verified across all six cursor states in manual captures (`~/.claude/plans/roost/026-mac-polish/`), but still no CI-wired pixel guard for cursor shape/color — and selection tint / link underline remain untested by pixels too | P1 → 3h | Cursor mapping repinned against the header in `roost-vt` unit tests (C2); focused cursor/selection/link CI-wired screenshots remain to be built |
| Terminal box-drawing/block glyphs | Both shipped UIs draw U+2500–U+257F and U+2580–U+259F geometrically rather than from the font, because font glyphs do not tile pixel-perfectly across adjacent cells — `mac/Sources/Roost/Sprite.swift` and `crates/roost-linux/src/sprite.rs`, both ports of Ghostty's `font/sprite/draw/{block,box}.zig` and kept in lockstep per the CLAUDE.md parity rule | Closed (plan 020, engine-track slice E5): the geometry moved to `roost_ui_model::sprite` (pure-data primitives; `tessellate` flattens stroked shapes into stamped rects for quad-only renderers) and Iced draws U+2500–U+259F as integer-edge-snapped `fill_quad`s inside the glyph pass — seams gone; GTK's `sprite.rs` is now a thin cairo adapter over the same shared geometry. Row added 2026-08-06; the gap existed unnoticed because this inventory had no box-drawing row. Recorded divergences, decided not discovered: wgpu's linear-space alpha blend renders the shade glyphs (░▒▓) lighter than cairo's sRGB blend, and arcs/diagonals are stamped-quad staircases rather than cairo AA strokes | closed | Seam-free capture verified by the committed `tools/roosttest/test_sprite_pixels.py` pixel guards (seam + internal-edge + counter assertions, running in all three iced CI lanes) plus plan-020 screenshot artifacts; codepoint-dispatch unit coverage in `roost_ui_model::sprite` (dispatch/rejection/full-range tests) mirroring the existing Swift/Rust sprite tests; GTK regression pinned by the golden-hash fixture (153 codepoints × 3 cell sizes) |
| Palette placement | Centered, elevated compact panel over an undimmed terminal, styled semantic rows, keyboard focus, and scrolling | Closed for the visual-feasibility slice: exact reference neutrals, border/shadow, content-sized 660 pt card capped at 500 pt, compact command/agent/notification/provider rows, shortcut hints, fuzzy-match accents, disabled state, and a narrow neutral scrollbar. Further matched to the live mac in plan 026 C9 (2026-08-15): row insets to the mac's 14px selection-highlight-from-card-edge, scrollbar switched to `Scrollbar::hidden()` (wheel scroll intact), the divider under the filter input removed, and a top-row scroll-clipping bug fixed (a vertical row-grid snap on scroll-into-view) | closed | Five named GTK/Iced captures; focus/scroll tests plus real row/card/outside pointer routing; C9 adds palette_scroll unit tests for the row-grid snap, captures in the plan 026 folder |
| Terminal IME (dead keys, CJK composition, emoji picker) | **Row added by the plan-021 audit — no row existed (the E5 omission class).** Neither shipped UI wires terminal-surface IME: GTK's terminal is a bare `EventControllerKey` with no `IMContext` (deferral documented at `crates/roost-linux/src/key_encoder.rs:21-24`); Swift's `TerminalView` never adopts `NSTextInputClient` nor calls `interpretKeyEvents` (`KeyEncoder.swift:151-153`); both hardcode `set_composing(false)`. GTK/Swift absence is recorded divergence (GTK retiring; Swift under the M6 decision) | Closed for iced (plan 021, engine slice E6 — merges with this row): iced is the first Roost UI with working terminal IME. Commit→PTY through the libghostty encoder routed to the composing tab (a one-shot discard latch keeps canceled compositions off the PTY); widget-drawn preedit at the cursor (terminal font, cell-width-aware, right-aligned overflow, hidden-cursor suppressed); IME enabled only while the terminal owns the keyboard and the window is focused. GTK/Swift remain documented-divergent (GTK retiring; Swift under M6) | closed (iced) | `tools/roosttest/test_ime.py` in all three iced CI lanes via the `tab.feed_ime` test-mode op + unit vectors on winit's real delivery sequences; real-IME human smoke (dead keys/CJK/emoji/press-and-hold) tracked on the plan-021 checklist — the honest automated/human boundary is recorded in the roadmap E6 entry |
| Window vibrancy / translucency | Mac sidebar translucency is AppKit-native (no `NSVisualEffectView` in the Swift source — roadmap § 6f, :736-737); GTK has none | Not implemented. **Moved 2026-08-15 (plan 026, Charlie's Q9 call): OUT of 3h and into M6 §6f** as a mac-parity exploration item — not needed for the Linux release | P2 → M6 6f (needs Charlie) | Platform captures once 6f is scheduled |
| Context menus (right-click) | Both references expose right-click menus: GTK project-row popover (`crates/roost-linux/src/app.rs:1393-1408`) and per-pill right-click popover (button-3 gesture, `app.rs:1824`), Mac `NSMenu` on project rows (`App.swift:712-723`) and tab pills (`TabPillView.menu(for:)`, `App.swift:5105`) | None in iced — every covered operation (rename, delete, close) is reachable via double-click, keybind, or palette, so no capability is lost, but the affordance itself is absent. Row added by the plan-021 audit; previously mentioned only as a delete-mechanism option. **DECIDED 2026-08-15 (plan 026 walkthrough, Q4): deferred, tracked in [#338](https://github.com/charliek/roost/issues/338)** — the double-click/keybind/palette routes stay the accepted shape for now | P2 | Tracked in #338; revisit if the affordance gap is raised again |
| Terminal bell (BEL) | **Absent in all three products** — parity holds in absence. The shared VT layer deliberately defers the second-callback refactor that a bell needs (`crates/roost-vt/src/terminal.rs:336`) | Same absence | — | Row added by the plan-021 audit so the absence is a recorded decision, not an unnoticed gap; revisit if any UI grows a bell |
| Empty/loading/error states | Deliberate shell placeholders without changing hierarchy | Corrected 2026-08-07: errors are **no longer sidebar-appended** — they render as a 5 s self-expiring bottom-right toast (`app.rs:1911-1922`, `chrome::status_toast`). **Resolved 2026-08-15 (plan 026 C7)**: the "Starting terminal…" text placeholder is gone, replaced by a plain theme-background fill (no flash on fast spawns), and the recorded empty-workspace divergence is fixed — deleting the last project (UI, palette, keybind, or IPC) now exits the app on both platforms, matching the Swift window-close policy, instead of landing in the engine's empty-workspace state; `reconcile()` owns the check plus a new `UiTask::Exit` so `App` still drops and flushes `state.json` | closed | `tools/roosttest/test_exit_on_empty.py` (new, own CI lane — kills its instance) plus the rewritten `test_project_lifecycle.py` keep-alive case; seeded loading/error-toast pixel snapshots remain unbuilt if ever prioritized |
| Hover/focus/disabled states | Subtle per-control hover and visible focus without global blue fills | Corrected 2026-08-07 — "stock theme states" is no longer accurate: explicit custom hover/press/disabled styling ships for footer chip, danger button, palette rows, transparent controls, active agent wash, and rename-input focus border, each unit-tested (`chrome.rs:285-302,136-168,256-283,219-233` + its `#[cfg(test)]` module). **Narrowed further by plan 026 C4 (2026-08-15)**: tab pills now have a hover state — the × glyph tints red on hover, with no pill-background recolor (Charlie's Q3 pick) — and the pre-existing pill-background hover that "looked bad" is gone. Remainder still open: no keyboard focus ring anywhere, no sidebar-row hover distinct from active (both minor, not among the 16 walkthrough items) | P1 → 3h | State-style unit tests exist (audit-verified); tab-pill hover shipped and unit-tested (C4); keyboard focus ring / sidebar-row hover remain unbuilt |
| File/image drops | Swift and GTK accept text/file URI drops and image-paste paths using terminal-attached native adapters | Local file drops anywhere in the owned Iced window target the active terminal when no palette/editor owns input, using the shared GTK/Iced resolver and bracketed-paste path on macOS/X11. Clipboard image paste is also shipped: a System-clipboard read materializes to a GTK-parity temp PNG (same cap/naming/0600 policy) on paste, wrapped through the shared bracketed-paste path. Documented divergences: the pixel cap runs post-decode (arboard decodes internally), uri-list-only file copies stay GTK-only, and macOS is compiled but not live-verified. Exact hit-testing, raw text/URI drops, and native Wayland DnD are upstream-blocked (issue #302) | P1 | Shared payload/PTY-byte tests, real Finder evidence, and a reusable shed XDND guard proven under wgpu/tiny-skia; clipboard image materialization unit/paste-path tests (cap, naming, permissions, no-shell-escaping charset pin); retain the upstream-tracked coordinate/Wayland gaps (#302) |
| Native chrome | Platform-appropriate window controls and title/subtitle behavior | Native winit decorations; renderer screenshot cannot compare them directly | P2 | Platform launch artifacts and manual checklist, separate from content pixels |
| Renderer consistency | The terminal surface fills the available right pane under every supported renderer/backend | Closed for the current shell: renderer-neutral widget begins at x=220/y=32 with the sidebar and x=0/y=32 collapsed under wgpu/tiny-skia on macOS, X11, and Wayland (band height trued up to 32 pt in plan 016) | closed | Focused product screenshot regression runs in the existing renderer matrix; repeatable parity captures remain available for human review |

## Functional interaction gap register

Workspace and IPC operations already exist in the shared engine. “Missing”
below means the Iced presentation does not expose the common direct UI path; it
does not justify a second state machine.

| Operation | Engine/IPC | Iced direct UI | Required adapter work |
|---|---|---|---|
| Select project/tab/agent | implemented | implemented | Preserve while restyling; keep one authoritative active state |
| Sidebar scrolling | UI-owned presentation | implemented | Body scrolls independently while the header/footer stay fixed; constrained real-pointer fixture activates the final row |
| Create project | implemented | implemented (plan 010) | None outstanding. Ships without a directory picker — name `""` + cwd `$HOME`, deliberately matching both shipped UIs (roadmap 3b); footer + keybind + palette routes covered by `test_project_lifecycle.py` and the real-input footer click |
| Rename project | implemented | implemented | Compact stable-ID inline editor; shared trim/no-op policy, physical input, exact dispatch, and relaunch persistence are covered |
| Delete project | implemented | implemented (plan 010) | None outstanding: in-app confirm overlay then engine cascade; real-input keybind confirm/cancel with zero PTY leakage (`iced_clipboard_check.py`); the separate tab-close→project cascade is covered by `_direct_tab_close` |
| Reorder projects | implemented | implemented (plan 010) | None outstanding: vertical `ReorderStrip` instantiation with stable IDs and insertion feedback; real-input XTEST project drag + sub-threshold select covered (`iced_clipboard_check.py`) |
| Open tab | implemented | implemented | Restyle plus control and preserve PTY launch path |
| Close tab | implemented | active-pill close implemented | Keep exact rendered tab IDs and last-tab/project cascade coverage. Hover-close DECIDED against (plan 026, Q3): stays ×-active-only, matching the Mac; not a gap |
| Rename tabs | implemented | implemented | Double-click/configured command uses the authoritative operation; physical input proves focus, cancel, commit, zero PTY leakage, and restoration |
| Reorder tabs | implemented | implemented | Stable-ID pointer preview commits through the authoritative operation and persists. Automatic offscreen reveal shipped (plan 026 C4): `reconcile()` triggers a reveal on every observed active-tab change (keyboard, palette, click, IPC). Hover-close stays out by decision (see Close tab row). A tab-drag press-jump flicker was diagnosed but not fixed — filed [#339](https://github.com/charliek/roost/issues/339) |
| Sidebar collapse | implemented/persisted | implemented | Resolved (plan 016): no in-window affordance is the decided shape (Mac parity), not a gap — reopen via command/shortcut only, plus drag-to-collapse below the half-floor threshold |
| Sidebar resize | UI-owned geometry | implemented | Shipped in slice 3c (plan 011): grip drag adapter + engine-persisted 160–400 pt width policy |
| Notifications inbox | shared model/UI port | implemented | Resolved (plan 016): bell removed; sidebar project-row dots + existing pill badges replace it, palette/keybind own the inbox entry |
| Command/agent/provider palettes | shared model/UI port | implemented | Visual/focus polish; provider activation behavior remains shared |
| New Project command | shared command ID | implemented (plan 010) | None outstanding — palette command routes through the same `new_project` dispatch as the footer and keybind |
| Select Font command | shared command ID | implemented | Shared ordering/resolution/confirmation policy drives toolkit discovery adapters; preview/cancel/confirm and config persistence are covered by the target-neutral Rust-UI E2E |
| Configured workspace shortcuts | shared keybinding IDs | exhaustive dispatch is implemented; every action now routes to a real adapter — plan 010 closed the last project stubs (the `Err("… not available in Iced yet")` grep has zero hits, roadmap :243-249) | None outstanding |
| Font increase/decrease/reset | shared keybinding IDs/config | implemented | Renderer-measured metrics reflow all live tabs atomically and persist exact values; new tabs inherit the live typography |
| Terminal scrollback | shared VT policy | implemented (wheel + bare PageUp/PageDown) | None outstanding — shared mode precedence, snap/selection semantics, and physical wheel + page real-input gates are covered |
| Text/file/image drop | UI-owned native adapter | partial | macOS/X11 local files batch to a stable active tab, share GTK normalization, and honor bracketed paste; clipboard image paste materializes to a GTK-parity temp PNG through the shared paste path. Iced 0.14 has no drag coordinates, so the owned window is the current target boundary. Exact native hit-testing, raw text/URI drops, and native Wayland DnD are upstream-blocked (#302); no further adapter work is possible until iced/winit ships them |
| Native notifications | engine action exists | implemented (Linux) | Linux adapter speaks `org.freedesktop.Notifications` (Desktop Notifications spec) for fire, per-tab replace-not-stack, and click-to-focus via the spec `default` action → focus tab + clear badge + reveal sidebar + best-effort raise; adapter-only, engine suppression untouched. One worker owns all D-Bus I/O so nothing native runs under an engine lock. macOS backend deferred (#303) |
| IME input (terminal) | UI-owned input path | implemented (plan 021, E6) | None outstanding for iced — see the visual register's Terminal IME row; real-IME human smoke on the plan-021 checklist |

## Ordered implementation slices

Every slice remains launchable, adds focused tests, runs under both Iced
renderers, and is pushed only after the complete applicable gate is green.

1. **Deterministic comparison capture:** add one named workspace/palette fixture,
   comparable artifact manifests for all three targets, reusable shell
   geometry/color measurements, and provenance hashes. Visual inspection is
   the parity gate; hashes are not golden assertions. Do not compare native
   decoration pixels across toolkits.
2. **Renderer correctness:** fix the tiny-skia terminal viewport width defect,
   preserve wgpu behavior, and add a focused geometry regression independent of
   visual-parity assertions.
3. **Chrome foundation and tab close:** introduce Iced-owned chrome tokens and
   state styles; replace stock project, agent, tab, add-tab, notification, and
   sidebar controls; use shared lifecycle/rollup derivation; add a real active
   tab close control; reduce band and row density. Preserve all engine APIs and
   compare the resulting named captures against GTK and Swift.
4. **Palette convergence and visual go/no-go:** replace the remaining stock
   input/button treatment with a transparent click catcher, elevated panel,
   compact semantic rows, and reference-like focus/selection states across
   command, agent, provider, and notification frames. Refresh the three-target
   capture and explicitly decide whether Iced still has a credible path to
   reference-level polish.
5. **Scrollable navigation and shortcut safety:** make terminal scrollback
   reachable, then dispatch every configured workspace shortcut before terminal
   encoding so missing UI commands cannot leak bytes. Sidebar and tab chrome
   scrolling landed with slice 3.
6. **Project manipulation (complete — plan 010):** inline rename, create
   (no directory picker — the decided shipped shape, matching both
   references), delete with confirmation, pointer reorder, and the shared
   `new_project` command route all shipped.
7. **Tab manipulation (complete):** inline rename, pointer drag reorder/
   overflow, and restoration coverage are complete; automatic offscreen
   reveal shipped (plan 026 C4); hover-close was decided against (plan 026,
   Q3) — the Mac has none either.
8. **Interaction-cost go/no-go:** after at least one project or tab
   direct-manipulation path is polished and tested, assess focus, accessibility,
   text editing, drag feedback, and custom-widget complexity against GTK. Stop
   treating Iced as a replacement candidate if ordinary product interactions
   require brittle toolkit workarounds, even if the shared engine remains useful.
9. **Sidebar resizing and transient states:** 160–400 pt pointer resize,
   persistence, empty/loading/error treatment, hover/focus/disabled states.
10. **Terminal visual/input convergence:** measured padding/font/baseline,
   font commands, cursor/selection/link colors and geometry, file/image drops,
   then both-renderer artifacts.
11. **Final gap closure:** repeat the inventory against the named fixture;
   accept only explicitly documented native-toolkit differences.

## Palette-feasibility slice result (2026-08-01)

The palette visual go/no-go passes. The original purple rows were Iced's stock
primary-button theme, not a renderer or widget limitation. The Iced adapter now
renders the same `#2d2d33` surface and `#48484e` selection as GTK, with a
transparent input, reference border and shadow, compact neutral hover/disabled
states, fuzzy-match highlighting, platform shortcut labels, distinct agent-row
composition, and content-sized cards that clamp and scroll in short windows.
The underlying terminal stays undimmed, matching both current references.

The reusable comparison fixture now captures command, queried command, agent,
notification, and provider/disabled frames. Human comparison of those named
GTK and Iced artifacts is the parity gate; the functional suite continues to
own command semantics, focus, selection reveal, providers, and notifications.
The Linux real-input lane additionally proves a click inside blank card space
does not dismiss, an exact row click activates once, and an outside click
dismisses without activating the tab control underneath.

This result establishes a credible path to reference-level Iced styling on
macOS, X11, and Wayland with both released renderers. The project owner
accepted the styling-feasibility result and authorized continued parity work.
(Historical note, superseded by later plans and re-verified by the 2026-08-07
audit: the direct-manipulation milestone this paragraph originally set —
footer, project/tab manipulation, sidebar resizing, PageUp/PageDown
scrollback — has since shipped across plans 010-016; the open remainder is
the user-directed 3h polish cluster plus the rows the registers above mark
open.)

## First slice contract

The deterministic-capture commit is intentionally tooling-first so later
visual changes cannot grade themselves against subjective resemblance.

- Scope: harness-owned state/config, one deterministic workspace and palette
  seed, per-target screenshot/manifest output, and reusable application-content
  measurements. No production UI behavior changes and no dedicated parity CI
  job.
- Invariants: all three launches remain hermetic; the fixture uses only common
  IPC/test operations; native decorations are excluded from cross-toolkit pixel
  assertions; text antialiasing is not treated as a stable pixel oracle.
- Failure behavior: a missing component or refused trustworthy geometry
  produces a named diagnostic and preserves the latest PNG; an unsupported
  product-capture surface is recorded explicitly. The tool never silently
  substitutes another target or stale capture.
- Tests: small unit tests for the reusable pixel/provenance helpers, all three
  macOS captures, and local/shed Iced wgpu and tiny-skia X11/Wayland captures.
  Existing focused product tests—not a long-running parity suite—protect
  behavior after convergence.
- Acceptance: one command produces comparable named artifacts for Swift, GTK,
  and Iced; visual review makes the current gaps concrete; the following chrome
  commit can reuse the same fixture without changing its semantics. AppKit's
  product screenshot currently excludes its palette child panel, which is
  declared in metadata rather than represented by a misleading image.

## Current named deferrals

These are sequencing decisions, not accepted final gaps:

- Native window-decoration pixel identity is deferred because each renderer's
  in-process screenshot has a different ownership boundary. User-visible
  launch artifacts still need platform review.
- Native notifications: the Linux D-Bus adapter (fire, GTK-parity replace,
  click-to-focus) shipped in plan 015. The macOS backend remains deferred
  behind a narrow platform port (issue #303); the shared notification inbox
  and lifecycle indicators remain required regardless of backend.
- Arbitrary 2× software-renderer detail remains governed by the capture policy
  in `iced-poc-plan.md`; all focused pixel/geometry gates run at 1×.
- ~~The footer temporarily retains `Hide Sidebar`~~ Resolved (plans 010 +
  016): the footer is the fixed `+ New Project` chip and the create path
  shipped without a directory picker (the decided shape, matching both
  references). Kept for history; see the Sidebar footer row.
- Only the active tab exposes close — decided final (plan 026, Q3), not a
  gap; the Mac has no hover-close either. Automatic reveal after
  programmatic selection of an offscreen tab shipped in plan 026 C4.
  Pointer reorder, drag insertion feedback, persistence, and manual
  horizontal reachability are physical-input gates.
- Iced 0.14's native `FileDropped` event is explicitly unavailable on Wayland.
  Local file drops therefore work on macOS/X11 in the current adapter; an
  accepted Linux replacement still needs upstream support or a narrow Wayland
  platform port. Clipboard image materialization shipped in plan 015; raw
  text/URI drops remain upstream-blocked (#302).
- Iced 0.14/winit does not expose the native drag position on macOS or X11, and
  the ordinary mouse cursor may be unavailable or stale during external DnD.
  The current honest boundary is therefore the owned Iced window: a file drop
  targets the active terminal only while no palette/editor owns input. Exact
  terminal-surface targeting needs upstream coordinates or a native port.
- Iced 0.14 does not expose all desired accessibility labels/tooltips through
  the current compact controls without more adapter work. Semantic icon,
  tooltip, focus-ring, and assistive-technology refinement remains required
  before a release claim.

[#284]: https://github.com/charliek/roost/issues/284
