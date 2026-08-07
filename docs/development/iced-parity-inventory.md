# Iced visual and interaction parity inventory

Status: active POC gap register, not a parity claim

Baseline: `poc/iced` at `dea73a5` (2026-08-01)

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
decision, not a final parity claim; the remaining P0/P1 work below is mostly
missing interaction/polish rather than inability to express the look.

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
| Agent rows | Transparent compact nested rows; only active agent has a faint wash; lifecycle dot, name, status, and time have distinct roles | Shared lifecycle colors/alignment and a gray active wash are implemented; current one-line rows remain taller and typographically less distinct than GTK/AppKit | P1 | Add active-row background and row-height bounds to the existing four-state capture; retain click E2E |
| Sidebar footer | Centered compact `+ New Project` action in a separated footer | `Hide Sidebar` full-width action occupies list content; no visible project creation | P0 | Real directory-selection/create path plus capture and functional test |
| Sidebar overflow | Project and agent lists scroll vertically without moving the header/footer | Closed: body scrolls independently and final row activates in a constrained real-pointer fixture | closed | Small-window many-row fixture, wheel/drag navigation, final-row activation |
| Sidebar collapse/resize | Both references expose a toolbar toggle. Swift persists a 160–400 pt user width; GTK uses a 160 pt minimum/default 220 pt `GtkPaned` without persisting a 400 pt cap | Resolved (plan 016): no in-window collapse affordance, matching the Mac — reopen via keybind/palette only, the ☰ button is removed. Dragging the grip past half the 160 pt floor (below 80 px, unclamped) collapses the sidebar and drops the live drag width without committing, so reopening restores the pre-drag committed width rather than the 160 pt floor. Recorded parity divergence: NSSplitView lets a drag continue past the floor and re-expand within the same gesture; here the grip leaves the tree at collapse, so drag-back-to-reopen within one gesture is impossible | closed | Resize: functional e2e + real-input grip segment (shipped, plan 011). Collapse: unit matrix (threshold crossing, no-Dragged-after-Collapse, released-event no-op, reopen-width invariant) plus capture (shipped, plan 016) |
| Tab strip | About 24 pt pills in a compact band with 6 pt gaps and horizontal overflow | Closed for active/manual reachability: 24 pt dark pills in a 32 pt band (trued up from 34 pt in plan 016) with independent horizontal overflow | closed | Band/pill geometry assertions under both renderers; overflow test |
| Tab status | Shared lifecycle dot at leading edge, white active label, muted inactive label | Implemented with shared lifecycle derivation; inactive slots are transparent, but the current parity fixture does not yet pin dot/label geometry | P1 | Add focused status-slot geometry/color capture; retain the semantic color unit test |
| Tab close/badge | Active or hovered pill exposes close; inactive notification uses a distinct blue trailing badge | Active exact-ID close and blue badge implemented; hover-close remains deferred | P1 | Real click-close test, badge color/position assertion, notification clear test |
| Tab rename | Inline rename through double-click or the configured command, with authoritative persistence | Closed: compact inline editor uses stable IDs, select-all focus, Enter commit, Escape/click-away cancel, and shared GTK/Iced trim/no-op policy | closed | X11 physical shortcut/double-click/Enter/Escape/click-away gate, zero PTY leakage, relaunch persistence, and named GTK/Iced captures |
| Tab reorder | Pointer drag reorder with visible insertion feedback | Closed: stable-ID drag preview, insertion feedback, exact authoritative commit, cancellation, overflow, and relaunch persistence work under both renderers | closed | Bidirectional physical X11/Wayland input, outside-release/palette cancellation, zero PTY leakage, and named product captures |
| New-tab affordance | Compact plus control following the pills | Closed (re-shaped by plan 016): the plus sits inside the scrolling strip 6px after the last pill and scrolls with overflow — Mac parity, where ＋ is an arranged strip subview. Under overflow it scrolls offscreen (accepted; keybind/palette still create tabs) | closed | Real-input click computed from the last pill's rendered right edge opens one PTY-backed tab (plan 016 harness rework) |
| Notification entry | Header bell with count badge opens the inbox palette (original framing) — the Mac reference actually has no bell at all | Resolved (plan 016): the bell is removed entirely — a deliberate divergence from this row's original framing, brought into line with the Mac reference, which has no bell. Shipped shape: sidebar project-row dots (any project with a notifying tab, active project included) plus the existing pill badges; the palette/keybind own the inbox entry | closed | Sidebar-dot + pill-badge capture; `roostctl notify` demo comparison against the Mac; inbox-via-palette/keybind e2e (shipped, plan 016) |
| Terminal padding | Compact consistent inset around the grid — the Mac reference is edge-pinned at zero | Resolved (plan 016): `TERMINAL_PADDING` 0 on all sides, Mac-parity — the measurement half of this row is done. Typography/glyph-baseline comparison against the reference remains open (3h) | P1 | Cell-origin/viewport-edge assertions at padding 0 (shipped, plan 016); glyph-baseline comparison tracked under 3h |
| Terminal scrollback | Wheel/page navigation scrolls retained history locally when mouse reporting is off; alternate-screen behavior follows terminal modes | Closed: wheel and bare PageUp/PageDown page navigation both route through the shared GTK/Iced `roost-vt` policy — retained history, exact bottom state, next-terminal-key snap, mouse-report precedence, alternate-screen arrows/forwarding, and a full-viewport local page move that preserves selection and bypasses snap only on the local route. Swift Mac has no PageUp/PageDown scrollback route (deliberate Rust-UI-first divergence; no prior reference behavior existed) | closed | Physical X11 wheel under both renderers; `roost-vt::route_page` unit/fixture coverage plus both-UI (GTK/Iced) adapter fixtures (selection preserved, zero local PTY bytes, byte-identical Forward path); physical PageUp/PageDown segment in `iced_clipboard_check.py` |
| Terminal typography | Configured family/size, baseline and cell metrics stable across styles and graphemes | Renderer-measured size and installed-family selection now reflow every live tab atomically, persist through the shared config policy, and reach new/restored tabs; focused glyph baseline/style comparison remains | P1 | Latin/wide/combined/style fixture under wgpu and tiny-skia; shared GTK/Iced font selection E2E |
| Terminal cursor/selection/link | Shared colors and modes with reference-like cursor, selection, and link feedback | Functional coverage exists; geometry/color comparison remains incomplete | P1 | Focused cursor/selection/link screenshots plus existing real-input gates |
| Terminal box-drawing/block glyphs | Both shipped UIs draw U+2500–U+257F and U+2580–U+259F geometrically rather than from the font, because font glyphs do not tile pixel-perfectly across adjacent cells — `mac/Sources/Roost/Sprite.swift` and `crates/roost-linux/src/sprite.rs`, both ports of Ghostty's `font/sprite/draw/{block,box}.zig` and kept in lockstep per the CLAUDE.md parity rule | **No sprite path in `roost-iced` at all** — box-drawing and block elements fall through to font glyphs, so TUI chrome shows hairline seams (most visible in wordmark/logo art) that neither shipped UI has. Row added 2026-08-06; the gap existed unnoticed because this inventory had no box-drawing row. Tracked as engine-track slice E5: the Linux sibling is already Rust, so the work is move-to-shared-crate plus a draw call | P1 | Seam-free capture of a box-drawing/block fixture under both renderers, cross-checked against the GTK and Mac references; codepoint-dispatch unit coverage mirroring the existing Swift/Rust sprite tests |
| Palette placement | Centered, elevated compact panel over an undimmed terminal, styled semantic rows, keyboard focus, and scrolling | Closed for the visual-feasibility slice: exact reference neutrals, border/shadow, content-sized 660 pt card capped at 500 pt, compact command/agent/notification/provider rows, shortcut hints, fuzzy-match accents, disabled state, and a narrow neutral scrollbar | closed | Five named GTK/Iced captures; focus/scroll tests plus real row/card/outside pointer routing |
| Empty/loading/error states | Deliberate shell placeholders without changing hierarchy | Plain `Starting terminal…`; status errors are appended to the sidebar | P1 | Seeded empty/loading/error snapshots and recovery tests |
| Hover/focus/disabled states | Subtle per-control hover and visible focus without global blue fills | Mostly inherited stock theme states | P1 | Renderer-neutral state-style unit tests plus real pointer/keyboard capture |
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
| Create project | implemented | missing | Native/portal directory picker, then engine command; no renderer dependency in engine |
| Rename project | implemented | implemented | Compact stable-ID inline editor; shared trim/no-op policy, physical input, exact dispatch, and relaunch persistence are covered |
| Delete project | implemented | missing | Context menu or equivalent, confirmation, and deterministic error handling |
| Reorder projects | implemented | missing | Pointer drag with stable IDs and explicit insertion feedback |
| Open tab | implemented | implemented | Restyle plus control and preserve PTY launch path |
| Close tab | implemented | active-pill close implemented | Keep exact rendered tab IDs and last-tab/project cascade coverage; add hover-close polish separately |
| Rename tabs | implemented | implemented | Double-click/configured command uses the authoritative operation; physical input proves focus, cancel, commit, zero PTY leakage, and restoration |
| Reorder tabs | implemented | implemented | Stable-ID pointer preview commits through the authoritative operation and persists; hover-close and automatic offscreen reveal remain polish |
| Sidebar collapse | implemented/persisted | implemented | Resolved (plan 016): no in-window affordance is the decided shape (Mac parity), not a gap — reopen via command/shortcut only, plus drag-to-collapse below the half-floor threshold |
| Sidebar resize | UI-owned geometry | implemented | Shipped in slice 3c (plan 011): grip drag adapter + engine-persisted 160–400 pt width policy |
| Notifications inbox | shared model/UI port | implemented | Resolved (plan 016): bell removed; sidebar project-row dots + existing pill badges replace it, palette/keybind own the inbox entry |
| Command/agent/provider palettes | shared model/UI port | implemented | Visual/focus polish; provider activation behavior remains shared |
| New Project command | shared command ID | reports unimplemented | Route through the same Iced directory-picker port as the footer |
| Select Font command | shared command ID | implemented | Shared ordering/resolution/confirmation policy drives toolkit discovery adapters; preview/cancel/confirm and config persistence are covered by the target-neutral Rust-UI E2E |
| Configured workspace shortcuts | shared keybinding IDs | exhaustive dispatch is implemented; supported actions route to the workspace/UI port and unavailable project actions show a deterministic status without PTY bytes | Implement the remaining project adapters behind the already-safe action routes |
| Font increase/decrease/reset | shared keybinding IDs/config | implemented | Renderer-measured metrics reflow all live tabs atomically and persist exact values; new tabs inherit the live typography |
| Terminal scrollback | shared VT policy | implemented (wheel + bare PageUp/PageDown) | None outstanding — shared mode precedence, snap/selection semantics, and physical wheel + page real-input gates are covered |
| Text/file/image drop | UI-owned native adapter | partial | macOS/X11 local files batch to a stable active tab, share GTK normalization, and honor bracketed paste; clipboard image paste materializes to a GTK-parity temp PNG through the shared paste path. Iced 0.14 has no drag coordinates, so the owned window is the current target boundary. Exact native hit-testing, raw text/URI drops, and native Wayland DnD are upstream-blocked (#302); no further adapter work is possible until iced/winit ships them |
| Native notifications | engine action exists | implemented (Linux) | Linux D-Bus adapter ships fire, GTK-parity replace-not-stack, and click-to-focus via the freedesktop default action → focus tab + clear badge + reveal sidebar + best-effort raise; adapter-only, engine suppression untouched. The "never issue native UI calls from engine locks" constraint is satisfied structurally: the engine drain sends over a channel, one worker owns all notify-rust I/O. macOS backend deferred (#303) |

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
6. **Project manipulation:** inline rename is complete; add portal/native
   directory selection, create, delete, pointer reorder, and the shared
   `new_project` command route.
7. **Tab manipulation:** inline rename, pointer drag reorder/overflow, and
   restoration coverage are complete; add hover-close and automatic offscreen
   reveal behavior.
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
macOS, X11, and Wayland with both released renderers. The project owner accepted
the styling-feasibility result and authorized continued parity work; no further
approval pause is required. Direct manipulation is the next implementation and
cost-validation milestone: project or tab editing must still demonstrate that
focus, text editing, drag feedback, accessibility, and maintenance cost can be
competitive with GTK. The footer, project/tab manipulation, sidebar resizing,
dedicated PageUp/PageDown scrollback handling, terminal glyph-level visual
comparison, and several native adapters remain material P0/P1 work elsewhere
in this register.

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
- The footer temporarily retains `Hide Sidebar` until the project-manipulation
  slice supplies the shared `New Project` route and a portal/native directory
  picker; the missing direct creation path remains P0.
- Only the active tab exposes close. Hover-close and automatic reveal after
  programmatic selection of an offscreen tab remain in the tab-manipulation
  slice. Pointer reorder, drag insertion feedback, persistence, and manual
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
