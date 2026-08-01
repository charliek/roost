# Iced visual and interaction parity inventory

Status: active POC gap register, not a parity claim

Baseline: `poc/iced` at `dea73a5` (2026-08-01)

This inventory turns the Iced POC's remaining product work into named,
testable slices. GTK is the primary Linux visual reference. Swift/AppKit is a
second product reference, especially where GTK's client-side decorations or
desktop integration are toolkit-specific. A difference is acceptable only
when this document names the reference, reason, affected platforms, and
user-visible impact.

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
| Sidebar width | 220 pt | 220 pt | 220 pt |
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

## Visual gap register

Priority meanings: P0 blocks a usable parity claim, P1 is required common
product polish, and P2 is an optional native/toolkit refinement.

| Area | Reference behavior | Current Iced behavior | Priority | Acceptance evidence |
|---|---|---|---:|---|
| Shell hierarchy | Sidebar and tab strip are compact chrome bands around a darker terminal | Correct broad columns, but stock controls dominate and the 44 pt tab band is too tall | P0 | Same named fixture; focused band-height/background assertions; side-by-side capture |
| Sidebar surface | 220 pt default, `#282828` GTK chrome; Swift material resolves near `#3a3a3a`; header reads `PROJECTS` | 220 pt but near-black `#111111`; header reads `ROOST` | P0 | Metrics remain 220 pt; background sample and header-content assertion |
| Project rows | 28 pt compact rows; active project is an inset deep-blue rounded pill; lifecycle rollup is a narrow leading stripe | Full-width stock indigo buttons; active state is a text bullet; no rollup stripe | P0 | Selected/unselected geometry and color assertions; lifecycle stripe fixture; click E2E |
| Agent rows | Transparent compact nested rows; only active agent has a faint wash; lifecycle dot, name, status, and time have distinct roles | Every row is an indigo button; active state is an arrow suffix; two-line layout is much taller | P0 | Four-state capture, active-row background assertion, row-height bound, click E2E |
| Sidebar footer | Centered compact `+ New Project` action in a separated footer | `Hide Sidebar` full-width action occupies list content; no visible project creation | P0 | Real directory-selection/create path plus capture and functional test |
| Sidebar overflow | Project and agent lists scroll vertically without moving the header/footer | One unscrollable column; enough rows become unreachable below the window | P0 | Small-window many-row fixture, wheel/drag navigation, final-row activation |
| Sidebar collapse/resize | Both references expose a toolbar toggle. Swift persists a 160–400 pt user width; GTK uses a 160 pt minimum/default 220 pt `GtkPaned` without persisting a 400 pt cap. Intended Iced policy: persisted 160–400 pt, matching Swift while retaining GTK's 220 pt default | Collapse works through button/command but has no reference-like affordance; fixed 220 pt width | P1 | Keyboard/click/persistence tests; resize metrics and pointer test |
| Tab strip | About 24 pt pills in a compact band with 6 pt gaps and horizontal overflow | 44 pt band of stock indigo buttons | P0 | Band/pill geometry assertions under both renderers; overflow test |
| Tab status | Shared lifecycle dot at leading edge, white active label, muted inactive label | State is encoded as text bullet; no faithful status-slot geometry | P0 | Shared lifecycle-color assertion and exact status-slot geometry |
| Tab close/badge | Active or hovered pill exposes close; inactive notification uses a distinct blue trailing badge | No close control; notification changes the text prefix; global notifications is a text button | P0 | Real click-close test, badge color/position assertion, notification clear test |
| Tab rename/reorder | Inline rename and pointer drag reorder with visible insertion feedback | IPC operations work, but there is no direct Iced UI | P0 | Keyboard/pointer functional tests, persistence after relaunch |
| New-tab affordance | Compact plus control following the pills | Working but stock primary button | P1 | Click opens one PTY-backed tab; compact geometry assertion |
| Notification entry | Header bell with count badge opens the inbox palette | Text button in the tab band | P1 | Bell/count capture and click-to-palette E2E |
| Terminal padding | Compact consistent inset around the grid | 12 pt inset, visibly close but not yet measured against both references | P1 | Cell-origin and viewport-edge assertions at fixed size |
| Terminal scrollback | Wheel/page navigation scrolls retained history locally when mouse reporting is off; alternate-screen behavior follows terminal modes | 2,000 rows are retained but Iced has no local viewport-scroll path; non-reporting wheel events are dropped | P0 | Long-output wheel/page fixture, snap to bottom on non-modifier input, output preservation behavior compared with both references, alternate-screen and mouse-reporting tests |
| Terminal typography | Configured family/size, baseline and cell metrics stable across styles and graphemes | PTY and glyph rendering work; font selection is currently a one-row placeholder and baseline parity is unmeasured | P0 | Latin/wide/combined/style fixture under wgpu and tiny-skia; font selection E2E |
| Terminal cursor/selection/link | Shared colors and modes with reference-like cursor, selection, and link feedback | Functional coverage exists; geometry/color comparison remains incomplete | P1 | Focused cursor/selection/link screenshots plus existing real-input gates |
| Palette placement | Centered, elevated compact panel with dimmed background, styled rows, keyboard focus, and scrolling | Functional centered panel, but stock input/buttons and no scrim/elevation parity | P0 | Named command/agent/notification captures; focus/scroll/click E2E |
| Empty/loading/error states | Deliberate shell placeholders without changing hierarchy | Plain `Starting terminal…`; status errors are appended to the sidebar | P1 | Seeded empty/loading/error snapshots and recovery tests |
| Hover/focus/disabled states | Subtle per-control hover and visible focus without global blue fills | Mostly inherited stock theme states | P1 | Renderer-neutral state-style unit tests plus real pointer/keyboard capture |
| File/image drops | Swift and GTK accept text/file URI drops and image-paste paths using UI-owned native adapters | No Iced drop or image-paste adapter | P1 | Text/file/image payload tests, shell escaping parity, platform launch smoke |
| Native chrome | Platform-appropriate window controls and title/subtitle behavior | Native winit decorations; renderer screenshot cannot compare them directly | P2 | Platform launch artifacts and manual checklist, separate from content pixels |
| Renderer consistency | The terminal surface fills the available right pane under every supported renderer/backend | Closed for the current shell: renderer-neutral widget begins at x=220/y=44 with the sidebar and x=0/y=44 collapsed under wgpu/tiny-skia on macOS, X11, and Wayland | closed | Focused product screenshot regression runs in the existing renderer matrix; repeatable parity captures remain available for human review |

## Functional interaction gap register

Workspace and IPC operations already exist in the shared engine. “Missing”
below means the Iced presentation does not expose the common direct UI path; it
does not justify a second state machine.

| Operation | Engine/IPC | Iced direct UI | Required adapter work |
|---|---|---|---|
| Select project/tab/agent | implemented | implemented | Preserve while restyling; keep one authoritative active state |
| Sidebar scrolling | UI-owned presentation | missing | Wrap only the project/agent list in a scrollable viewport; keep header/footer fixed |
| Create project | implemented | missing | Native/portal directory picker, then engine command; no renderer dependency in engine |
| Rename/delete project | implemented | missing | Context menu or equivalent, inline rename, confirmation/error handling |
| Reorder projects | implemented | missing | Pointer drag with stable IDs and explicit insertion feedback |
| Open tab | implemented | implemented | Restyle plus control and preserve PTY launch path |
| Close tab | implemented | palette/IPC only | Direct pill close with last-tab/project cascade semantics |
| Rename/reorder tabs | implemented | missing | Inline rename and pointer drag; persist through engine events |
| Sidebar collapse | implemented/persisted | implemented | Move affordance into chrome; retain command and shortcut convergence |
| Sidebar resize | UI-owned geometry | missing | Iced split/drag adapter and persisted width policy |
| Notifications inbox | shared model/UI port | implemented | Replace text control with bell/badge without changing model |
| Command/agent/provider palettes | shared model/UI port | implemented | Visual/focus polish; provider activation behavior remains shared |
| New Project command | shared command ID | reports unimplemented | Route through the same Iced directory-picker port as the footer |
| Select Font command | shared command ID | placeholder/no-op | Enumerate/select fonts through a UI adapter and apply to live/new tabs |
| Configured workspace shortcuts | shared keybinding IDs | copy/paste and palette actions work; new/close/rename/cycle/project/sidebar/font actions fall through to PTY encoding | Dispatch every shared non-terminal action before encoding; unimplemented actions must surface an error, never leak bytes |
| Font increase/decrease/reset | shared keybinding IDs/config | missing | UI adapter updates effective metrics and resizes every terminal deterministically |
| Terminal scrollback | shared VT API | missing in Iced | Route non-reporting wheel/page input to `Terminal::scroll_viewport`; preserve mouse-reporting bytes |
| Text/file/image drop | UI-owned native adapter | missing | Normalize payloads, reuse shared escaping where possible, then send explicit PTY bytes |
| Native notifications | engine action exists | deferred | Add platform adapter only; never issue native UI calls from engine locks |

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
4. **Scrollable navigation and shortcut safety:** make the sidebar list and
   terminal scrollback reachable, then dispatch every configured workspace
   shortcut before terminal encoding so missing UI commands cannot leak bytes.
5. **Project manipulation:** portal/native directory selection, create,
   rename, delete, reorder, and the shared `new_project` command route.
6. **Tab manipulation:** inline rename, drag reorder/overflow, hover-close
   behavior, and restoration coverage.
7. **Sidebar resizing and transient states:** 160–400 pt pointer resize,
   persistence, empty/loading/error treatment, hover/focus/disabled states.
8. **Palette convergence:** scrim, panel elevation, row density, semantic
   status/trailing columns, and command/agent/provider/notification captures.
9. **Terminal visual/input convergence:** measured padding/font/baseline,
   font commands, cursor/selection/link colors and geometry, file/image drops,
   then both-renderer artifacts.
10. **Final gap closure:** repeat the inventory against the named fixture;
   accept only explicitly documented native-toolkit differences.

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
- Native notifications remain deferred until a narrow platform port is
  designed and tested. The shared notification inbox and lifecycle indicators
  remain required.
- Arbitrary 2× software-renderer detail remains governed by the capture policy
  in `iced-poc-plan.md`; all focused pixel/geometry gates run at 1×.
