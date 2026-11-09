# Chrome size, colors, opacity and looks (discovery)

Status: **discovery**, questions for a future, dedicated planning session.
It isn't a commitment. Written 2026-10-03 (at `21936f7`), updated 2026-11-08, as of
`main` @ `68231b9b`. Every Roost file:line below was read at that SHA.

**Timing:** v0.1.0 shipped on 2026-11-07. The Swift app is retired (DL-33), so iced is the
one desktop UI. Every item here is iced only and must cover **both macOS and Linux**.

**Order (Charlie, 2026-11-08):**
1. **Chrome size first.** Make the chrome text bigger, for example when tired.
2. **Then chrome colors and opacity,** in either order.
3. **Quick switching,** so size, colors and opacity can match the lighting, tiredness or
   mood in one step.
4. **Following the system light/dark setting is deferred.** It isn't a priority, especially
   if it's a lot of work.

This note owns chrome **size, scale, colors and opacity**. Chrome typography quality (the
font face, weights, hinting and rendering) belongs to the chrome-text section of
[`text-rendering.md`](text-rendering.md).

**Tracked by:**
- #584 (open): chrome themes and user color overrides on `ChromePalette`.
- #162 (open): Ctrl/Cmd + wheel font zoom. Terminal only; adjacent.
- #164 (open): following the system light/dark setting. **Deferred.** Its title still
  starts "mac:", although the work is now both OSes.
- #209 (open): libghostty device-query replies, including DSR `?996`. Only needed for the
  deferred light/dark work.

There is **no issue** for chrome font size, chrome opacity or looks.

---

## Summary

- **Chrome size is a geometry change, not a text change.**
  - The chrome has 10 distinct text sizes (10, 10.5, 11, 11.5, 12, 13, 14, 15, 16, 17 px),
    at 56 `.size()` sites: 36 literals in `app.rs`, 20 named constants.
  - Text is coupled to geometry: 7 pinned line heights, the 32 / 24 / 42 / 44 px bands
    and rows, measured width budgets, and `terminal_top`.
  - About 212 references to `chrome::` constants sit outside `chrome.rs`: 122 in `app.rs`,
    74 in `app/agents_panel.rs`, 12 in `agents_split.rs`.
  - So a text-only knob would clip text unless rows and lines scale too.
- **Recommended seam: `ChromeMetrics`,** the geometry twin of `ChromePalette`.
  - Its value at the default size is today's constants, byte for byte, at any device scale.
  - Any other size multiplies every length and snaps heights to whole device pixels.
  - A size change re-grids every terminal through the batch path font zoom already uses.
- **Recommended user model:** one key, `chrome-font-size`, default 13 (the project label).
  - Everything scales by `chrome-font-size / 13`. Zed works the same way: its UI font
    size is the UI's rem size.
  - Three actions: Increase, Decrease and Reset Chrome Size. Palette rows and macOS View
    menu items; written through `config_writer` as `font-size` is.
- **Colors:** about 10 semantic inputs derive all 42 `ChromePalette` fields.
  - `roost-dark` stays a literal special case, so the default never drifts.
  - A few bundled chrome themes aimed at lighting and tiredness (`roost-dim`,
    `roost-contrast`, `roost-light`), plus `auto` from the terminal theme.
  - Select Chrome Theme… with live preview, built on the Select Theme code.
- **Opacity:** live changes need a window **created** transparent.
  - iced has no runtime transparency call, and X11 can only set it at creation.
  - Minimum path: create the window transparent when any opacity key is below 1 at launch.
    After that, values change live. Switching on from opaque needs a relaunch.
  - The terminal paints its default background as an opaque color today, so the terminal
    widget needs an alpha seam too.
- **Quick switching:** "looks" as macros, not layered profiles.
  - A look is a small file of appearance-only keys. Applying one runs and writes each key.
  - Select Look… previews live; `apply_look:<name>` binds one to a key.
- **Light/dark following stays deferred.** Two pieces of groundwork still pay now:
  - build iced's `Theme` from `ChromePalette` (A1), needed by any light chrome theme;
  - resolve the macOS accent under the chrome's own appearance.

---

## 1. Chrome size

### Today

All values are logical px (macOS points; 1/96 in on Linux). "Const" means named in
`crates/roost-iced/src/chrome.rs` unless another file is given.

**Text sizes.**

| Role | Size | Where |
|---|---|---|
| Sidebar band label ("PROJECTS", host label), SemiBold | 11 | `SIDEBAR_BAND_LABEL_SIZE`, `app.rs:2127` |
| Project label | 13 | `PROJECT_LABEL_SIZE`, `chrome.rs:111` |
| Project agent count | 10.5 | `chrome.rs:112` |
| Host rollup and fidelity pill / banner / banner action | 10 / 12 / 11.5 | `chrome.rs:100`, `:106-107` |
| "↻ Reconnect" and fidelity rows | 12 | `app.rs:6795-6822` |
| Agents panel name / detail / group / empty / sort | 13 / 11 / 12 / 11.5 / 11 | `chrome.rs:148-161` |
| Tab title (Medium when active) | 12 | `TAB_TITLE_SIZE`, `chrome.rs:59` |
| "+" new tab / "×" close / "+ New Project" | 16 / 13 / 13 | `app.rs:7238`, `:7191`, `:6909` |
| Context menu | 13 | `LABEL_SIZE`, `app/context_menu.rs:39` |
| Palette input / row title / subtitle and trailing | 17 / 14 / 12 | `app.rs:7343`, `:7417`, `:7424-7433` |
| Agent palette columns | 14 / 13, metrics 12 monospace | `app.rs:2284-2304`, `:7360-7398` |
| Modal title / body and buttons | 15 / 12 | `modal_heading`, `app.rs:563-571`; `:605-645` |
| Agent-hooks dialog chips, notes, footer | 10–12 | `app.rs:652-731` |
| Toast / notice strip | 12 / 12 and 11.5 | `app.rs:7311-7323`; `:814-856` |

- **Unsized text:** iced's default (16 px, `iced_core-0.14.0/src/settings.rs:26`, `:51`)
  applies to any text with no `.size()`. A rough scan found a `.size()` near every
  `text(`, `rich_text(` and `text_input(` call in `app.rs` and `app/*.rs`. Inferred, not
  parsed. `default_text_size` is a launch-time setting, so it can't be a runtime zoom.

**Geometry.**

| Item | Value | Where |
|---|---|---|
| Band (sidebar header, host band, tab strip) | 32 | `BAND_HEIGHT`, `chrome.rs:13` |
| Project row / selection pill | 32 / 30 | `ROW_HEIGHT`, `chrome.rs:14`; insets `:40-41` |
| Rollup rail | 3 wide, 22 tall | `chrome.rs:36-37` |
| Tab pill height / pill padding y | 24 / 4 | `chrome.rs:15`, `:19` |
| Tab pill width band | 80–220 | `chrome.rs:52-53` |
| Footer band | 44 = 24 + 8 + 12 | `chrome.rs:28-30` |
| Agents panel row | 42 = 2×4 + 18 + 16 | `chrome.rs:127-129`, `:149-151` |
| Agents panel group head | 27 = 8 + 16 + 3 | `chrome.rs:152-157` |
| Agents split divider / nub | 6 / 28×2 | `chrome.rs:119-120` |
| Palette card | 660 wide, at most 500 tall | `chrome.rs:68-69` |
| Confirm card / toast / rename field | 420 / 520 / 140 | `app.rs:165`, `:7311-7323`, `:169` |
| Context menu | item 24, separator 9, width 180–360 | `app/context_menu.rs:35-41` |
| Minimum window | 640×360 | `MIN_WINDOW_SIZE`, `app.rs:159` |
| Sidebar width | 160–400, default 220; owned and persisted by the workspace | `crates/roost-engine/src/workspace.rs:75-79`, `set_sidebar_width` `:1111` |

**Pinned line heights (7 sites).**
- Five in `app/agents_panel.rs:298-400`, from the `AGENT_PANEL_*_LINE` constants.
- Both inline-rename editors, at an absolute 18 px (`app.rs:6565`, `:7134`). The comment at
  `app.rs:6559-6564` explains why: an integral editor height keeps the 1 px focus border
  on whole pixels.

**Values derived from sizes.**
- Width budgets measured at a size: `project_label_budget` (`app.rs:2226`),
  `band_text` (`app.rs:2189-2223`), `text_budgets` (`app/agents_panel.rs:35-48`),
  `pill_title_metrics` and `pill_trailing_width` (`app.rs:3247-3273`).
- The agents panel floor (`agents_panel.rs:80-94`) and the projects floor
  (`agents_split.rs:28`).
- The context menu is measured **once at open** with `LABEL_SIZE`
  (`fitted`, `context_menu.rs:480-510`).
- `terminal_viewport` starts the terminal at `y = chrome::BAND_HEIGHT`
  (`app/terminal_tab.rs:489-496`). `app.window_metrics` reports that as `terminal_top`
  (`app/servicing.rs:4110`).
- Caches: `ElisionMemo` is keyed by font, size bits and width bits (`chrome.rs:511`), so
  it is size-safe. `pill_labels` (`app.rs:3421`) is keyed by title, active and
  notification, **not by size**, and its own doc says it must grow if the font changes.

**Tests and harnesses that hard-code chrome geometry.**

| Value | Where |
|---|---|
| `BAND_HEIGHT = 32` | `tools/roosttest/test_tab_strip_pixels.py:84` |
| `BAND_HEIGHT`, `ROW_HEIGHT = 32`, grouped floor `32+3+27+42`, `FOOTER_HEIGHT = 44` | `tools/roosttest/test_agent_panel.py:370-380` |
| `BAND_HEIGHT = 32.0`, `ROW_HEIGHT = 32.0` | `tools/roosttest/test_real_input_mac.py:153-157` |
| `SIDEBAR_ROW_HEIGHT = 32` | `tools/input/linux/iced_clipboard_check.py:56` |
| Agent row 42, group 27, pill padding 4, footer = pill + 20 | Rust tests at `chrome.rs:1230`, `:1452`, `:1514` |

`app.window_metrics` (`crates/roost-ipc/src/messages.rs:2112`, built at
`servicing.rs:4103-4122`) reports window, sidebar, terminal origin, cells and
`scale_factor`, but **no chrome geometry**. No IPC result reports chrome size, chrome
theme or opacity.

**What a chrome-size feature can reuse.**
- **Font zoom actions:** `FontIncrease`, `FontDecrease`, `FontReset`
  (`crates/roost-ui-model/src/keybind.rs:91-96`, names `:161-163`), bound to primary
  `+`, `=`, `-` and `0` (`:515-537`). Primary is `super` on macOS and `alt` on Linux
  (`:355-359`).
- **Palette rows:** "Increase / Decrease / Reset Font Size"
  (`crates/roost-ui-model/src/palette.rs:256-258`).
- **macOS View menu:** "Zoom In / Zoom Out / Actual Size"
  (`crates/roost-iced/src/macos/menu.rs:706-708`).
- **Apply and persist:** `apply_font_size_transition` re-grids every tab with an
  all-or-nothing batch, then writes `font-size` through `config_writer`
  (`app/palettes.rs:748-800`).
- **Live-preview palette frames:** `preview_selected_palette_item` dispatches on the
  frame id, "themes" or "fonts" (`palettes.rs:2068-2079`); dismiss restores through
  `restore_palette_theme` / `restore_palette_family` (`:2200-2212`).
- **Tests drive zoom through the palette,** not keys: `test_z_typography.py` activates
  `font_increase`. `app.keybind_dispatch` accepts only `paste`
  (`servicing.rs:1215-1226`). So every new action needs a palette row to be e2e-testable.
- **No config hot reload.** Only the UI's own `config_writer` changes appearance keys at
  runtime. A hand edit needs a relaunch.

**Scaling today.**
- Chrome scales only with the OS window scale factor. It ignores terminal zoom.
- The terminal uses points: 14 pt on macOS, 13 pt on Linux
  (`crates/roost-ui-model/src/typography.rs:12-16`), and `POINT_TO_LOGICAL_PIXEL` is
  96/72 on Linux (`terminal_widget.rs:33-38`). So the Linux terminal is 17.3 px beside
  12–13 px chrome.
- **X11:** winit reads `Xft.dpi`, so desktop text scaling that raises it enlarges the whole
  window (inferred for GNOME Large Text).
- **Wayland:** only the compositor's fractional scale. The desktop `text-scaling-factor`
  is ignored.
- **COSMIC (this box, via the portal):** UI font Open Sans 10.5 pt, about 14 px;
  `text-scaling-factor` 1.0. GNOME's default is Cantarell 11 pt, about 14.7 px. So 13 px
  chrome is smaller than native Linux UI text.
- **macOS:** no system-wide app text size applies to a custom-drawn app (inferred). So a
  Roost knob is the only lever there.

### How others let users enlarge UI text

- **Zed** (GPL-3.0 app crates; behavior only, nothing quoted).
  - Two sizes: `buffer_font_size` 15 and `ui_font_size` 16
    (`assets/settings/default.json:41`, `:71`), plus agent-panel sizes (`:75-79`).
  - **The UI font size is the UI scale.** It sets the window's rem size
    (`crates/theme_settings/src/settings.rs:604-613`), and spacing is in rems.
  - Increase / Decrease / Reset UI Font Size step 1 px, clamped 6–100 px
    (`crates/zed/src/zed.rs:1165-1208`). Each has a `persist` flag. `ResetAllZoom` resets
    every size (`:1257`).
  - Default workspace keys zoom the **buffer**; UI zoom is reached mostly through the
    palette.
  - `profiles` are named overlays of settings, "temporarily applied" per its own comment;
    the documented "Presenting" example raises all three sizes and switches the theme
    (`default.json:3040-3069`).
- **cmux** (GPL-3.0-or-later; docs only, `docs/customizing-appearance.md:135-146`).
  - `sidebar-font-size` 10–20 (default 12.5) and `surface-tab-bar-font-size` 8–14
    (default 11).
  - `app.globalFontMagnification` 50–200 in steps of 10 scales terminals, tab titles,
    sidebars and other chrome together.
- **VS Code** (web sources only). `window.zoomLevel` zooms the whole window, about 20% per
  step; `editor.fontSize` is separate.
- **Ghostty** (MIT). No chrome size knob; chrome follows the OS. Its font actions take
  parameters, such as `set_font_size:14.5` (`src/input/Binding.zig:388-408`).
- **WezTerm** (MIT). `window_frame.font_size` (12), `command_palette_font_size` and
  `char_select_font_size` are separate from the terminal size, and follow DPI.
- **herdr** (Apache-2.0) is a TUI; its chrome is the host terminal's font.

**Pattern.** Every GUI peer separates chrome size from content size. Zed and cmux each
offer one "scale the chrome" knob. Zed's choice, UI font size as the scale, is the
simplest model that matches Charlie's ask.

### Options

**S1. Text only, with scaled line heights. S–M.**
- Scale the 10 text sizes and the 7 pinned line heights; keep 32 / 24 / 42 geometry.
- Inter at about 1.3× line height fits a 24 px tab pill up to roughly a 13–14 px title.
  So the clamp is about 1.25× before titles clip (arithmetic, not measured). Too small for
  "bigger when tired". Stopgap only.

**S2. `ChromeMetrics`: one scale for text and geometry (recommended). M.**
- See the design below. Every view function gains a metrics argument once.

**S3. iced's application `scale_factor`, with the terminal compensated. M, high risk.**
- Divide the terminal font by the factor so it keeps its device size.
- But `window_metrics`, hit-testing, screenshots and the host grid would all change unit,
  and Roost's device-pixel snapping reads only winit's factor
  (`iced_winit-0.14.0/src/lib.rs:1545-1550`). Not recommended.

**S4. Per-region keys, cmux style. S on top of S2.**
- A multiplier per region (sidebar, tab strip, palette) over the one scale. Defer until
  one scale proves not enough.

### `ChromeMetrics` design (S2)

**Where:** `crates/roost-iced/src/chrome.rs`, next to `ChromePalette`. The key is parsed
in `crates/roost-ui-model/src/config.rs`, beside `chrome-accent` (`config.rs:697-709`).

**Shape** (a sketch):

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeMetrics {
    pub scale: f32, // chrome_font_size / 13
    // text roles, fractional allowed: band_label, project_label, tab_title,
    // agent_name, agent_detail, menu_label, palette_input, modal_title, body, ...
    // lengths, snapped to device px: band_height, row_height, pill_height,
    // rename_line, agent_name_line, agent_detail_line, agent_row_pad_y, ...
}
impl ChromeMetrics {
    pub const BASE: Self = /* today's constants, verbatim */;
    pub fn new(chrome_font_size: f32, device_scale: f32) -> Self;
    // derived sums, built from snapped parts so they stay exact:
    pub fn agent_row_height(&self) -> f32;
    pub fn footer_band_height(&self) -> f32;
    pub fn band_pill_padding_y(&self) -> f32;
}
```

**Rules.**
- **The default is exactly today, at any device scale.** `new(13.0, d) == BASE` for every
  `d`, pinned by a unit test that keeps the existing assertions. This matters on
  fractional Linux scales, where snapping 42 at 1.25 would move today's layout.
- **Other sizes snap every length** to whole device px: `round(base × s × d) / d`. Sums are
  built from snapped parts, so the agent row stays exactly `2·pad + lines`, and the
  integral-height reason at `app.rs:6559-6564` holds at every size.
- **Text sizes scale without snapping.** cosmic-text handles fractional sizes, and
  `ElisionMemo` is already keyed by size bits.
- **Container widths scale too:** palette 660, confirm card 420, toast 520, menu 180–360,
  rename field 140, pill band 80–220, project column 140. The palette's agent columns
  elide by character columns (`palettes.rs:11-25`), so they need only their containers
  scaled.
- **Hoist the 36 literal `.size(N)` calls** into named roles first. It's a pure refactor
  and the first commit of the plan.

**Where the scale enters.**
- `App.metrics: ChromeMetrics` beside `App.chrome`, built by a `chrome_metrics_for` that
  mirrors `chrome_palette_for` (`app.rs:280-285`). Keep the two fields separate; bundling
  them would rewrite every `self.chrome` use for no gain.
- **Rebuilt** at bootstrap, in `window_rescaled` (`app.rs:5007-5031`, since the device
  scale feeds snapping), and on the chrome-size actions.
- **Free view functions** take `(&ChromePalette, &ChromeMetrics)`: `modal_card`,
  `modal_heading`, `modal_buttons`, `sidebar_band`, `band_text`, `project_label_budget`,
  `notice_strip`, `dialog_field`, `tab_band`, `palette_agent_segment`.
- **Custom widgets** carry metrics in their structs: `SidebarResizeGrip`
  (`sidebar_resize.rs:21-22`), `AgentsSplitGrip` floors (`agents_split.rs:28`, `:78`,
  `:577-578`), the agents panel height (`agents_panel.rs:80-94`).
- **Elsewhere:** `terminal_viewport` takes the band height; `pill_title_metrics`;
  context-menu `fitted` and `entry_height` (`context_menu.rs:470-510`).

**One chrome-size change, in one update:**
1. Rebuild the metrics.
2. Clear `pill_labels`, which isn't keyed by size.
3. Close an open context menu, which was sized at open.
4. Re-grid every tab through the batch font zoom uses (`palettes.rs:766-800`).
   `terminal_top` moved, so rows change and every PTY gets a resize. A failed batch rolls
   back, as font zoom does.
5. `advertise_host_grid` for host tabs, as `window_rescaled` does.
6. `config_writer.set("chrome-font-size", …)`.

**Sidebar width under chrome zoom.**
- The width is a workspace value (`workspace.rs:1111`), persisted in `state.json` as
  logical px. The UI reads it through one seam, `effective_sidebar_width`
  (`app.rs:1891`, `:5577`).
- **Recommended:** treat the stored width as **base units** and multiply by `s` at that
  seam, clamping to `[160·s, 400·s]` and to a share of the window. Reset is then exact,
  and a zoom step writes nothing to the workspace. A drag divides by `s` before
  `set_sidebar_width`.
- **Alternative:** rescale and persist the logical width on each step. Simpler to reason
  about in `state.json`, but each step is a workspace write and rounding drifts.
- `window_metrics.sidebar_width` keeps reporting the effective logical width.

**Minimum window:** keep 640×360. The scaled sidebar clamp must leave a usable terminal;
the exact share is open.

**DPI:** device px = logical × `s` × `d`, where only `d` comes from the OS. Folding the
Wayland `text-scaling-factor` into the default `chrome-font-size` is a later step (D6
below); it needs the portal reader the deferred light/dark plan would build.

**Tests.**
- **IPC:** an optional `chrome` block on `WindowMetricsResult` (`messages.rs:2112`),
  additive like every field there: `font_size`, `scale`, `band_height`, `row_height`,
  `pill_height`, `agent_row_height`, `footer_height`.
- **Harnesses:** replace the hard-coded 32 / 42 / 44 in the four files above with reads
  from that block. Land this first, so the default lanes prove nothing moved.
- **Unit invariants:** for `s` from 10/13 to 24/13 and `d` in {1, 1.25, 1.5, 2}, every
  height is whole in device px and derived sums hold.
- **e2e** (palette-driven, like `test_z_typography.py`): activate `chrome_size_increase`;
  assert `chrome.band_height` and `terminal_top` grew, rows shrank, and `config.conf` holds
  `chrome-font-size`; reset and assert byte-equal geometry.
- **Pixel lanes:** run once at 13 (unchanged) and once at a large size such as 18.

### Proposed product surface

- **Config:** `chrome-font-size = <n>`, in the chrome's logical px (macOS points). 13
  matches the macOS system font. Range 10–24. Warns and falls back like `chrome-accent`.
- **Actions:** `chrome_size_increase`, `chrome_size_decrease`, `chrome_size_reset`. Step
  1 px of base, about 7.7%. A parameterized `set_chrome_font_size:<n>` (Ghostty's pattern)
  serves looks and power users; `from_name` already parses prefixed forms such as
  `switch_project_<n>` (`keybind.rs:170-180`).
- **Palette rows:** "Increase / Decrease / Reset Chrome Size", beside the font rows.
- **macOS View menu:** three items after "Actual Size". Wording is open.
- **Default keys** (key question 4):
  - macOS `super+alt+equal`, `super+alt+minus`, `super+alt+0`;
  - Linux `ctrl+alt+equal`, `ctrl+alt+minus`, `ctrl+alt+0`.
  - Neither set is in the defaults (`keybind.rs:354-551`). On Linux, `alt+shift+=` is not
    free: on US layouts it is `alt+plus`, which `FontIncrease` holds.
  - macOS Accessibility Zoom uses Option-Command-= and Option-Command-minus when its
    keyboard shortcuts are on (off by default; from general knowledge, not checked here).
  - Linux `ctrl+alt` chords are also taken from terminal programs that would see them.
  - Zed leaves UI zoom unbound, so "palette and menu only" is a defensible default.
- **Live and persisted:** immediate, no relaunch; written at once, like `font-size`.

---

## 2. Chrome colors

### Today

- **The palette:** `ChromePalette` (`chrome.rs:187-273`), 42 `Color` fields, `Copy`. Its
  doc comment (`chrome.rs:180-185`) names it as the seam for `chrome-theme` and overrides.
- **The one constructor:** `ChromePalette::roost_dark(accent)` (`chrome.rs:288-351`).
  - The default accent `#007aff` (`DEFAULT_ACCENT`, `chrome.rs:171`) returns the
    hand-picked fills byte for byte.
  - Any other accent re-derives `accent`, `active_tab` (18% over `band`), `active_row`
    (50/50 with `#1f1f1f`) and `palette_match`, with integer 8-bit mixing (`mix`,
    `chrome.rs:361`).

| Group | Fields |
|---|---|
| Surfaces | `band` (sidebar header, footer **and** tab strip: one field), `list`, `divider`, `footer_chip` / `_hover` / `_pressed` |
| States | `active_row`, `active_tab`, `hover`, `active_agent`, `dragged_pill` (alpha 0.65) |
| Text | `text`, `muted_text`, `project_label_active`, `project_label_inactive` |
| Accent | `accent` |
| Palette overlay | `palette_surface`, `palette_selection`, `palette_hover`, `palette_placeholder`, `palette_match` |
| Host status | `host_dot_connected` / `_offline` / `_pending`, `host_rollup_text`, `host_fidelity_text`, `host_reconnect_text`, `host_banner_bg` / `_text` / `_border` / `_button_border`, `host_frame_scrim` (alpha 0.55) |
| Errors | `error_text`, `danger`, `danger_accent` |
| Agent lifecycle | `agent_working`, `agent_waiting`, `agent_finished`, `agent_failed`, `agent_inactive` (alpha 0.5) |
| Split handle | `split_handle`, `split_handle_hover` |

- **Built only through** `chrome_palette_for` (`app.rs:280-285`): at construction
  (`app.rs:4002`), after the macOS accent is read (`servicing.rs:3214`), and on
  `EngineFeed::AccentChanged` (`servicing.rs:2647`).
- **No chrome color literals leak out of `chrome.rs`,** except in tests.
- **Config:** `chrome-accent = system | #rrggbb` (`config.rs:160-182`, `:697-709`). On
  Linux, `system` is `#007aff`; the portal's `accent-color` isn't read.
- **macOS accent:** observed in `crates/roost-iced/src/macos/accent.rs:62-88`, and
  resolved **under darkAqua**, because the chrome is always dark (`accent.rs:3-5`).
- **The hidden second seam:** `fn theme(_app) -> Theme { Theme::Dark }`
  (`crates/roost-iced/src/main.rs:1062-1064`). It still styles the agents-panel and
  project-list scrollbars (`agents_panel.rs:241-247`; `app.rs:6945`, `:7031`), the
  agent-hooks toggler (`app.rs:720`), any text drawn with no color, and the window
  appearance iced pushes to winit (see the deferred section). Any light chrome theme has
  to change this first (A1).
- **Pixel lanes carry chrome colors:** `test_tab_strip_pixels.py:84-92` (`BAND`,
  `ACTIVE_TAB`, `DIVIDER_FILL`), and `test_agent_panel.py` and `test_real_input_mac.py`.
- **Mobile:** the Flutter client has its own `RoostColors`
  (`mobile/lib/app/theme.dart:6-27`), not `ChromePalette`.

### A1. Build iced's `Theme` from `ChromePalette` (prerequisite for light themes). S.

- `Theme::custom("roost", Palette { background: band, text, primary: accent, success,
  warning, danger })` in place of `Theme::Dark`.
- Scrollbars, the toggler and uncolored text follow the chrome.
- A light palette flips the window appearance by itself, through `is_dark`.
- **Risk:** iced's generated shades for the scrollbar and toggler differ from
  `Theme::Dark`'s. Check by eye, or style both from `chrome.rs` in the same change.

### Semantic inputs (recommended core)

| Input | Default in `roost-dark` | Feeds |
|---|---|---|
| `bar` | `#24292c` | `band`, footer, scrim base |
| `sidebar` | `#2d3235` | `list`, hover and `active_agent` mixes |
| `overlay` | `#2d2d33` | `palette_surface`, menus, cards |
| `text` | `#f2f2f2` | `text`, `project_label_active` |
| `muted` | `#a0a4b0` | `muted_text`, `palette_placeholder`, `split_handle_hover` |
| `accent` | system or `#007aff` | `accent`, `active_tab`, `active_row`, `palette_match`, `agent_working` |
| `attention` | `#e6b23a` | pending dot, fidelity text, host banner, `agent_waiting` |
| `danger` | `#e05252` | `agent_failed`, `error_text`, `danger`, `danger_accent` |
| `success` | `#3fca6b` | connected dot, the found chip |
| `divider` (optional) | `#1a1d1e` | `divider`; otherwise derived from `bar` |

- **Derivation:** the existing `mix` with a fixed step table (hover = 8% text over
  sidebar, selection = 15% text over overlay, active_tab = 18% accent over bar, as today).
  Under a light `bar` the steps darken rather than lighten; the test is `ColorRgb::is_light`
  (`crates/roost-vt/src/color.rs:31-33`).
- **The default stays exact:** `roost-dark` stays the literal at `chrome.rs:288-351`.
- **Contrast tests** over every bundled chrome theme, herdr style: active row against
  panel at least 1.05:1, text on the active row at least 3:1 (herdr
  `src/app/state.rs:1392-1450`).
- **Rejected alternative (C1):** a file over all 42 fields. Exact control, but each new
  field silently leaves user themes on dark values, and a light theme is hard to write by
  hand.

### Bundled chrome themes for lighting, tiredness and mood

| Name | Intent |
|---|---|
| `roost` (today's dark) | Default |
| `roost-dim` | Late night: lower contrast, darker bar, warmer muted text |
| `roost-contrast` | Tired eyes or glare: brighter text, stronger selection |
| `roost-light` | Bright rooms. Needs A1, and the macOS accent resolved under aqua |
| `auto` | Derived from the terminal theme's background and foreground |

- `auto` re-derives on every Select Theme preview and commit (`apply_theme_name`,
  `palettes.rs:2081`). Odd themes need contrast clamps.
- **Status hues stay fixed** across themes (recommended), so agent and host states stay
  recognizable; they follow `attention`, `danger` and `success`, not the ANSI colors.
- **All 26 bundled terminal themes are dark** (`crates/roost-ui-model/src/resources/themes/`).
  A `roost-light` chrome is far more useful with a few light terminal themes beside it:
  byte-identical MIT copies from the iterm2 set Ghostty ships.

### Keys and Select Chrome Theme

- `chrome-theme = roost | roost-dim | roost-contrast | roost-light | auto | <user file>`.
- `chrome-accent` stays an independent override (key question 7).
- A repeatable `chrome-color = <input>:#rrggbb[aa]` applies last, keyed by the **inputs**
  above, not the 42 fields. Unknown inputs warn, as `chrome-accent` does.
- User files: `~/.config/roost/chrome-themes/<name>`, `key = #hex`.
- **Select Chrome Theme…:** a drill-in frame, previewed by a new arm in
  `preview_selected_palette_item`, restored on dismiss, committed through
  `config_writer.set("chrome-theme", …)`. Cheaper than Select Theme: no tab batch, no
  host re-seed, just a palette swap and a redraw.

---

## 3. Opacity and translucency

### Today

- The window is opaque: `window_settings` (`main.rs:555-578`) never sets `transparent`.
- The palette's only alphas are `dragged_pill`, `host_frame_scrim` and `agent_inactive`.
- **The terminal paints an opaque default background.** The widget's first layer,
  `Band::Background`, covers the whole widget, padding included (`terminal_widget.rs:1019`),
  in the theme's `ColorRgb`, which has no alpha (`roost-vt/src/color.rs:7-11`).
- **The clear color** is the theme's `base().background_color`
  (`iced_core-0.14.0/src/theme.rs:291-335`), or whatever iced's `.style()` hook returns
  (`iced-0.14.0/src/application.rs:394-407`). Roost sets neither, so it's `Theme::Dark`'s.

### Verified constraints (iced 0.14, winit 0.30.13)

- **iced:** `window::Settings.transparent` and `.blur` are applied only at creation
  (`iced_winit-0.14.0/src/conversion.rs:52-53`). The runtime window module has no
  transparency or blur call (`iced_runtime-0.14.0/src/window.rs`), and exposes raw handles
  only (`trait Window: HasWindowHandle + HasDisplayHandle`, `:196`).
- **winit's `set_transparent`:** works on macOS (toggles `setOpaque` and clears the
  background color, `platform_impl/macos/window_delegate.rs:870-884`) and Wayland
  (`linux/wayland/window/state.rs:1122`). X11 can only set it at build time
  (`winit-0.30.13/src/window.rs:940-944`).
- **`iced_wgpu`** prefers a post- or pre-multiplied alpha mode whenever the surface offers
  one (`iced_wgpu-0.14.0/src/window/compositor.rs:123-136`). This box reports
  PreMultiplied.

| | Transparent window | Blur through winit |
|---|---|---|
| macOS | yes | yes, through a private CoreGraphics call, as Ghostty does |
| Wayland | yes | only KDE's `org_kde_kwin_blur` |
| X11 | needs a running compositor; the alpha visual is chosen at creation | no |

- **COSMIC** advertises `ext_background_effect_manager_v1` to ordinary clients, but not
  `org_kde_kwin_blur_manager` (verified from a live registry dump). So winit's blur flag
  is a no-op there.
- **The binding is present:** `wayland-protocols` 0.32.13 with `staging`
  (`crates/roost-iced/Cargo.toml:236-239`) includes `ext::background_effect`. The seam is
  `crates/roost-iced/src/linux/wayland.rs`, the #351 `xdg_activation_v1` hook.
- **Fallbacks stay opaque, never broken:** the software renderer, opaque-only GPUs, X11
  with no compositor.
- **Screenshots keep RGBA** (`crates/roost-iced/src/screenshot.rs:36-44`). Whether iced's
  offscreen capture carries real window alpha is **unverified**.

### Per-region translucency

| Region | Color source | Translucent? |
|---|---|---|
| Sidebar project list, agents panel | `list` | yes |
| Sidebar header, footer and tab strip | `band` (split it if they should differ) | yes |
| Terminal default background | the terminal theme, via `Band::Background` | yes, with an alpha seam |
| Cells with an explicit background (vim, tmux) | the app | opaque unless `background-opacity-cells` |
| Palette, menus, modals, banners, toasts | `palette_surface` and friends | no; keep opaque |

### O1. Launch-time transparent window, live values (minimum path). M.

- **Keys:**
  - `background-opacity`: Ghostty's name and meaning, the terminal's default background
    only. Needs `Band::Background` to paint with alpha.
  - `chrome-opacity`: applies to `band` and `list`; defaults to following
    `background-opacity`.
- **Launch:** create the window `transparent: true` when either key is below 1. Set the
  clear color's alpha through `.style()` or A1.
- **Live:** once created transparent, changing either value is just a color change. From 1
  to below 1 in a window created opaque gives a toast, "takes effect after relaunch".
  Ghostty needs a full restart for any change on macOS (`Config.zig:1004-1011`).
- **macOS:** force opaque in native full screen, as Ghostty does. Check the transparent
  title bar (`titlebar_transparent: true`, `main.rs:566`) doesn't turn see-through
  (unverified).
- **Report the effective state** (`transparent`, both opacities) in the `window_metrics`
  chrome block, so tests and users can see why nothing changed on a fallback renderer.
- **`toggle_opacity`** flips between the configured value and 1.0 for the session.
  Ghostty's `toggle_background_opacity` is the model (`Binding.zig:890-901`; macOS only
  there).

**O2. winit's launch-time `blur` flag (macOS, KDE). S.**

**O3. COSMIC blur through `ext-background-effect-v1`. M, optional.** Needs a blur region
kept in step with the translucent regions. GNOME has no blur.

**Window-level costs:** on macOS a transparent window's shadow follows its content;
antialiased text over a translucent background can fringe; the compositor pays a little
every frame.

---

## 4. Quick switching and looks

The goal is to change size, colors and opacity to match the moment, in one step.

**L0. Separate knobs. Free once phases 1–3 land.** Chrome size, Select Chrome Theme,
Select Theme, font size and `toggle_opacity` are each a keystroke or a palette row.

**L1. Layered profiles, Zed or WezTerm style. M–L.**
- A named overlay over the config. Zed's is a process global; WezTerm's
  `set_config_overrides` is a per-window, non-persisted overlay
  (`wezterm/docs/config/lua/window/set_config_overrides.md`).
- **Cons:** if a user zooms while in a look, which layer gets the write? And Roost has no
  config hot reload to re-evaluate layers.

**L2. Looks as macros (recommended). S–M.**
- `~/.config/roost/looks/<name>` holds **appearance-only keys**: `chrome-font-size`,
  `chrome-theme`, `theme`, `font-size`, `background-opacity`, `chrome-opacity`. Other keys
  warn and are ignored.
- **Applying a look** runs each key's existing apply path and writes each key through
  `config_writer`. No `look =` key, no layer. A later tweak just edits the live keys.
- **Select Look…** previews by applying in memory, and restores on dismiss, reusing the
  theme, font and chrome-theme preview arms. The theme part of the preview is the slowest
  (a tab batch).
- **`apply_look:<name>`** binds a look to a key.
- **Bundled examples** such as "Night" and "Bright room" are optional.
- It loses only "which look am I in", which can be shown by matching the current keys.

---

## Deferred: following the system light/dark setting (#164)

Not a priority now. These verified findings stay for the later plan.

**Why iced's own support doesn't help as Roost is built.**
- iced pushes the window mode at creation and whenever the theme's mode changes
  (`iced_winit-0.14.0/src/lib.rs:670-672`, `window/state.rs:218-258`). The mode is the
  palette's `is_dark`, so `Theme::custom` with a light background gives a light window by
  itself.
- `Theme::Dark` means darkAqua on macOS, `_GTK_THEME_VARIANT=dark` on X11
  (`winit-0.30.13/src/platform_impl/linux/x11/window.rs:631-652`), and dark CSD on Wayland
  (COSMIC uses server-side decorations, so this matters mainly on GNOME).
- **macOS:** winit's appearance KVO returns early when the window has its own appearance
  (`platform_impl/macos/window_delegate.rs:461-465`), so `iced::system::theme_changes()`
  never fires while Roost forces dark. `NSApp.effectiveAppearance` still reports the
  system value, which is what Ghostty observes (`macos/Sources/App/AppDelegate.swift:329-344`).
  Returning `None` from the theme function would unforce it, but give up a chrome-matched
  title bar.
- **Linux:** winit's `system_theme()` is `None` (`platform_impl/linux/mod.rs:909-911`).
  iced reads it only under `linux-theme-detection` (mundy, plus a blocking startup probe of
  up to 200 ms, `iced_winit-0.14.0/src/lib.rs:524-553`), which Roost doesn't enable
  (`crates/roost-iced/Cargo.toml:48-58`).

**This COSMIC box, through the portal:** `color-scheme` 1 (prefer dark), `accent-color`
a teal `(0.388, 0.816, 0.875)`, `contrast` 0, `text-scaling-factor` 1.0, font
`Open Sans 10.5`.

**Terminal themes.**
- `load_bundled` (`crates/roost-ui-model/src/theme.rs:75-86`) falls back to `roost-dark`
  **silently** for an unknown name. So `theme = light:X,dark:Y` renders `roost-dark`
  today.
- No user theme directory, although the module doc (`theme.rs:9-12`) promises
  `~/.config/roost/themes/<name>`.
- Mode 2031 sends `CSI ?997;1n` / `?997;2n` from the terminal theme's background, not the
  system (`app/terminal_tab.rs:1776-1784`).
- DSR `?996` isn't answered. libghostty at the pinned SHA has
  `GHOSTTY_TERMINAL_OPT_COLOR_SCHEME`; `roost-vt` doesn't wire it (#209).
- `session.set_theme` is one supervisor-wide seed, **last-writer-wins by design**
  (`crates/roost-engine/src/ipc.rs:3026-3040`; `docs/reference/ipc.md:3833-3857`). Two
  clients in different appearances would keep re-seeding each other.

**The later plan, condensed.**
- **B1. Appearance observers. S–M.** macOS: KVO on `NSApp.effectiveAppearance`, shaped like
  `accent.rs`. Linux: Roost's own zbus `org.freedesktop.portal.Settings` reader
  (`ReadOne` plus `SettingChanged`) on the session bus `notifications.rs` already uses;
  WezTerm's `window/src/os/xdg_desktop_portal.rs` is the model. Plus a test-mode op that
  injects an appearance.
- **B2. Theme pairs. S–M.** Ghostty's `theme = light:X,dark:Y`, or #164's
  `theme-light` / `theme-dark`. A flip takes the commit path (re-seed hosts) without
  writing `config.conf`.
- **B3. Light content. S–M.** Light terminal themes (also a quick win now) and
  `roost-light` (phase 2 here).
- **B4. `window-theme = auto|system|light|dark`. S** on A1, B1 and B2.
- **B5. DSR `?996` (#209). S–M.**
- **D6. Wayland `text-scaling-factor`** folded into the default `chrome-font-size`. S on
  phase 1 plus B1.

**Groundwork that pays now:** A1 (needed by `roost-light`), and the macOS accent resolved
under the chrome's own appearance (S). A pure `(config, mode) → names` resolver with mode
fixed to dark is optional.

---

## Roadmap (one plan per phase)

Complexity: S is a few days, M about a week-sized plan, L several weeks.

| Phase | Contents | Complexity | Closes or advances |
|---|---|---|---|
| **0. Seams** (can fold into 1) | Hoist the 36 literal sizes into named roles; the `window_metrics` chrome block; harnesses read geometry from it; A1 | S | Prep for all phases |
| **1. Chrome size** | `ChromeMetrics` (default = today, snapped scaling); `chrome-font-size`; three actions, palette rows, View menu, optional default keys; live re-grid; persisted; sidebar-width rule; unit invariants; palette-driven e2e; pixel lane at a second size | M (wide, low design risk) | New issue (none exists) |
| **2. Chrome colors** | Semantic inputs and derivation; `roost-dim`, `roost-contrast`, `roost-light`, `auto`; `chrome-theme`, `chrome-color`, user files; Select Chrome Theme…; accent under aqua; contrast tests; a few light terminal themes | M | #584 |
| **3. Opacity** | `background-opacity`, `chrome-opacity`; launch-time transparent with live values; terminal background alpha seam; `toggle_opacity`; macOS full-screen opaque; effective state in IPC; winit blur flag | M (+ M optional for COSMIC blur) | New issue |
| **4. Looks** | L2 macro looks, Select Look…, `apply_look:<name>`, optional bundled looks | S–M | New issue |
| **Later** | Light/dark following (B1–B5), D6 Wayland text scaling, per-region multipliers (S4) | M–L in total | #164, #209 |

- **Phases 2 and 3 can swap.** Phase 3 depends only on phase 0 (the clear color and the IPC
  block), not on phase 2: opacity scales the alpha of `band` and `list` whatever the theme.
- **Phase 4 is most useful after 1–3,** but can ship after phase 1 with only size and the
  terminal theme in a look.
- **Alternative:** phases 0 and 1 as one plan; phases 2 and 3 as one L plan. Fewer review
  rounds, longer branch.

---

## Quick wins (each S)

**Docs (verified stale).** The `themes.md` and zoom fixes overlap item 4 of
[`backlog-triage-2026-11.md`](backlog-triage-2026-11.md)'s Plan A (v0.1.1); whichever
lands first takes them.
- `docs/reference/themes.md:10` and `:80` say themes aren't hot-reloaded. Select Theme
  previews and commits live (`palettes.rs:2081-2127`).
- `themes.md:10` says an unknown name logs a warning. None is logged.
- `themes.md:43` links Ghostty's `src/config/themes`, which no longer exists.
- `themes.md:59` says `cursor-text` is ignored. It's honored (`theme.rs:31-36`).
- `docs/reference/fonts.md:6`, `:23`, `:94-95` say zoom is per tab and family changes need
  a relaunch. Zoom is app-wide (`palettes.rs:766-780`); the font picker previews live.
- The `FontIncrease` doc comment still says "Per-tab font adjusters"
  (`keybind.rs:89-90`).
- #164's title still starts "mac:".

**Code:**
- Hoist the 36 literal `.size(N)` calls into named roles in `chrome.rs` (phase 0's first
  commit).
- The `window_metrics` chrome block, and harnesses that read it.
- A1 on its own: with today's palette it should look the same, apart from the scrollbar
  and toggler shades to check.
- Warn on an unknown `theme` name at load, as `chrome-accent` does.
- Bundle a few light terminal themes (byte-identical MIT copies). Mode 2031 already
  reports `light` for them.
- A contrast unit test over `roost_dark` with sample accents.
- #162 wheel zoom: terminal only, independent of all of this.

---

## Key questions for the planning session

1. **Knob name and unit:** `chrome-font-size` in px with base 13 (recommended, Zed-like),
   `chrome-scale` as a percentage (cmux, VS Code), or `ui-font-size`?
2. **Linux default:** keep 13, or 14 to match COSMIC and GNOME? It changes the default
   Linux look and the pixel lanes. Recommendation: ship 13 in phase 1, decide 14 as its own
   small change.
3. **Sidebar width under chrome zoom:** store base units and scale at render (recommended),
   or rescale and persist the logical width on each step?
4. **Default keys:** bind macOS `super+alt+=/-/0` and Linux `ctrl+alt+=/-/0`, or leave
   chrome zoom on the palette and menu only, as Zed does?
5. **Step and range:** 1 px of base (about 7.7%, Zed) or 10% (cmux)? Range 10–24?
6. **Per-region sizes:** is one chrome scale enough, or should the sidebar and agents text
   grow more than the tab strip (cmux's two keys)?
7. **Accent precedence:** does a chrome theme set the accent, or does `chrome-accent`
   (including `system`) always win? Should Linux `system` follow the portal's
   `accent-color`?
8. **Status hues:** fixed across chrome themes (recommended), or from the terminal's ANSI
   colors under `auto`?
9. **Which bundled chrome themes:** is `roost-dim` plus `roost-contrast` enough for phase 2,
   with `roost-light` waiting for light terminal themes?
10. **One opacity or two:** should `chrome-opacity` follow `background-opacity` by default
    (recommended), or default to 1?
11. **Turning transparency on:** relaunch with a toast (recommended), or always create the
    window transparent on macOS and Wayland and accept the compositor cost?
12. **Looks:** macro looks (L2, recommended) or layered profiles (L1)? Do looks include the
    terminal `font-size` and `theme`? Bundle examples, or only document them?
13. **Ops or UI-local:** should chrome size, theme and opacity be workspace ops (the north
    star), or UI-local with IPC readback as font size and theme are today?
    Recommendation: UI-local, plus palette rows and the `window_metrics` chrome block.
14. **Should `band` split** into sidebar and tab-strip fields, now that the agents panel
    also sits on it and opacity may want them to differ?
15. **Mobile:** should the semantic chrome inputs live in `roost-ui-model`, so the Flutter
    client can share them later?

---

## Corrections (2026-11-08)

- **Order changed.** The 2026-11-08 morning revision put light/dark first. Charlie put
  chrome size first and deferred light/dark, so the roadmap now leads with size.
- **Zed line numbers:** `assets/settings/default.json:41` is `buffer_font_size` (15) and
  `:71` is `ui_font_size` (16). The research draft had them swapped.
- **Opacity doesn't strictly need A1.** iced's `.style()` hook sets the clear color
  independently of the theme. But the terminal's `Band::Background` paints an opaque color
  over the whole terminal, so `background-opacity` needs an alpha seam in the terminal
  widget, which the research draft missed.
- **Pinned line heights are 7 sites,** not "four places": five in `agents_panel.rs`, two
  rename editors.
- **`chrome::` references outside `chrome.rs` are 212** (122 `app.rs`, 74 `agents_panel.rs`,
  12 `agents_split.rs`, the rest in `servicing.rs`, `terminal_tab.rs`,
  `context_menu.rs`), not about 200 with 117 / 69 / 10.
- **`HOST_ROLLUP_SIZE` is `chrome.rs:100`,** not `:99`.
- **Sidebar width is workspace-owned** (`workspace.rs:1111`), so "rescale and persist" is a
  workspace write per step. This note now recommends base units scaled at render instead.
- **Still true from the earlier note:** 56 `.size()` sites (36 literal, 20 named), 42
  palette fields, the forced `Theme::Dark`, the COSMIC blur facts.

---

## Verified and inferred

**Verified** by reading code at `68231b9b`, vendored crate source, reference-repo source and
docs, or a command on this COSMIC box:
- every Roost file:line above, including the call-site counts;
- the iced and winit transparency, blur, clear-color, `.style()` and appearance facts;
- the portal values and COSMIC's advertised globals;
- Zed's settings, actions, rem tie and profiles comment;
- Ghostty's font and opacity actions and the `background-opacity` restart note;
- cmux's documented size keys and ranges;
- the stale docs lines.

**Inferred,** not executed:
- the 1.25× clamp for text-only scaling;
- that no chrome text falls to iced's default 16 px (rough scan, not a parse);
- that macOS gives a custom-drawn app no system text size;
- that GNOME Large Text on X11 enlarges all of Roost;
- the macOS Accessibility Zoom shortcut overlap;
- macOS title-bar behavior on a transparent window;
- whether iced's offscreen screenshot carries window alpha;
- that Zed's active profile doesn't persist;
- VS Code details (web sources only).

---

## Related notes

- [`text-rendering.md`](text-rendering.md): chrome typography quality (face, weights,
  hinting, rendering), terminal text, #162.
- [`swift-retirement.md`](swift-retirement.md): sidebar translucency as a former Swift
  feature. Native vibrancy (`NSVisualEffectView`) was ruled out on 2026-10-03 as macOS only.
- [`mobile-client.md`](mobile-client.md): the separate mobile palette.

---

## References

| Reference | Path @ commit | License | How it was used |
|---|---|---|---|
| Zed | `/tmp/claude-1000/-home-charliek-projects-roost/722ee52e-3f4c-49dd-ac65-84b1c809c9a5/scratchpad/refs/zed` @ `d2a4fb6` (sparse) | Repo Apache-2.0 / GPL-3.0; the `theme`, `theme_settings`, `settings` and `zed` crates are GPL | Behavior only; no code quoted or paraphrased |
| Ghostty | `/home/charliek/projects/thirdparty/ghostty` @ `eb895f612` | MIT | `Binding.zig`, `Config.zig`, macOS appearance |
| herdr | `/home/charliek/projects/thirdparty/herdr` @ `ee5bfb5` | Apache-2.0 | Theme model, contrast tests |
| WezTerm | `/home/charliek/projects/thirdparty/wezterm` @ `458a100` | MIT | Chrome font keys, portal reader, `set_config_overrides` |
| cmux | `/home/charliek/projects/thirdparty/cmux` @ `a79cbde73ba` | GPL-3.0-or-later | Docs only, design level |
| cosmic-term | `/home/charliek/projects/thirdparty/cosmic-term` @ `9129277` | GPL-3.0 | Behavior only, from its settings surface |
| iced 0.14.0 family, winit 0.30.13, wayland-protocols 0.32.13 | `~/.cargo/registry/src/index.crates.io-*/` | MIT / Apache-2.0 | Transparency, blur, clear color, appearance, runtime window API |
| mundy 0.2.3 | crates.io metadata only | Apache-2.0 | License only |
| VS Code | [Real Python: Setting the Zoom Level](https://realpython.com/lessons/setting-vscode-zoom/), [bobbyhadz: zoom in/out in VS Code](https://bobbyhadz.com/blog/zoom-in-out-vscode) | n/a | Web sources only |

Not used: zellij, alacritty, tmux and bonsplit. None is a GUI-chrome reference for this
topic.
