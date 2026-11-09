# Text rendering in iced: terminal and chrome (discovery)

Status: **discovery**, an options list for tuning, not a commitment.
Written 2026-09-27, revised 2026-11-08. Chrome text (the left panel's
Projects list and Agents panel, the tab strip, the palettes, menus, modals
and toasts) was added 2026-11-08 in the afternoon. File and line references in
the 2026-11-08 sections are as of `main` @ `68231b9b`. The original sections
below keep their `474e6b5` references and are marked as such.
Companion to [`swift-retirement.md`](swift-retirement.md) and
[`chrome-themes.md`](chrome-themes.md). That note owns chrome **size, scale,
colors and opacity**. This one owns how chrome text is drawn: face, weights,
shaping, truncation, contrast of today's palette, and rasterization.

**Update 2026-10-02:** plan 073 (the first gap release, see
[`swift-retirement.md`](swift-retirement.md)) shipped all of Tier 0 and the
cursor, selection and cell-metrics items of Tier 1. They are marked **Shipped**
below, with [findings](#findings-from-plan-073). The "today" tables
describe the code at `474e6b5`, before that work.

**Update 2026-10-03:** plan 074 shipped the theme's `cursor-text` color (see
Tier 1). #590, #591 and #592 stay open.

**Update 2026-11-08:** v0.1.0 shipped and the Swift app is retired, so iced
is the only desktop renderer and Swift parity no longer constrains
anything. Since the last update:

- **Shipped: Kitty graphics** are drawn in the terminal widget (plan 088, PR #677,
  `fb65a92a`), without a Tier 2 renderer. Unicode placeholder cells are no
  longer drawn as text (#680, `f146bf3e`). Animation is deferred (#676).
- **Shipped: cell pixel size** is now reported in device pixels to the PTY and to
  size queries (`53200e81`), so images show at native size.
- **`Style` carries decorations** (faint, underline, strikethrough,
  invisible) since `bd02ab61` (2026-11-03), but only the mobile frame
  encoder reads them. **The desktop draws none of them.** This contradicts
  the side-by-side table below, which said dim renders; it never did. See
  [Corrections](#corrections-2026-11-08).
- Nothing else in this note shipped. #585, #590, #591 and #592 are open.

**Update 2026-11-08 (afternoon):** chrome text added, and the terminal list
re-checked. The terminal ranked list (items 1–11) still holds, and three
items were added (12–14). #585, #590, #591, #592, #162 and #309 are still
open (checked with `gh`).

The new material is:
- [Where to start when you come back](#where-to-start-when-you-come-back): a
  suggested first plan;
- terminal: [Today (2026-11-08)](#today-2026-11-08) and
  [Remaining polish](#remaining-polish-ranked-2026-11-08);
- chrome: [Chrome text (2026-11-08)](#chrome-text-2026-11-08);
- then corrections, key questions, sequencing and references.

iced's terminal text is good. The question here is which knobs exist to
make it better, especially on macOS once the Core Text-based Swift app is
gone, and what each costs.

---

## Summary

**2026-11-08 (afternoon), chrome text:**

- **Chrome text is in good shape.** The chrome draws bundled Inter 4.1 on both
  OSes. On Linux it goes through cosmic-text and swash, the same path COSMIC's
  own apps use (*inferred* for libcosmic's iced fork, which wasn't checked
  out). So next to COSMIC only the face (Inter vs Open Sans) and the body size
  (13 vs 14 px) differ. Nothing was captured on macOS this pass.
- **Two visible bugs, both S:**
  - **Agents palette overflow.** The project column is cut to 24 display
    columns, then drawn in a 140 px box that doesn't clip. 21 columns of 14 px
    SemiBold Inter ("mobile-client-android") is already 148 px. Seen: the
    project runs into the tab name after it.
  - **Palette highlight weight bleed.** A fuzzy match that starts or ends
    inside an ASCII word gives the whole word one weight. Seen: the unmatched
    "s" of "tab**s**" draws SemiBold. This is a cosmic-text 0.15 bug, fixed in
    0.16. iced 0.14.0, still the newest iced on crates.io, pins 0.15.
- **Contrast: the main text passes; several small secondary texts don't.**
  - Below WCAG AA's 4.5:1: the project row's agent count (2.74; 1.67 on the
    selected row), the host band rollup (3.11), "Failed" as text (3.40), the
    command palette's selected-row subtitle (3.39) and match (3.45), and the
    agents palette's "Idle" (1.81).
  - On macOS the selected row follows the system accent. Under a yellow
    accent the count is about 1.0:1 (accent hexes *from memory*).
  - Each fix is a palette value. Separate text roles keep the dot colors
    that `test_sidebar_pixels.py` asserts.
- **Kerning depends on the code path.**
  - Unannotated `text()` is iced's `Auto`, which is Basic shaping for ASCII:
    no kerning and no `calt`.
  - Labels pinned to `Advanced` get kerning and Inter's `calt`, which turns
    `->` into `→` in tab titles.
  - Elision always measures with Advanced, so a label drawn Basic can come
    out wider than its measured budget.
- **Ten sizes, three weights, no tracking, no tabular figures.** iced 0.14
  passes no font features and no letter spacing. A role scale is the
  groundwork for chrome font size ([`chrome-themes.md`](chrome-themes.md)
  phases 0 and 1), which Charlie wants first.
- **Inter stays the right default.** A system-font option, a tabular face and
  a patched `calt` are listed below, all low priority.
- **Where to start:** one S–M plan. Fix the bugs and contrast (C1, C2, C3,
  C8) and add a label constructor with a role scale (C4, C5), which is also
  `chrome-themes.md` phase 0. See
  [Where to start](#where-to-start-when-you-come-back).

**2026-11-08, terminal text:**

- **One real gap remains, and it is not about glyph quality.** The desktop
  drops every SGR text decoration: dim (SGR 2), all underline styles (SGR
  4, including curly) and the underline color (SGR 58), strikethrough
  (SGR 9), overline (SGR 53) and invisible (SGR 8). Claude Code itself
  prints its `⎿` result rows in dim, so this shows in every agent session.
  It needs no new renderer. Effort S–M.
- **Second: agent-TUI symbols come from mismatched fallback faces.** The
  bundled JetBrains Mono lacks four of Claude Code's six spinner glyphs
  (✢ ✳ ✻ ✽), its `⎿`, and its macOS bullet `⏺`. So the spinner switches
  faces from frame to frame. The fixes are a fallback list (#592) and
  per-face sizing, both possible inside today's `fill_text` path.
- **The rest is real but low-value polish:**
  - antialiased curves and diagonals (#590), done with a tiny-skia image
    cache rather than Tier 2;
  - powerline and braille sprites (#591);
  - bundling a Nerd Font symbols face;
  - Linux cell metrics and `adjust-*` options (#585);
  - a few config knobs.
- **Tier 2 (Roost's own glyph atlas and shader) is still not needed** for
  anything above. It is the route for ligatures, synthetic bold,
  `font-thicken`, `linear-corrected` blending and Display P3, none of which
  has a user asking for it. Its trigger stays performance (#309).

**2026-09-27 (original):**

- **Four settings explain most of the macOS difference:** point size is
  converted at 96/72, glyphs are hinted, colors blend in linear light, and
  cells are rounded down with a fixed 1.2 line height.
- **The cheapest improvements are small:** a macOS size factor, turning
  hinting off on macOS, and iced's `web-colors` feature. They can be tried
  together and judged with the capture recipe below.
- **Ghostty is the reference for native-looking Mac text.** cosmic-term
  offers a better drawing structure and access to hidden cosmic-text
  settings, but nothing Mac-specific. Ghostling contributes one idea:
  Kitty inline images through libghostty.
- **The big fork in the road is a terminal renderer of Roost's own** (a
  glyph atlas and a GPU shader). It unlocks the Ghostty techniques and
  lowers per-frame cost on both platforms, but it is one to two plans of
  work.

---

## Where to start when you come back

Charlie's position (2026-11-08): fonts generally look good; this is for
later. Chrome **size** comes first, and lives in
[`chrome-themes.md`](chrome-themes.md). The plan below is the text-quality
work that size builds on, so it is the natural first step whichever note
you start from.

### Suggested first plan: chrome text fixes and the label seam (S–M)

One plan, three commits. Everything is iced only and covers macOS and Linux.

1. **Chrome text bugs (S).**
   - C1: elide the agents palette columns in pixels, not display columns, and
     elide generic palette titles and subtitles against the row.
   - C2: drop the SemiBold on palette match spans; the blue carries the match.
   - C8: strip a leading marker glyph from tab pill titles, with the helper
     the Agents panel already uses.
   - Issues: none exist for C1, C2 or C8. File them first, or one issue for
     all three.
2. **Contrast (S).** C3: text roles and lifted values for the five failing
   roles, and a selected-row color for the project count. Add the contrast
   unit test over `roost_dark` with sample accents that `chrome-themes.md`
   lists as a quick win. The asserted dot colors stay as they are.
3. **The label seam (M).** C4 and C5: a `chrome::label(text, role)`
   constructor that sets font, size, line height, shaping and wrapping in one
   place; a role table that absorbs the ten sizes; every `.size()` site
   migrated. This is `chrome-themes.md` phase 0's "hoist the 36 literals"
   commit, so do it once, here or there.

**Out of scope:** `chrome-font-size` and `ChromeMetrics` themselves
(`chrome-themes.md` phase 1), a patched or tabular Inter (C4b, C6), a family
option (C11), and every terminal item. The next terminal plan is still SGR
decorations (item 1), and it is independent.

**Decide first:** chrome key questions 2 (C2's fix), 3 (C3's colors), 5
(merge the half-pixel sizes, which changes two labels by half a pixel) and 8
(C8 for pills).

**Gate and evidence:**
- Unit tests for the pixel budgets (C1) and contrast (C3).
- `test_view_perf.py` for the constructor's shaping cost.
- The default pixel lanes must stay green unchanged: `test_tab_strip_pixels.py`
  and `test_sidebar_pixels.py` pin colors C3 doesn't touch.
- Before and after captures: the Linux recipe in this note's chrome section,
  and a 2x `roostctl screenshot` on the mini through the `mac-test` skill.

**Re-check before planning** (each takes minutes):
- `cargo search iced`: an iced on cosmic-text 0.16 or later fixes C2 for free
  and may expose font features and letter spacing (C4, C6, C9).
- `grep -rn '\.size(' crates/roost-iced/src` against the inventory below.
- Re-run the contrast numbers if `ChromePalette::roost_dark` changed.

**Then:** `chrome-themes.md` phase 1 (`ChromeMetrics` and `chrome-font-size`)
lands on the role table, and that is Charlie's "bigger font when tired".

---

## Today (2026-11-08)

Everything here was read in the code at `68231b9b` unless marked
*inferred*.

### Pipeline and metrics (`crates/roost-iced/src/terminal_widget.rs`)

| What | Today | Where |
|---|---|---|
| Point size | 1 pt = 1 px on macOS, 96/72 on Linux | `:35-39` |
| Cell rule | macOS `FontDerived`: the ceilings of the "M" advance and of ascent − descent + leading, glyph top 0. Linux `Legacy`: the floors of the advance and of 1.2 × size, glyph top 1. | `CellRule` `:88-131`, `PLATFORM` `:99`, line height `:40` |
| Face metrics | From the face the shaper actually used for "M" | `shaped_face_metrics` `:135` |
| Glyph draw | One `fill_text` per inked cell, `Shaping::Auto`, unbounded width (a glyph may overflow its cell; nothing clips it per cell) | `cell_text` `:351`, call `:1466` |
| Family choice | Wide clusters use `Family::SansSerif`. Text-presentation symbols without VS16 use "STIX Two Math" (macOS) or "DejaVu Sans" (Linux). Everything else uses the configured family. Bold and italic request real faces; nothing is synthesized. | `draw_font` `:319`, `is_default_text_presentation` `:250`, `monochrome_fallback_family` `:308` |
| Inked cells | Blank cells and Kitty placeholders are skipped | `inked_cells` `:696`, `KITTY_PLACEHOLDER` `:694` |
| Underlines drawn | Only the hovered-link underline (1 px at the cell bottom) and the underline cursor (2 px) | `draw_text` `:1448-1552` |
| Sprites | U+2500–257F (box) and U+2580–259F (blocks). Every primitive becomes rects rounded to **logical** px. Arcs and diagonals are stair-stepped at alpha 1. `SpriteGeometry::antialias` is ignored. | `crates/roost-ui-model/src/sprite.rs:102`, `draw_sprite` `:1809` |
| Stroke width | `round(h / 14)`, at least 1, in logical px | `sprite.rs:464` |
| Device-px snapping | Exists for Kitty images only | `GridGeometry::snap_span` `:992`, `image_rects` `:1076` |
| Draw order | Bands share a layer unless an image sits between them. Within a layer, iced_wgpu draws quads, meshes, images, then text. | `draw` `:1721`; iced_wgpu 0.14 `lib.rs` render loop |

### Decorations are read, then dropped

- `crates/roost-vt/src/render_state.rs:100` `Style` holds `bold, italic,
  inverse, faint, underline: bool, strikethrough, invisible`.
- `read_cells_style` (`:1023`) narrows libghostty's `int underline` (single,
  double, curly, dotted, dashed) to a bool. It also drops `overline`,
  `blink` and `underline_color`. All of them are in
  `third_party/ghostty/out/include/ghostty/vt/style.h:94-108`.
- `crates/roost-vt/src/resolve.rs:16` `DrawCell` has no faint, underline,
  strikethrough, overline or invisible field.
  - `RenderedRow::build` (`:90`) keeps a cell only when it has text, a
    background or inverse, so an underlined blank cell would also be lost.
  - `inked_cells` filters blanks a second time.
- The only reader of the decoration fields is the mobile encoder
  (`crates/roost-mobile/src/frame.rs:207-221`, whose `drawless` rule at
  `:214` keeps decorated blanks). A grep of the desktop crates finds no
  other use.
- **libghostty does not pre-apply faint.** Ghostty dims in its renderer
  (`../thirdparty/ghostty/src/renderer/generic.zig:3193`,
  `faint-opacity`, default 0.5), so the resolved foreground Roost reads is
  undimmed.
- **libghostty gives no resolved underline color.** The row-cells data kinds
  (`render.h:1444-1506`) include fg and bg but no underline color. A
  palette-indexed SGR 58 needs the 256-color palette. `Colors`
  (`render_state.rs:88`) drops that palette, although the C struct
  carries it (`:319`).
- `tab.dump_resolved` (`crates/roost-engine/src/ipc.rs:250`
  `ResolvedCellData`) reports bold, italic and inverse only, so no test can
  assert on decorations today.

### Fonts, coverage and fallback

- **What ships:** `crates/roost-iced/src/fonts.rs:24` loads JetBrains Mono
  v2.304 (four static faces) and maps generic Monospace to it.
- **Config:** only `font-family` and `font-size` exist
  (`crates/roost-ui-model/src/config.rs:568-569`). `docs/reference/fonts.md`
  § Limitations says so. It doesn't mention that decorations aren't drawn.
- **Coverage of the bundled Regular face**, from `fc-query`'s charset:

| Covered | Not covered |
|---|---|
| U+2500–25A1 box and blocks; ✶ (U+2736) · ● ✓ ✗ ❯; powerline U+E0A0–E0A2 and U+E0B0–E0B3 | ⏺ U+23FA, ⎿ U+23BF; ✢ ✳ ✻ ✽; ✔ ✘ ↳ ★ ⏵ ⏸ ※; all braille U+2800–28FF; powerline U+E0B4–E0BF; all Nerd Font PUA (for example U+E718, U+F121); Legacy Computing U+1FB00+ |

- **What Claude Code prints.** Read from the installed binary
  (`~/.local/share/claude/versions/2.1.319`):
  - Spinner frames are `· ✢ ✳ ✶ ✻ ✽` on macOS and `· ✢ * ✶ ✻ ✽`
    elsewhere. With `TERM=xterm-ghostty` they are `· ✢ ✳ ✶ ✻ ✻`.
  - The bullet is `⏺` on macOS and `●` elsewhere.
  - Result rows start with `⎿` and are printed with `dimColor` (SGR 2).

  So on every OS, four or five of the six spinner frames come from a
  fallback face, and on macOS the bullet does too.
- **cosmic-text 0.15 fallback.** After the requested family it tries
  script lists, then a common list:
  - macOS: `.SF NS`, Menlo, Apple Color Emoji, Geneva, Arial Unicode MS.
    That puts the proportional UI font first.
  - Linux: Noto Sans, DejaVu Sans, FreeSans, Noto Sans Mono, DejaVu Sans
    Mono, FreeMono, Noto Sans Symbols, Noto Sans Symbols2, Noto Color
    Emoji.

  Sources: `src/font/fallback/{macos,unix}.rs` and `fallback/mod.rs`.
- **The Linux `⏺` route names a font without the glyph.** On the Pop!_OS
  box, `fc-list ':charset=23fa'` lists FreeMono, FreeSans, FreeSerif, Noto
  Color Emoji, Noto Sans Symbols2 and the Unifonts. It does not list
  DejaVu Sans.
  - *Inferred from the cascade order:* the route falls through to FreeSans,
    which is monochrome, so on this box it changes nothing. The comment at
    `:303-307` says it was never checked on Linux.
  - Claude Code doesn't print `⏺` on Linux, so the practical impact there is
    small.
- **The custom-fallback seam exists.** cosmic-text has
  `FontSystem::new_with_locale_and_db_and_fallback`
  (`cosmic-text-0.15.0/src/font/system.rs:166`) and
  `into_locale_and_db` (`:250`). iced exposes `font_system().write().raw()`
  (`iced_graphics-0.14.0/src/text.rs:116-150`). That is #592's mechanism.
- **`monospace_fallback` (a cosmic-text feature) is off.** Enabling it makes
  a monospace request prefer monospace fallback faces.
  - Its rescale-to-cell part needs `Buffer::set_monospace_width`
    (`shape.rs:1597`), which iced's `fill_text` never calls.
  - So through today's path it changes only which face is picked.
- **What `fill_text` can't pass:** iced's `to_attributes` (`text.rs:259`)
  passes family, weight, stretch and style only, so font features and the
  `FAKE_ITALIC` / `DISABLE_HINTING` cache-key flags need the raw path
  (`text::Raw`, `:180`).
  - Neither cosmic-text 0.15 nor 0.19 can synthesize bold.
  - swash can embolden outlines (`third_party/swash/src/scale/outline.rs:99`),
    but only a renderer of Roost's own could use that.
- **What `fill_text` does allow:** a size and position per call, so
  per-face scaling and baseline correction are possible without the raw
  path. `shaped_face_metrics` already finds the resolved face of a shaped
  string.

### Measured metrics (font tables; on-screen effect *inferred*)

| Font | upm | ascent | descent | notes |
|---|---|---|---|---|
| JetBrains Mono | 1000 | 1020 | −300 | advance 600; real descender ink (g, y, p) reaches −180; x-height 550, cap 730 |
| Noto Color Emoji (Linux) | 2048 | 1900 | −500 | advanceWidthMax 2550 |
| Noto Sans CJK | 1000 | 1160 | −288 | ideograph advance 1000 |

At Linux 13pt (17.33 px, cell 10×20, line box 20.8, glyph top 1), with
cosmic-text's centering rule (`buffer.rs:135-137`):

| Glyph | Result |
|---|---|
| JetBrains Mono baseline | about 17.6 px below the cell top. Descender ink reaches about 20.8, so **under 1 px** past the 20 px cell. The font's natural line is 1.32 em, about 23 px, so Roost's lines are about 13% tighter than Ghostty's or VTE's. |
| Emoji | about 21.6 px wide against a 20 px two-cell slot, so it overflows about 1.6 px |
| CJK ideograph | 17.3 px in a 20 px slot, left-aligned, leaving about 2.7 px on the right; baseline about 1.3 px lower than the primary's |

On macOS at 14pt the cell is 9×19, and descenders stay inside it. Apple
Color Emoji and the macOS faces cosmic-text actually picks were not
measured; that needs the mini.

---

## Remaining polish (ranked, 2026-11-08)

The ranking weighs how often a developer running Claude Code, nvim and a
prompt like starship would see the defect against what it costs. "Ghostty
port" means the reference is MIT and its design can be followed, often
line for line.

| # | Item | Value | Cost | Needs Tier 2? |
|---|---|---|---|---|
| 1 | SGR decorations | high | S–M | no |
| 2 | Agent-symbol fallback: list and per-face sizing (#592, #585) | medium | S–M | no |
| 3 | Antialiased sprites through an image cache (#590), with powerline (#591) | medium on Mac, low on Linux | M | no |
| 4 | Braille as sprites (#591) | low–medium | S | no |
| 5 | Bundle a Nerd Font symbols face | medium for icon users, zero for others | S plus a license audit | no |
| 6 | Linux cell metrics and `adjust-*` options (#585) | low | S | no |
| 7 | Kitty image aspect on the Mac (no issue yet) | unknown until reproduced | S–M | no |
| 8 | `font-codepoint-map`, per-style families | low | S–M | no |
| 9 | Raw path: features, synthetic italic, retire the swash delta (#585) | low | M | no |
| 10 | Ligatures, synthetic bold, `font-thicken`, `linear-corrected`, P3 | low | L–XL | yes |
| 11 | Ctrl/Cmd + scroll font zoom (#162) | low | S | no |
| 12 | Text-presentation routing misses Dingbats, Arrows and U+2B00 (added in the afternoon) | medium on Mac | S (with #592) | no |
| 13 | VS15 (U+FE0E) ignored (added in the afternoon) | low | S | no |
| 14 | Ligature expectations: document now, fix in Tier 2 (added in the afternoon) | low–medium | S to document; L–XL to fix | yes, to fix |

Re-checked at `68231b9b` in the afternoon: `DrawCell` still has no faint,
underline kind, strikethrough, overline or invisible field
(`crates/roost-vt/src/resolve.rs:7-18`), and nothing in `roost-iced` draws
faint. Items 1–11 stand as written.

### 1. SGR decorations (new; no issue filed)

- **Symptom:**
  - Dim text shows at full intensity: Claude Code's `⎿` rows and hints,
    git and shell prompts, `ls` colors.
  - nvim's LSP and spell undercurls (`4:3` plus SGR 58) are invisible, as
    are matchparen and reference underlines.
  - man pages lose their underlined arguments.
  - Markdown strikethrough in agent TUIs shows as plain text.
  - SGR 8 hidden text is shown.
- **How to see it:**
  `printf '\e[2mdim\e[0m \e[4munder\e[0m \e[4:3m\e[58:2::255:0:0mcurl\e[0m \e[9mstrike\e[0m \e[53mover\e[0m \e[8mhidden\e[0m\n'`.
  The capture recipe below already prints dim and underline, and so already
  shows the gap.
- **Approach:**
  1. **`read_cells_style`:** keep the underline kind, `overline` and the
     tagged `underline_color`.
  2. **`DrawCell`:** add `faint`, `underline` (kind), `underline_color`,
     `strikethrough`, `overline` and `invisible`.
  3. **Keep decorated blanks:** in `RenderedRow::build` and `inked_cells`,
     keep a blank cell that carries an underline, strikethrough or overline,
     as mobile's `drawless` rule does.
  4. **Draw in `draw_text`:**
     - underlines, strikethrough and overline are quads in the cell's ink
       (`cell_ink`, so they follow cursor and selection roles) or in the
       underline color;
     - positions and thicknesses come from the face's underline and
       strikeout metrics;
     - faint is the ink at alpha 0.5;
     - invisible skips the glyph.
  5. **Curly, dotted and dashed** can be stair-stepped quads first, then
     move to item 3's antialiased route.
  6. **`tab.dump_resolved`:** add the fields so `tools/roosttest` can assert
     on them.
- **Constraints:**
  - **Cache inputs.** RGB underline colors need no new input, because
    `build` already reads the style. Resolving a palette-indexed SGR 58
    adds the palette as an input, and the caching invariant then needs a
    new guard (`crates/roost-iced/src/app/terminal_tab.rs:1810-1815`).
    Storing the tagged color and resolving it at draw time avoids that.
  - **Draw order.** A strikethrough quad lands under its glyph, because
    iced draws quads first. Both are the same color, so it looks the same.
  - **Wire format.** `ResolvedCellData` is a wire type. New fields must
    keep one-back compatibility.
- **Ghostty port:** yes. Positions and thicknesses come from
  `src/font/Metrics.zig:298-301` (`ceil(max(1, …))`, measured from the
  baseline). The shapes are in `src/font/sprite/draw/special.zig` (372
  lines: single, double, dotted, dashed and curly underline,
  strikethrough, overline). WezTerm's curly underline is in
  `wezterm-gui/src/glyphcache.rs:1223` (MIT).
- **Complexity:**
  - **S:** faint, invisible, single underline and strikethrough.
  - **M:** the full underline set, with color and tests.

### 2. Agent-symbol fallback: a list and per-face sizing (#592, #585)

- **Symptom:**
  - Claude Code's spinner alternates between JetBrains Mono (`·`, `✶`) and
    fallback faces (`✢ ✳ ✻ ✽`), so its size and weight change from frame
    to frame (*inferred* from coverage; not yet seen in a capture).
  - `⎿` and, on macOS, `⏺` come from other faces too. The 2026-09-27
    capture already showed `⏺` much smaller than `●`.
  - On macOS the first common fallback is the proportional `.SF NS`.
- **How to see it:** in a Roost tab, print
  `printf '·✢✳✶✻✽ ⏺●⎿ ✔✘↳★\n'`. Compare the glyph boxes in a
  2x `roostctl screenshot` crop.
- **Approach, cheapest first:**
  - (a) Measure first. On the mini, record which face each cluster
    resolves to (`shaped_face_metrics` already gets the `font_id`).
  - (b) Install a custom `Fallback` on macOS at startup, before the
    registry scan (#592). A candidate order: Menlo, SF Mono, Apple
    Symbols, STIX Two Math, Apple Color Emoji, PingFang or Hiragino.
    Whether Menlo or Apple Symbols cover the dingbats is unmeasured.
  - (c) "Tier 1.5" sizing: per resolved face, cache a scale (Ghostty's
    order: the width of 水, then x-height, then cap height, then line
    height; `../thirdparty/ghostty/src/font/Collection.zig`), an x-centering
    offset for wide cells, and a baseline correction. Pass them through
    `fill_text`'s per-call size and position. This also fixes the emoji
    overflow and the CJK offset measured above.
  - (d) Fix the Linux route's family (Noto Sans Symbols2 or FreeSans) or
    drop it, and correct its comment.
- **Ghostty port:** the design follows Ghostty, not the code. Ghostty finds
  fallbacks through Core Text and constrains symbol-like glyphs to their
  cell, or to two cells when the next cell is blank
  (`src/renderer/cell.zig:244-290`).
- **Complexity:** S for (a), (b) and (d); M for (c).
- **Correction to #592:** its body says sizing needs the raw path. It
  doesn't; the per-call size is enough.

### 3. Antialiased sprites through an image cache (#590), with powerline (#591)

- **Symptom:**
  - `╭╮╰╯` and `╱╲╳` are stair-stepped, at 2 device px per step on Retina.
    Claude Code's input box has rounded corners.
  - Strokes snap to logical px, so on fractional Linux scales (1.25, 1.5)
    they come out uneven (*inferred* from the rounding in `draw_sprite`).
  - Powerline triangles come from the font and may show seams.
  - The rounded caps E0B4/E0B6 aren't in JetBrains Mono at all.
- **How to see it:** `printf '╭──╮ ╱╲╳\n╰──╯ \n'`, at 2x.
- **Approach:**
  - **Raster:** draw curve and diagonal primitives, and powerline geometry,
    with `tiny-skia`. It is already in `Cargo.lock` at 0.11.4 through
    iced's software renderer, so the dependency graph gains nothing.
    Render at device size.
  - **Cache:** key on (codepoint, cell device px, color). The color is
    baked in, because iced's `image::Image` has no tint (only `opacity`).
    Handles must stay stable across frames, or each frame re-uploads.
  - **Order:** draw them as images in the glyph layer. Images draw after
    quads and before text, and a sprite replaces a glyph, so the order
    holds.
  - **Snapping:** snap the rect sprites to device px with
    `GridGeometry::snap_span`.
  - **Software renderer:** this also works there, which already draws Kitty
    images. Memory note R6 records tiny-skia truncating an *upscaled*
    image's origin. A 1:1 device-size sprite should avoid that; check it.
  - **Alternative, not evaluated:** iced meshes with MSAA. That needs
    iced's `antialiasing` setting, which applies window-wide, and meshes
    on the software renderer are a separate question.
- **Ports:**
  - WezTerm `wezterm-gui/src/customglyph.rs` (MIT, Rust, about 6,000 lines,
    `use tiny_skia` at `:8`) is the closest model. It draws white
    intensity masks and tints them in its shader; Roost would bake the
    color instead.
  - Ghostty `src/font/sprite/draw/{box,powerline,geometric_shapes,branch,symbols_for_legacy_computing*}.zig`
    is the authoritative geometry (Zig on z2d), so porting it means
    transliterating.
  - Alacritty `alacritty/src/renderer/text/builtin_font.rs`
    (Apache-2.0/MIT) is a smaller cross-check.
- **Complexity:** M for the route, cache and pixel tests
  (`tools/roosttest/test_sprite_pixels.py`). Legacy Computing is another M.

### 4. Braille as sprites (#591)

- **Symptom:** braille spinners and graphs (btop, sparklines, ora-style
  spinners) come from DejaVu Sans, FreeSerif or Noto Sans Symbols2 on
  Linux, and from an unmeasured face on macOS. Dot size and alignment
  follow that face. The 2026-09-27 capture judged braille "fine" on
  macOS, so this is consistency, not breakage.
- **How to see it:** `printf '⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ⣿⡀ █\n'`.
- **Approach:** add a `0x2800..=0x28FF` arm to `sprite_geometry`. These are
  pure rects, so today's quad path draws them with no antialiasing. Extend
  `test_sprite_pixels.py`.
- **Ghostty port:** `src/font/sprite/draw/braille.zig` (148 lines). WezTerm's
  version is at `customglyph.rs:4603`.
- **Complexity:** S.

### 5. Bundle a Nerd Font symbols face (new; no issue)

- **Symptom:** starship presets, p10k, nvim devicons, lualine, yazi and
  `eza --icons` show tofu, or Unifont CSUR bitmaps on Linux, unless the user
  installs a Nerd Font. On the Pop!_OS box, `fc-list ':charset=e718'`
  finds only Unifont CSUR and AR PL. Users who already set
  "JetBrainsMono Nerd Font", which is in the curated picker list, see
  nothing wrong.
- **References:** Ghostty embeds Symbols Nerd Font on every OS, with no size
  adjustment (`src/font/SharedGridSet.zig:320-333`). It also embeds Noto
  Color Emoji and a monochrome Noto Emoji off macOS (`:376-405`). WezTerm
  ships `assets/fonts/SymbolsNerdFontMono-Regular.ttf` (2,333,488 bytes)
  with `LICENSE_POWERLINE_EXTRA.txt` (MIT).
- **Approach:**
  - Add `third_party/symbols-nerd-font` with a `README.roost.md` giving
    provenance and a removal condition.
  - Load it in `install_bundled_terminal_fonts`.
  - Let the fallback find it, or route the PUA ranges to it through item 8.
  - Add a `packaging/copyright` stanza.
- **Open points:**
  - **Licenses.** The Nerd Fonts glyph sets come from several upstreams
    under several licenses (MIT, OFL, CC-BY and others). That two MIT
    terminals ship the face suggests it's fine. *Inferred, not audited.*
  - **Size.** It adds about 2.3 MB to the binary.
- **Complexity:** S in code; the audit is the real work.

### 6. Linux cell metrics and `adjust-*` options (#585)

- **Symptom:** Linux rows are 20 px against the font's natural 23 px at
  13pt, so text looks dense. Descender ink crosses into the next row by
  under 1 px, where colored rows such as diffs and status bars might show
  it.
- **Approach:**
  - Add `adjust-cell-height` and `adjust-cell-width` (Ghostty's
    `MetricModifier`: a percent or whole px, `src/config/Config.zig:439`)
    to both rules in `CellRule::geometry`, and perhaps
    `adjust-font-baseline` too.
  - Flipping Linux to `FontDerived` is a product decision: it changes row
    counts, as plan 073 did on macOS.
- **Complexity:** S for the options. S in code, plus a release note, for the
  default flip.

### 7. Kitty image aspect on the Mac (new; no issue)

- **Symptom:** after plan 088, Charlie saw an image about 12% too tall on
  the Mac (memory note `plan-088-kitty-graphics`). The suspected cause is a
  session tab whose cell-px aspect differs from the viewer's, which
  `image_rects` (`:1088-1089`) scales per axis.
- **Inferred:** for an in-process tab the two aspects match since
  `53200e81` (both are the logical cell × scale), so a mismatch would have
  to come from a session tab sized by another client.
- **Approach:** reproduce with `kitten icat` in a local tab and in a session
  tab, and compare the tab's TIOCGWINSZ px with the viewer's cells. If they
  differ, scale uniformly. File an issue once it reproduces.
- **Complexity:** S–M. Strictly an image item, not text.

### 8. `font-codepoint-map` and per-style families

- **Value:** a power-user escape hatch, and the general form of the
  hard-coded `⏺` route.
- **Approach:** parse `U+XXXX-U+YYYY=family` into a range table that
  `draw_font` consults. Add `font-family-bold` and `-italic` the same way.
- **Complexity:** S–M.

### 9. The raw draw path (#585)

- **Unlocks:**
  - `font-feature` per glyph (stylistic sets, `zero`, `cvXX`);
  - `FAKE_ITALIC` for families with no italic face (Fira Code, Inconsolata
    and Monaco are in the picker);
  - `DISABLE_HINTING` for terminal text, which lets the vendored swash
    delta go once chrome has a route too.
- **Nothing visible today.** Whether cosmic-text 0.15 swaps in another
  family's italic is unverified; test it with Fira Code.
- **Approach:** widget-owned cosmic-text buffers, cached per (cluster, font,
  flags, features) and drawn with `fill_raw` at the column position, which
  keeps E4's per-column rule.
- **Complexity:** M.

### 10. Tier 2 items

- **The items:**
  - ligatures (`calt`, which Ghostty enables by default; it needs run
    shaping snapped to columns, the E4 drift in #309 item 3);
  - synthetic bold and `font-thicken` (needs Roost's own rasterization);
  - `linear-corrected` blending;
  - Display P3;
  - per-frame upload savings.
- **Why they wait:** no user has asked for any of them, and none fixes a
  visible bug. Keep them behind #309's performance trigger. Cost is L–XL.
  The Tier 2 and Tier 3 text below still applies.

### 11. Ctrl/Cmd + scroll-wheel zoom (#162)

A font-UX item, not rendering. S: an accumulator feeding the existing zoom
actions.

### 12. Text-presentation routing misses Dingbats, Arrows and U+2B00 (new)

- **Symptom (*inferred* on macOS; measure on the mini):**
  - `is_default_text_presentation`
    (`crates/roost-iced/src/terminal_widget.rs:250-275`) covers U+2300–23FF,
    U+25A0–25FF and U+2600–26FF only.
  - Other emoji characters whose Unicode default is text
    (`Emoji_Presentation=No`) fall to cosmic-text's cascade. On macOS that can
    reach Apple Color Emoji, which is the `⏺` bug again (commit `7f8c6de`).
  - Counted from `emoji-data.txt` (WezTerm's copy,
    `wezterm-char-props/data/emoji-data.txt`):
    - Dingbats U+2700–27BF: 18 text-default (✂ ✈ ✉ ✌ ✍ ✏ ✒ ✔ ✖ ✝ ✡ ✳ ✴ ❄
      ❇ ❣ ❤ ➡) and 15 emoji-default (✅ ✊ ✋ ✨ ❌ ❎ ❓ ❔ ❕ ❗ ➕ ➖ ➗ ➰ ➿).
    - Arrows U+2190–21FF: 8 text-default (↔ ↕ ↖ ↗ ↘ ↙ ↩ ↪).
    - U+2B00–2BFF: 3 text-default (⬅ ⬆ ⬇) and 4 emoji-default (⬛ ⬜ ⭐ ⭕).
  - **Of Claude Code's spinner frames only ✳ is an emoji character.** ✢ ✶ ✻
    ✽ aren't, so no color face claims them. ✔ (a common "done" mark) is.
- **How to see it:** `printf '✳ ✴ ❇ ✔ ✖ ❤ ➡ ↩ ⬆\n'` in a Roost tab on macOS.
  Look for color.
- **Approach:**
  - Extend the curated ranges to U+2190–21FF, U+2700–27BF and U+2B00–2BFF,
    minus their `Emoji_Presentation=Yes` entries.
  - The routing target, STIX Two Math, may not cover dingbats. So route
    through #592's macOS list rather than one family.
- **Complexity:** S, measured first (item 2a).

### 13. VS15 (U+FE0E) ignored (new)

- **Symptom:** an emoji-default character followed by VS15 (`⌚︎`) still
  cascades to color. VS16 is handled (`wants_monochrome_fallback`, `:279-287`);
  VS15 isn't.
- **Approach:** route clusters that contain U+FE0E to the monochrome family.
- **Complexity:** S. Low value.

### 14. Ligature expectations (new; raises item 10's note)

- **Symptom:** users coming from Ghostty, WezTerm or Kitty with JetBrains
  Mono expect `->`, `!=` and `>=` as ligatures. Those terminals enable `calt`
  by default. Roost shapes per cell, so ligatures never appear.
  - The chrome is the opposite (C4): Inter's `calt` does turn `->` into `→`.
- **How to see it:** `printf -- '-> => != >= <= ===\n'` beside Ghostty.
- **Approach:** the fix stays Tier 2 (run shaping snapped to columns, #309's
  trigger). Document it now in `docs/reference/fonts.md` § Limitations, beside
  the missing decorations.
- **Complexity:** S to document. L–XL to fix.

**Also confirmed:** the emoji overflow and CJK offset (item 2c) are
unchanged.

### Config knobs, if any ship

| Knob (Ghostty name) | Roost today | Cost | Depends on |
|---|---|---|---|
| `faint-opacity` | none (dim not drawn) | S | item 1 |
| `adjust-underline-position` / `-thickness`, `adjust-strikethrough-*` | none | S | item 1 |
| `adjust-cell-height` / `-width`, `adjust-font-baseline` | none | S | item 6 |
| `font-codepoint-map` | hard-coded `⏺` route | S–M | item 8 |
| `font-family-bold` / `-italic` | none | S | item 8 |
| `font-feature` (ss, cv, zero) | none | M | item 9 |
| `font-synthetic-style` (italic) | none | M | item 9 |
| `bold-color = bright` | theme RGB only | S–M | the cell fg's palette tag |
| `minimum-contrast` | none | M | own blending, or a CPU color fix per cell |
| `font-feature` (calt), `font-thicken`, `alpha-blending`, synthetic bold | none | L | Tier 2 |

Ghostty's options are in `src/config/Config.zig` (`font-codepoint-map`
`:317`, `font-thicken` `:345`, `alpha-blending` `:408`,
`adjust-cell-height` `:439`, `minimum-contrast` `:784`, `bold-color`
`:3922`, `faint-opacity` `:3929`).

---

## Chrome text (2026-11-08)

Everything here was read in code at `68231b9b` unless marked *inferred* or
*from memory*. Linux observations come from one isolated dev instance under
headless weston (llvmpipe on the Vulkan backend, not the GL fallback that
#496 was about). It was a `target/debug/roost-iced` built 2026-11-03; no
chrome, font or swash file changed after that. Nothing was captured on macOS.
Size and scale knobs and color themes are in
[`chrome-themes.md`](chrome-themes.md) and aren't repeated here.

### Today: the font and how it's loaded

- **The family:** `CHROME_FONT_FAMILY = "Inter"`
  (`crates/roost-iced/src/chrome.rs:93`). `chrome_font(weight)` (`:371`) is
  the one seam.
  - The three faces are registered with `.font(include_bytes!(...))`
    (`crates/roost-iced/src/main.rs:503-505`), and Inter Regular is the app's
    default font (`:506`).
  - There is no `default_text_size`, so text with no `.size()` falls to
    iced's 16 px (`iced_core-0.14.0/src/settings.rs:51`).
- **The bundled faces:** Regular, Medium and SemiBold (`third_party/inter/`,
  about 0.4 MB each). The README says v4.1; the name table reads
  `Version 4.001;git-9221beed3`. OFL-1.1, and the copyright line names **no
  Reserved Font Name**, so a modified copy may keep the name (renaming it is
  still cleaner).
- **Measured from the font tables** (fontTools, on the bundled Regular):

| Property | Inter Regular | Note |
|---|---|---|
| upm, ascender, descender, gap | 2048, 1984, −494, 0 | Natural line 1.21 em. iced's default line height is 1.3. |
| x-height, cap height | 0.546 em, 0.728 em | A tall x-height: Inter at 13 px reads larger than most UI faces at 13 px. |
| Digits | proportional: `1` is 833 units, `0` is 1292 | Open Sans, Noto Sans and JetBrains Mono are tabular by default. Fira Sans is proportional. |
| GSUB | `aalt calt case ccmp cv01–cv14 dlig dnom frac locl numr ordn pnum salt sinf ss01–ss08 subs sups tnum zero` | `tnum`, `case` and the `cv`/`ss` sets exist, but iced can't request them. |
| GPOS | `cpsp kern mark mkmk` | |
| Hinting | TrueType instructions (`fpgm`), `gasp` 0xF for all sizes | Used on Linux, ignored on macOS (see Rasterization). |
| Coverage | 2,852 cmap entries | Latin, Greek (105), Cyrillic (about 250), and ✓ ✗ ● ○ → ← ↑ ↓ ↳ ⌘ ⌥ ⇧ ⌃ ⏎ ⎋ ⌫ … › · • ★ ⚠. **Missing:** ✳ ✢ ✶ ✻ ✽ ✔ ✘ ⏺ ⎿ ◐ ⚡, all braille (0 of 256), all box drawing (0 of 128), emoji and CJK. |

### Today: type per region

Sizes are iced logical px (points on macOS). "Shaping" is what reaches
cosmic-text:
- **Adv:** `Shaping::Advanced`, which applies kerning and `calt`.
- **Auto:** iced's default. It becomes **Basic for ASCII**: cmap advances
  only, no kerning, no `calt` (`iced_graphics-0.14.0/src/text.rs:325-337`;
  `shape_skip` in `cosmic-text-0.15.0/src/shape.rs:475`).
- `rich_text` is always Advanced (`iced_widget-0.14.2/src/text/rich.rs:498`).

| Region | Text | Size / weight | Line height | Shaping | Color (role) | Truncation | Where |
|---|---|---|---|---|---|---|---|
| Sidebar band | `PROJECTS`, `AGENTS` (caps literals) | 11 SemiBold | default 1.3 | Auto (Basic) | `muted_text` on `band` | none | `app.rs:2127`, `:2167-2173`; `app/agents_panel.rs:22` |
| Host band | host label, uppercased | 11 SemiBold | default | Adv | `muted_text` | tail "…", sharing the line with the pill and rollup | `app.rs:2189-2224`, `:6716-6722`; uppercasing at `roost-ui-model/src/host_sidebar.rs:492` |
| Host band | rollup ("2 agents"), update or fidelity pill | 10 Regular | default | Adv | `host_rollup_text`, pending amber | the rollup is dropped when it doesn't fit | `chrome.rs:100`, `app.rs:2246-2280`, `:6735-6745` |
| Project row | name | 13 Regular | default | Adv | `project_label_inactive` `#d1d1d1`; white when selected (color, not weight) | tail "…" through `ElisionMemo` | `chrome.rs:111`, `app.rs:6589-6602` |
| Project row | agent count | 10.5 Regular | default | Auto | `host_rollup_text` `#6e747a`, also on the selected pill | fixed 18 px slot, right-aligned | `chrome.rs:112-113`, `app.rs:6604-6612` |
| Agents panel header | sort word (`grouped`, …) | 11 Medium | default | Auto | muted; `text` on hover | none | `agents_panel.rs:261-270` |
| Agents group head | project label, count | 12 SemiBold; count 12 Regular | 16 absolute | Adv label, Auto count | `muted_text` | label tail "…"; count in a 20 px slot | `agents_panel.rs:282-323` |
| Agent row, line 1 | name, age ("45s") | 13 Regular; age 11 Regular | 18 absolute | Adv name, Auto age | `text`; age muted | name tail "…", after a leading marker glyph is stripped; age right-aligned in a 28 px slot | `agents_panel.rs:328-381`, `chrome.rs:147-151`; stripping at `roost-ui-model/src/agent_palette.rs:483-510` |
| Agent row, line 2 | `location · status` | 11 Regular | 16 absolute | rich text (Adv) | location muted; status muted, or amber or red for waiting or failed | elided as one string, then split so the surviving status keeps its color | `agents_panel.rs:386-404`, `:54` |
| Agents empty | "No agents running" | 11.5 Regular | default | Auto | muted | none | `agents_panel.rs:214`, `chrome.rs:160` |
| Tab pill | title, as the tab reports it | 12; Regular inactive, **Medium active** | default | Adv | muted inactive, `text` active | tail "…" in an 80–220 px pill; the weight change re-elides | `chrome.rs:52-59`, `app.rs:3263-3290`, `:7104-7160` |
| Tab band | `×`, `+` | 13, 16 | default | Auto | `text`, muted | none | `app.rs:7191`, `:7238` |
| Sidebar footer | "+ New Project" | 13 Regular | default | Auto | `text` on `footer_chip` | none | `app.rs:6909` |
| Palette | query input | 17 | default | n/a | `text`; placeholder `#9e9e9e` | n/a | `app.rs:7338-7343` |
| Palette row | title with match spans | 14; **match spans SemiBold** in `palette_match` | default | rich text (Adv) | `text` | **none**: no wrap, and nothing stops it running under the accel (*inferred*) | `app.rs:7400-7420` |
| Palette row | subtitle, trailing accel ("Alt+T") | 12 | default | Auto | `palette_placeholder` | **none** | `app.rs:7421-7436` |
| Agents palette row | agent, **project (in a 140 px box)**, name, status | 14 / 14 SemiBold / 14 / 13 | default | Auto | muted / `text` / `text` / lifecycle color | **by display columns, not pixels**: project 24, agent 14, the whole left side 58 (`app/palettes.rs:11-25`). The project overflows its box (seen). | `app.rs:2284-2303`, `app/palettes.rs:136-190` |
| Agents palette row | metrics, time | 12 **JetBrains Mono** (`Font::MONOSPACE`) | default | Auto | muted | none | `app.rs:7385-7397` |
| Context menu, Linux | items, headers | 13 Regular | default | Adv | `text`, muted | tail "…" | `app/context_menu.rs:39`, `:640-662` |
| Context menu, macOS | native `NSMenu`: system font; headers bold at `smallSystemFontSize` | n/a | n/a | Core Text | AppKit | AppKit | `macos/context_menu.rs:196-201`; `app/context_menu.rs:719-725` |
| Modals | title, body, buttons, field labels, error | 15 SemiBold, 12, 12, 11, 11 | default | Auto (Adv only when non-ASCII) | `text`, muted, error | wraps | `app.rs:563-570`, `:605-640`, `:873-890`, `:6324` |
| Agent-hooks card | agent name, chip, file and status lines, footer | 12 Medium, 10, 10, 11 | default | Auto | `text`; green or muted; muted | wraps | `app.rs:655-725` |
| Toast | the bottom line | 12 | default | Auto | info `text`, warning `#eecfa2`, error `#ee7878` | max 520 px, wraps | `app.rs:266-272`, `:7313-7316` |
| Host notice strip | message, action | 12, 11.5 | default | Auto | `#eecfa2` on `#4a3323` | wraps | `chrome.rs:106-107`, `app.rs:814-845` |
| Inline rows | "↻ Reconnect", fidelity row, "Sized by …", lock tooltip | 12 | default | Adv (non-ASCII) or Auto | various | none | `app.rs:6787-6870` |

**What the table shows:**

- **Ten sizes:** 10, 10.5, 11, 11.5, 12, 13, 14, 15, 16 (the `+` only) and 17.
  - The half-pixel sizes sit next to whole ones (10 and 10.5, 11 and 11.5), so
    on screen they read as the same size.
  - `chrome-themes.md` counts the same ten at 56 `.size()` sites.
- **Three weights:** Regular, Medium and SemiBold. No Bold.
- **Two shaping paths at random.** About a dozen labels pin `Advanced`; the
  rest get Basic for ASCII. The choice follows history (labels that are
  elided were pinned so their measurement matches), not a rule.
- **No letter-spacing and no font features anywhere.** cosmic-text 0.15
  supports both (`Attrs::letter_spacing`, `attrs.rs:311`;
  `Attrs::font_features`, `:317`). iced's `to_attributes` passes only family,
  weight, stretch and style (`iced_graphics-0.14.0/src/text.rs:259-265`). The
  same wall blocks `tnum`, `case` and turning `calt` off.

### Today: measured on Linux (1x, isolated instance)

- A 13 px Inter label's lowercase drew across 8 pixel rows (7.1 px nominal),
  with ascenders 3 rows above. 11 px SemiBold caps drew 8 rows (8.0
  nominal).
  - *Inferred:* the TrueType hinting rounds the x-height up at 13 px. That is
    part of why Linux chrome looks crisp, and slightly larger than its size.
- At 2x everything looked clean. Elision with "…" worked everywhere it is
  implemented in pixels (tab pills, project rows, agent rows, group heads).
- Seen: the agents palette overflow (C1), the highlight weight bleed (C2),
  and the near-invisible "Idle" and selected-row count (C3).

### Today: contrast

WCAG 2.x ratios from `ChromePalette::roost_dark` (`chrome.rs:288-345`) with
the default accent, recomputed independently. The palette is opaque, and
`web-colors` makes blending sRGB, so alpha roles are composited in sRGB.

- **The bar:** no chrome text is WCAG "large" (18.66 px bold or 24 px
  regular), so 4.5:1 applies. Status dots are non-text and need 3:1.
- **Exempt:** disabled rows (a dimmed host section at alpha 0.45,
  non-actionable palette rows) count as inactive components.
- **Reference:** Apple's secondary label is white at about 55% (*from
  memory*), about 5.5:1 on `band`. Roost's `muted_text` `#a0a4b0` is 5.9:1.

| Text | Colors | Ratio | Passes |
|---|---|---|---|
| Band labels, sort word, inactive tab title | `#a0a4b0` on `#24292c` | 5.90 | AA |
| Active tab title | `#f2f2f2` on `#243751` | 10.78 | AAA |
| Inactive project label | `#d1d1d1` on `#2d3235` | 8.49 | AAA |
| Selected project label | white on `#13509d` | 7.89 | AAA |
| Agent name | `#f2f2f2` on `#2d3235` | 11.58 | AAA |
| Agent detail, age, group head, empty | `#a0a4b0` on `#2d3235` | 5.21 | AA |
| Agent detail on the active agent row | `#a0a4b0` on `#3a3a3a` | 4.57 | AA, barely |
| Waiting status | `#f0a040` on `#2d3235` | 6.06 | AA |
| **Failed status (11 px)** | `#e05252` on `#2d3235` | **3.40** | **fails** |
| **Failed status on the active agent row** | `#e05252` on `#3a3a3a` | **2.98** | **fails** |
| **Host band rollup (10 px)** | `#6e747a` on `#24292c` | **3.11** | **fails** |
| **Project agent count (10.5 px)** | `#6e747a` on `#2d3235` | **2.74** | **fails** |
| **Project agent count on the selected row** | `#6e747a` on `#13509d` | **1.67** | **fails badly** |
| Palette title | `#f2f2f2` on `#2d2d33` | 12.22 | AAA |
| Palette subtitle, accel | `#9e9e9e` on `#2d2d33` | 5.11 | AA |
| Palette subtitle on a hovered row | `#9e9e9e` on `#3d3d43` | 4.03 | fails, slightly |
| **Palette subtitle on the selected row** | `#9e9e9e` on `#48484e` | **3.39** | **fails** |
| Palette match | `#5fa3f0` on `#2d2d33` | 5.20 | AA |
| **Palette match on the selected row** | `#5fa3f0` on `#48484e` | **3.45** | **fails** |
| **Agents palette "Idle"** (`agent_inactive`, alpha 0.5) | about `#545456` on `#2d2d33` | **1.81** (1.47 selected) | **fails** |
| Agents palette "Finished" | `#7a7a7a` on `#2d2d33` | 3.19 | fails |
| Agents palette "Failed" on the selected row | `#e05252` on `#48484e` | 2.38 | fails |
| Modal body | `#a0a4b0` on `#2d2d33` | 5.49 | AA |
| Modal and toast error | `#ee7878` on `#2d2d33` | 4.95 | AA |
| Host banner | `#eecfa2` on `#4a3323` | 7.87 | AAA |
| Primary button label | `#f2f2f2` on `#007aff` | 3.59 | fails; the same as Apple's white on system blue |
| Finished dot (non-text) | `#7a7a7a` on `#2d3235` | 3.02 | 3:1, barely |

**The selected row follows the macOS accent.** `chrome-accent` defaults to
`system` (`roost-ui-model/src/config.rs:154-164`). For a non-default accent
the selected row is the accent mixed half and half with `#1f1f1f`
(`chrome.rs:334-347`, `mix` at `:361`). With macOS's accent colors (hexes *from memory*):

| Accent | Selected row | White label | Count today | White at 0.7 |
|---|---|---|---|---|
| Blue (default path) | `#13509d` | 7.89 | 1.67 | 4.73 |
| Graphite | `#565656` | 7.34 | 1.55 | 4.57 |
| Orange | `#8b511d` | 6.38 | 1.35 | 4.04 |
| Green | `#416d33` | 6.07 | 1.28 | 3.90 |
| Yellow | `#8f7310` | 4.54 | 1.04 | 3.08 |

So no fixed alpha of white works for every accent. The count on a selected
row should use the label's own color. That is only as good as the label: a
bright custom accent such as `chrome-accent = #ffffff` gives a `#8f8f8f` row,
where the white label itself is 3.23:1. Meeting 4.5:1 there means picking dark
text on a light row (by the row's luminance), with a bright accent in the
contrast tests.

**The smallest lifts that reach 4.5:1 at the same hue:**

| Role | Lift to | Ratio | Note |
|---|---|---|---|
| `host_rollup_text` | `#94999e` | 4.51 on `list`, 5.12 on `band` | |
| Count on the selected row | the label's color | equal to the label | white at 0.7 fails under some accents |
| Failed, as text | `#e77777` | 4.51 on `list` | close to the existing `error_text` `#ee7878` |
| Palette subtitle on the selected row | `#b8b8b8` | 4.58 | |
| Palette match on the selected row | `#89bbf4` | 4.53 | |

### Today: rasterization per OS

- **Both OSes:**
  - shaping is cosmic-text 0.15 (harfrust) and rasterization is swash;
  - antialiasing is grayscale only;
  - glyph positions are quantized to quarter pixels (`SubpixelBin`,
    `cosmic-text-0.15.0/src/glyph_cache.rs:63-70`);
  - blending is sRGB (`web-colors`);
  - iced's `crisp` feature snaps quads, not text
    (`iced_core-0.14.0/src/renderer.rs:99`).
- **macOS: unhinted.** The vendored swash forces `hint(false)` on macOS
  (`third_party/swash/src/scale/mod.rs:400-402`; plan 073 D8, #581,
  `README.roost.md:36-44`), and that covers chrome text too.
  - **Possibly lighter than AppKit text** (*inferred*). Core Text also
    applies font smoothing (stem darkening), which Ghostty's `font-thicken`
    imitates. So Inter chrome may read slightly lighter than Finder at the
    same size. The 2026-09-27 capture measured terminal text only.
  - **Inter beside SF.** Native context menus and the menu bar are SF, so a
    right-click puts SF next to Inter.
- **Linux: hinted** with Inter's own TrueType instructions.
  - COSMIC apps go through the same cosmic-text and swash path. libcosmic is
    built on an iced fork that wasn't checked out, so "it doesn't turn
    hinting off either" is *inferred*.
- **cosmic-text 0.16's `Hinting` enum isn't a way out of the swash delta.**
  It is metrics hinting (snapping x positions in layout;
  `../thirdparty/cosmic-text/src/layout.rs:197-215`), not outline hinting.

### Today: compared with native apps

| Role | Roost | macOS (AppKit) | COSMIC (libcosmic `dd108aa`) | GNOME (libadwaita) |
|---|---|---|---|---|
| UI face | Inter 4.1, bundled | SF Pro (system) | Open Sans (`cosmic-theme/src/model/font.rs:9`). This box's portal reports `Open Sans 10.5` (per `chrome-themes.md`). | Cantarell before GNOME 48; Adwaita Sans, which is Inter-derived, from 48 (*from memory*). `gsettings` here says `Cantarell 11`, which isn't installed. |
| Body or list row | 13 | 13 pt (*from memory*) | 14 px, line 21 (`text::body`, `src/widget/text.rs:106-112`; `default_text_size: 14.0` at `src/app/settings.rs:102`) | 11 pt, about 14.7 px |
| Secondary | 11 (agent detail, ages), 12 (tabs, palette subtitle) | Callout 12, Subheadline 11, Footnote 10 (HIG text styles, *from memory*) | caption 12 px, line 17 (`:119-125`) | not re-read |
| Section header | 11 SemiBold, ALL CAPS | sidebar headers 11, bold, title case (*from memory*) | `caption_heading` 12 SemiBold, line 17 (`:93-99`) | `.heading`, bold (*from memory*) |
| Card title | 15 SemiBold | Title 3 is 15 (*from memory*) | `heading` 14 Bold; `title4` 20 Bold (`:67-86`) | not re-read |
| A user font setting for the chrome | none | n/a | the interface font | the interface font |

**Other terminals and editors (design only):**

- **Ghostty (MIT):** chrome size follows the OS. Only the title font family
  is configurable (per `chrome-themes.md`).
- **WezTerm (MIT):** `window_frame.font` and `font_size` for the tab bar
  (Roboto, 12 pt by default off Windows;
  `docs/config/lua/config/window_frame.md:51-56`); a separate
  `command_palette_font_size = 14.0` and `command_palette_font`; also
  `char_select_font_size` and `pane_select_font`.
- **Zed** (app crates GPL-3.0-or-later; only setting names and defaults from
  `assets/settings/default.json` at `d2a4fb6`): `ui_font_family` `.ZedSans`
  (IBM Plex Sans) or `.SystemUIFont`; `ui_font_features` `{"calt": false}`;
  `ui_font_weight` 400; `ui_font_size` 16, separate from `buffer_font_size`
  15; a separate `agent_ui_font_size`.

**Reading it:**
- **macOS:** Roost's sizes already match AppKit's scale.
- **Linux:** every Roost role is about 1 px smaller than COSMIC's and about
  1.5 px smaller than GNOME's. That is a default-size choice
  (`chrome-themes.md` key question 2: 13 or 14 on Linux), not a rendering
  defect.

### Chrome items (ranked)

Ranked by how often a user running agents would see the defect, against the
cost.

| # | Item | Value | Cost | Issue |
|---|---|---|---|---|
| C1 | Palette overflow: agents palette columns budgeted in display columns, palette rows unbudgeted | high | S | none |
| C2 | Match-highlight weight bleed (cosmic-text 0.15 ASCII fast path) | medium | S | none |
| C3 | Low-contrast secondary text (count, rollup, failed, palette selected row, Idle) | medium | S | none; lands on the #584 seam |
| C4 | One chrome label constructor: one shaping path, a `calt` decision | medium | S (+ S–M for a patched face) | none |
| C5 | A role type scale (groundwork for chrome font size) | high, as an enabler | M | none; `chrome-themes.md` phase 0 |
| C6 | Tabular figures for ages, counts and metrics | low–medium | S–M | none |
| C7 | Truncation quality: path-aware titles, grapheme cuts | medium | S each | none |
| C8 | Agent marker glyphs in tab pills | medium | S | none |
| C9 | All-caps band labels without tracking | low | S | none |
| C10 | macOS: Inter beside native SF, and its weight | low | S to measure | none |
| C11 | A chrome font family option (`system`, any installed family) | low now | S–M after C5 | none |
| C12 | Inter Display or variable Inter | very low | S | none |

#### C1. Palette overflow (new)

- **Symptom:**
  - **Agents palette.** The project column is cut to 24 display columns
    (`PALETTE_AGENT_PROJECT_MAX_COLUMNS`, `app/palettes.rs:11`) and drawn in
    a `container(...).max_width(140)` (`app.rs:2293-2300`). Inter is
    proportional, so 24 columns at 14 px SemiBold run from about 85 px
    (`iiii…`) to 285 px (`wwww…`). "mobile-client-android" (21 columns) is
    148 px, so it isn't cut at all and still overflows. Seen in the capture:
    the project ran into the tab name "flutter run".
  - **Why nothing clips:** iced's text `draw` clips to the viewport, not to
    the widget's bounds (`iced_core-0.14.0/src/widget/text.rs:336-357`).
  - **Generic palette rows.** A long title or subtitle has no budget, so it
    can run under the trailing accel text (*inferred*, same mechanism).
  - The existing tests (`app/palettes.rs:2830-2892`) pin the column budgets,
    which hold; nothing pins pixels.
- **How to see it:** name a project `mobile-client-android-tablet-experiments`,
  give it an agent (`roostctl tab report --claim --lifecycle failed …`
  against an isolated instance), then `roostctl palette open --kind agents`.
- **Approach:**
  - Elide the project column in pixels through the font-keyed
    `sidebar_elisions` memo, against 140 px. Then either drop the column
    budget for it, or keep it as a cheap upper bound.
  - Give the name column the row's remaining pixel budget. The palette is a
    fixed 660 px (`PALETTE_WIDTH`, `chrome.rs:68`), so the budget is
    computable once the metrics and time widths are measured.
  - Elide generic titles and subtitles against the row width minus the
    measured trailing text. The match spans must be cut with the title.
  - Add a unit test like `the_age_and_count_slots_hold_their_widest_text`:
    every segment measures within its box.
- **Note for `chrome-themes.md`:** it says the agent columns "need only
  their containers scaled". After C1 they are measured in pixels, so they
  scale with `ChromeMetrics` like every other elided label.
- **Complexity:** S.

#### C2. Match-highlight weight bleed (new)

- **Symptom:**
  - "Use a session for local tab**s**": the unmatched "s" draws SemiBold.
  - "Nex**t** T**ab**": the matched "t" and "ab" draw Regular.
  - Whenever a fuzzy match starts or ends inside an ASCII word, the whole
    word takes the weight of its first character. Color still comes per
    glyph, so only the weight is wrong.
- **Cause (verified):**
  - cosmic-text 0.15's ASCII fast path shapes a whole ASCII word as one run
    (`cosmic-text-0.15.0/src/shape.rs:666-680`), and `shape_run` picks the
    font from the span at the run's start (`:300-302`).
  - Fixed in 0.16.0 ("Do not use ASCII fast path when a word has
    incompatible spans"; `../thirdparty/cosmic-text` commit `8cd21a31`, a
    10-line change, `shape.rs:848-855` at 0.19).
  - Roost's lockfile has cosmic-text 0.15.0, and iced 0.14.0 is still the
    newest iced on crates.io (`cargo search`, 2026-11-08).
- **How to see it:** `roostctl palette open`, then `roostctl palette query
  tab`. Look at "tabs" and "Next Tab" at 2x.
- **Approach, cheapest first:**
  - (a) Color-only highlight: drop the `span.font(Semibold)` at
    `app.rs:7410-7411`. The blue already carries the match.
  - (b) Widen each match span to whole words. This changes what a match
    looks like; not recommended.
  - (c) Vendor cosmic-text 0.15 with the fix, as swash is. A whole second
    vendored crate for one function.
  - (d) Wait for an iced on cosmic-text 0.16 or later. No release date.
- **Recommendation:** (a).
- **Complexity:** S.

#### C3. Low-contrast secondary text (new)

- **Symptom:** the project row's agent count is close to invisible on the
  selected pill, and faint elsewhere. The 10 px host rollup and the 11 px
  "Failed" are hard to read when tired or in a bright room. The ratios are
  in the contrast tables above.
- **How to see it:** the sidebar crop of an isolated capture at 1x. The
  selected row's "1" is barely visible.
- **Approach:**
  - **Text roles, not dot roles.** Add text roles (for example
    `agent_failed_text`, and an Idle and Finished status text in
    `muted_text`) and leave the lifecycle colors for dots. The dot colors are
    asserted by `tools/roosttest/test_sidebar_pixels.py:67-70` and by unit
    tests at `app.rs:15131-15143`; text roles keep those unchanged.
  - Lift `host_rollup_text` (it is used only as text), and the palette's
    selected-row subtitle and match, to the hexes in the "smallest lifts"
    table.
  - Draw the count on a selected row in the label's color.
  - Add the contrast unit test `chrome-themes.md` lists as a quick win: every
    text role on its background at 4.5:1, for the default and a few sample
    accents.
  - This is a palette data change on `ChromePalette`, the seam chrome themes
    (#584) fill. A `roost-contrast` theme there builds on it.
- **Constraints:** the default pixel lanes (`test_tab_strip_pixels.py:85-98`
  pins `BAND`, `ACTIVE_TAB`, `DIVIDER`, `MUTED_TEXT` and the badge) don't
  pin any of the changed roles, so no lane needs to change.
- **Complexity:** S.

#### C4. One chrome label constructor: one shaping path, and `calt` (new)

- **Symptom:**
  - **Uneven kerning.** ASCII labels without `.shaping(Advanced)` get no
    kerning: modal titles, toasts, buttons, palette subtitles, agents
    palette columns, ages and counts. Shaped with HarfBuzz, "AV Te" is 5%
    wider unkerned (5918 vs 5618 units). "PROJECTS" happens to have no
    kerning pairs, but in the same sidebar the host band labels are kerned
    and the header isn't.
  - **Measured one way, drawn another.** `chrome::text_width` always
    measures with Advanced (`chrome.rs:419-421`). A label that is drawn
    Basic can come out wider than its budget. The age and count slot test
    (`agents_panel.rs:486-496`) measures Advanced for text drawn Auto.
  - **Arrow substitutions.** Advanced labels get Inter's `calt` (verified
    with uharfbuzz on the bundled Regular):
    - `a->b` draws as `a→b`, and `x => y` as `x ⇒ y`;
    - `12:30` gets a raised `colon.case`.
    - So a tab title that quotes a command, such as "Rename a -> b", shows
      an arrow, while the same string in a toast shows `->`. That harfrust
      applies the same default features as HarfBuzz is *inferred* from it
      being a HarfBuzz port.
- **How to see it:** `roostctl set-title --title 'a -> b'`. The pill shows
  `→`.
- **Approach:**
  - **A constructor.** `chrome::label(text, role)` sets font, size, line
    height, `Shaping::Advanced` and `Wrapping::None` in one place. Measuring
    goes through the same role, so measured and drawn can't drift.
  - **Migrate** the 56 `.size()` sites to it (36 literals and 20 constants,
    per `chrome-themes.md`). The 36 literals are that note's phase 0 commit.
  - **Cost:** `test_view_perf.py` guards it. iced caches paragraphs, so the
    shaping cost is paid only when the text changes.
  - **The `calt` decision.** iced can't turn `calt` off per run. The options:
    - (a) accept it; arrows in prose read fine;
    - (b) ship a patched Inter with the arrow lookups removed from `calt`.
      fontTools can do it. OFL has no Reserved Font Name here, but rename the
      family anyway, for example "Inter Roost", with a provenance README;
    - (c) wait for iced to pass font features. Zed's UI default turns `calt`
      off; its reason isn't stated in its settings file.
- **Complexity:** S for the constructor; S–M for a patched face.

#### C5. A role type scale (the groundwork for chrome font size)

- **Why first:** Charlie wants a bigger chrome font for tired days.
  `chrome-themes.md`'s `ChromeMetrics` (its option S2, phase 1) needs every
  size, line height, fixed slot and elision budget to come from one place.
  Today ten sizes are scattered as literals and constants.
- **Proposed roles** (today's sizes, half-pixel steps merged):

| Role | Size | Used for |
|---|---|---|
| caption | 11 | detail lines, ages, band labels, the sort word, field labels; absorbs 10, 10.5 and 11.5 |
| label | 12 | tab titles, palette subtitles, toasts, buttons, banners |
| body | 13 | project rows, agent names, menus, the footer |
| list-title | 14 | palette rows |
| title | 15 | modal titles |
| input | 17 | the palette query |

  Weights stay Regular, Medium and SemiBold. Merging 10 into 11 makes the
  host rollup and the agent-hooks chips a step bigger; keep a `micro` 10 role
  if that is unwanted.
- **Derived values:** line heights become the role's (1.3 × size, or the
  agents panel's absolute 18 and 16). The fixed slots (age 28, counts 18 and
  20) become measured from the widest string at the role's size;
  `the_age_and_count_slots_hold_their_widest_text` already pins that
  contract.
- **Then** `chrome-font-size` scales the roles through `ChromeMetrics`, and
  a Linux default of 14 (if chosen) is one table entry.
- **Complexity:** M on its own; it folds into `chrome-themes.md` phases 0
  and 1.

#### C6. Tabular figures

- **Symptom:**
  - **Ages and counts.** Agents panel ages ("1m", "45s") and counts use
    proportional digits. At 11 px a "1" is about 4.5 px and a "0" about
    6.9 px, so a right-aligned age's left edge moves as it ticks. The clock
    is 15 s (`agents_panel.rs:19`).
  - **Palette metrics.** The agents palette draws metrics and time in
    JetBrains Mono instead, which puts two typefaces in one row.
- **How to see it:** watch an agent's age across a tick at 2x.
- **Approach:**
  - (a) A fourth bundled face, "Inter Roost Tabular", with `tnum` frozen in
    as the default (a fontTools feature freeze, about 0.4 MB, OFL allows
    it). Use it for numeric roles, and for the palette metrics in place of
    JetBrains Mono.
  - (b) A raw chrome text path (a cosmic-text buffer with `font_features`,
    drawn with `fill_raw`). Large for what it buys.
  - (c) Accept it.
- **Complexity:** S–M for (a).

#### C7. Truncation quality

- **Symptom:**
  - **Paths lose their useful end.** Shell tab titles are often paths. Tail
    elision keeps the uninformative head: a pill showed
    `/tmp/claude-1000/-home-…` in the isolated capture.
  - **Cuts can split clusters.** The cut is at char boundaries
    (`chrome.rs:462-465`), so a ZWJ emoji sequence, a flag or a combining
    mark can be split.
  - **RTL.** A name in Arabic or Hebrew gets the "…" on the logical tail,
    which is visually the left (*inferred*).
- **Approach:**
  - **Grapheme cuts.** `unicode-segmentation` is already a dependency of
    `roost-ui-model` (`crates/roost-ui-model/Cargo.toml:19`), which
    `roost-iced` uses. The comment at `chrome.rs:462-464` says no segmenter
    is in the dependency set, which is out of date. S.
  - **Path-aware titles.** When a title looks like a path, abbreviate
    `$HOME` to `~`, then elide in the middle (`~/p/…/roost`). macOS Finder
    and Terminal favor the tail of a path. S.
  - **Dumps stay full.** `app.sidebar_dump` already reports full text, so the
    IPC contract doesn't change.
  - **Later:** cosmic-text 0.18 added start, middle and end ellipsizing
    (CHANGELOG). Once iced exposes it, `elide_to_width` could go.
- **Complexity:** S each.

#### C8. Agent marker glyphs in tab pills (new)

- **What already exists:** the Agents panel and the agents palette strip a
  leading marker glyph from agent titles (`strip_leading_marker`,
  `roost-ui-model/src/agent_palette.rs:491-510`). It is structural: leading
  non-ASCII, non-alphanumeric characters go. Its comment records that Claude
  Code prefixes its window title with `✳ ` (U+2733).
- **Symptom:** tab pills draw the title as reported, so they show the
  marker the agent rows strip.
  - Inter has none of these glyphs. On Linux the cascade lands on DejaVu
    Sans, which covers U+2733 and U+2802 on this box (`fc-list`).
  - On macOS ✳ is an emoji character (text-default), and chrome text has no
    monochrome routing, so it may draw as Apple Color Emoji (*inferred*; the
    `⏺` failure mode).
  - If an agent animates its title (a braille spinner, say; *not verified*
    for Claude Code), each frame re-elides. The memo caps at 1024 entries
    (`chrome.rs:494`).
- **How to see it:** `roostctl set-title --title '✳ Claude Code'` in a dev
  instance. On the mini, compare the pill at 2x.
- **Approach:**
  - (a) Apply the same strip to the pill's display text, through one shared
    helper made public, only on a tab an agent owns (the rows' case). The
    strip is structural, so on a plain tab it would eat a user's `🚀 Deploy`;
    those keep their title. Dumps keep the full title. The tab's status dot
    already shows the agent's state.
  - (b) Also let #592's macOS fallback list reach chrome. It is installed on
    the shared `FontSystem`, so it does by construction.
- **Complexity:** S for (a). (b) rides on #592.

#### C9. All-caps band labels

- **Symptom:** "PROJECTS", "AGENTS" and host bands are 11 px SemiBold caps
  with no tracking and no `case` feature. Caps at small sizes usually want
  about +0.05 em. macOS sidebars use title case (*from memory*).
- **Approach:**
  - (a) Keep them; it is a deliberate look, like VS Code's.
  - (b) Title case. Tests assert "PROJECTS" (`app/servicing.rs:5548-5577`,
    `app.rs:12183`), and `host_sidebar.rs:492` uppercases host labels.
  - (c) Tracking needs iced to pass letter spacing; not today.
- **Complexity:** S for (b).

#### C10. macOS: Inter beside native SF, and its weight

- **Symptom (*inferred*; nothing captured on macOS this pass):**
  - **Weight.** Unhinted Inter without Core Text's darkening may look
    lighter than Finder at 13 pt.
  - **Two fonts.** Right-click menus are native `NSMenu`, in SF, beside
    Inter chrome.
- **How to see it:** on the mini, with a dev build (the `mac-test` skill):
  `roostctl screenshot --scale 2` of the sidebar, next to a Finder sidebar
  capture at the same size. Compare the inked-pixel share with this note's
  "Measuring" method.
- **Approach:** measure first.
  - **If too light:** a variable Inter at weight 430–450 for "Regular", on
    macOS only (C12). cosmic-text 0.15 drives the `wght` axis of a variable
    font from the requested weight (`swash.rs:25-41`).
  - **The menu mismatch:** accept it. Native menus were plan 073 D9's
    choice.
- **Complexity:** S to measure; S for the weight change.

#### C11. A chrome font family option

- **Precedents:** Ghostty's `window-title-font-family`, WezTerm's
  `window_frame.font`, Zed's `ui_font_family` (`.SystemUIFont`).
- **Approach:**
  - `chrome-font-family = system | <family>`, Inter by default.
  - **macOS:** `system` resolves to `.SF NS`, which cosmic-text's own macOS
    fallback list names, so fontdb finds it (*inferred*). It won't get
    AppKit's size-dependent tracking or its Text and Display switch, so it
    won't match AppKit exactly (*inferred*: harfrust and cosmic-text don't
    apply `trak`).
  - **Linux:** read the desktop interface font through the portal reader
    `chrome-themes.md` B1 would build. COSMIC gives Open Sans; GNOME 48 and
    later give Adwaita Sans.
  - Fixed slots and elision budgets must be measured per family, which C5
    makes automatic.
  - `third_party/inter/README.roost.md`'s removal condition already expects
    a config key.
- **Recommendation:** not now. Inter keeps both OSes identical, which the
  pixel lanes and the mockups rely on.
- **Complexity:** S–M after C5.

#### C12. Inter Display and variable Inter

- **Inter Display** is cut for large text, roughly 20 px and up (*from
  Inter's docs, from memory*). Roost's largest chrome text is 17 px. Skip
  it.
- **Variable Inter:** one file in place of three, and any weight (C10).
  cosmic-text doesn't drive the `opsz` axis, so the default instance is
  used. File size wasn't measured.
- **Recommendation:** only if C10 shows a need.

### Key questions (chrome)

1. **Issues:** file C1, C2 and C8 as one "chrome text bugs" issue, or
   three? None exists today.
2. **C2's fix:** color-only highlight (recommended), or vendor the
   cosmic-text fix?
3. **C3's colors:** accept the lifted values and separate text roles? Should
   the count on a selected row simply use the label's color?
4. **C4's `calt`:** keep the arrows, ship a patched "Inter Roost", or wait
   for iced to pass font features?
5. **C5:** merge the half-pixel sizes (10.5 and 11.5 into 11), and 10 into
   11 too, or keep a 10 px `micro` role?
6. **C6:** bundle a tabular Inter face, or keep JetBrains Mono for numbers
   in the palette?
7. **C7:** middle-elide path-like tab titles with `~` abbreviation?
8. **C8:** strip marker glyphs from tab pills as the agent rows already do?
9. **C9:** all-caps band labels or title case?
10. **C10:** worth a Mac measurement session (Inter vs Finder at 2x)? It
    can share the mini session with terminal items 2a and 12.
11. **C11's default:** if a family option ships, Inter or `system`?

### Sequencing (chrome)

1. **C1, C2, C3 and C8 together.** Small fixes and a contrast test; no lane
   changes.
2. **C4's constructor with C5's roles.** The seam `chrome-themes.md` phase 1
   needs.
3. **`chrome-font-size` on top** (`chrome-themes.md` phase 1). This is
   Charlie's "bigger font when tired".
4. **C7,** independent and S, any time.
5. **C6, then C10's Mac measurement, then C11 or C12,** only on demand.

### Capture recipe: isolated chrome on Linux

This never touches a running Roost or roost-session:
- a private runtime dir and an in-process backend;
- an explicit `--socket` on every call;
- `env -i`, so no `DBUS_SESSION_BUS_ADDRESS` reaches the user's portal.

The runtime dir must be short (SUN_LEN). Stop only the pid you started.

```bash
S=<scratch dir>; RT=$(mktemp -d /tmp/rr.XXXX); chmod 700 "$RT"
mkdir -p "$RT/sess" "$S"/{home,cfg,state,data,cache,rstate}; chmod 700 "$RT/sess"
printf 'local-backend = in-process\nagent-hooks = off\nshow-sidebar-agents = true\n' > "$S/config.conf"
env -i PATH=/usr/bin:/bin HOME="$S/home" XDG_RUNTIME_DIR="$RT" \
  XDG_CONFIG_HOME="$S/cfg" XDG_STATE_HOME="$S/state" XDG_DATA_HOME="$S/data" XDG_CACHE_HOME="$S/cache" \
  ROOST_CONFIG="$S/config.conf" ROOST_STATE_DIR="$S/rstate" ROOST_SESSION_SOCKET_DIR="$RT/sess" \
  ROOST_TEST_USER_RUNTIME_DIR="$RT" ROOST_WL_WIDTH=1400 ROOST_WL_HEIGHT=900 SHELL=/bin/bash \
  tools/wayland/weston-run.sh target/debug/roost-iced &
C="env -i PATH=/usr/bin:/bin target/debug/roostctl --socket $RT/roost-iced/roost.sock"
for _ in $(seq 120); do $C identify >/dev/null 2>&1 && break; sleep 0.5; done
$C identify >/dev/null || { echo "UI never answered on $RT/roost-iced/roost.sock" >&2; exit 1; }
# check the log's "Selected: AdapterInfo" says Vulkan/llvmpipe, not GL
$C open --project mobile-client-android-tablet-experiments --cwd "$S/home"
$C tab report --tab <id> --source probe --session-id s --claim --lifecycle failed --title "Build failed"
$C screenshot --scale 2 --out "$S/shot-2x.png"
$C palette open --kind agents; $C screenshot --scale 2 --out "$S/palette-agents-2x.png"
# crop with tools/screenshot/pngtool.py crop; stop: kill the roost-iced pid you launched
```

On macOS, use the `mac-test` skill's dev-build recipe on the mini.

---

## Corrections (2026-11-08)

**To this note:**

- **The side-by-side table's "Bold / italic / dim: all three render
  properly" was wrong for dim.** At `474e6b5`, `Style` had only bold, italic
  and inverse, and nothing on the desktop has ever drawn faint.
- **Ghostling section:** Roost now uses the Kitty graphics calls (plan
  088).
- **Tier 2 listed Kitty inline images as something it unlocks.** They
  shipped without it.
- **cosmic-term section:** `monospace_fallback` "rescales them to whole
  cells" holds only for a buffer with a monospace width set, which
  `fill_text` never sets.
- **#592's body** says fallback sizing needs the raw path. It doesn't.

**To the 2026-11-08 research:**

- **Descender overlap on Linux** was put at about 2.8 px, from the hhea
  descent. Real descender ink (−180 units) crosses by under 1 px.
- **The Linux `⏺` route** names a font without the glyph, but the cascade
  lands on monochrome FreeSans on the Pop!_OS box, and Claude Code prints
  `●` off macOS. Low impact.
- **Claude Code's glyph set** was recalled; it is now read from the binary.
  The bullet differs per OS, and the spinner has three variants.
- **SGR 58 color:** libghostty gives no resolved underline color, so a
  palette color is a new cache input.
- **The full Ghostty sprite range list** attributed to `Face.zig` couldn't
  be confirmed by grep. Only the `draw/` file set above is verified.

**To the chrome research (2026-11-08 afternoon), found while verifying it:**

- **C1's cause.** The draft said the agents palette's project column is
  "never elided". It is, by 24 display columns
  (`PALETTE_AGENT_PROJECT_MAX_COLUMNS`), but a column budget doesn't bound a
  proportional font: "mobile-client-android" is 21 columns and 148 px in a
  140 px box. The fix is pixel elision, not adding elision.
- **C8's scope.** The draft proposed stripping spinner glyphs from chrome
  titles. The Agents panel and agents palette already do
  (`strip_leading_marker`). Only tab pills show the marker.
- **C3's lanes.** The draft said the pixel lanes would need new colors. They
  don't pin any changed role. `test_sidebar_pixels.py` pins the lifecycle
  **dot** colors, so lifts must go into separate text roles, not into
  `agent_failed`.
- **C3's selected-row count.** White at 0.7 reaches 4.73:1 on Roost's blue
  but only about 3.1:1 under a yellow macOS accent. Use the label's color.
- **Item 12's ranges.** Arrows U+2190–21FF also hold text-default emoji
  characters, and of Claude Code's spinner frames only ✳ is an emoji
  character. Counts are now from `emoji-data.txt`.
- **cosmic-text 0.16's `Hinting`** is layout snapping, not outline hinting,
  so it doesn't retire the swash delta.
- **Labels in `chrome-themes.md`.** The draft cited D1, D2, D5 and B1 there.
  That note's labels are now phases 0–4, size options S1–S4 (S2 is
  `ChromeMetrics`), key question 2 for the Linux default, and B1 for the
  appearance observers with the portal reader. This note uses those.
- **The companion-note citation** of `text-rendering.md:398-402` for hinting
  no longer exists in `chrome-themes.md`; nothing to fix.
- **#496** (GL fallback screenshots) is closed; it is cited only to say the
  capture didn't use that backend.
- **Zed's `calt: false`** is a default in its settings file; the reason
  isn't stated there.

---

## Key questions

1. **Should SGR decorations become their own issue and the next
   text-rendering plan?** They are the one item that is a visible defect
   rather than polish, and dim shows in every Claude Code session.
2. **Which decorations go in the first cut?** All of them, or faint,
   underline and strikethrough first, with the curly, dotted and dashed
   shapes waiting for item 3's antialiased route?
3. **SGR 58 with a palette color:** resolve it at draw time with the
   palette carried on the snapshot, or support RGB only at first?
4. **Is the tiny-skia image cache acceptable** for #590 and #591, or
   should antialiased sprites wait for a Tier 2 atlas that may never come?
5. **Is bundling Symbols Nerd Font acceptable,** after a license audit,
   at about 2.3 MB? Should Roost also bundle an emoji font on Linux, as
   Ghostty does?
6. **Before #592 is designed, which faces does macOS actually pick** for
   `✢ ✳ ✻ ✽ ⏺ ⎿` and for emoji? That needs a session on the mini.
7. **Should Linux keep 1.2 lines and floor,** gain only the `adjust-*`
   options, or move to `FontDerived`? That costs rows, as plan 073 did on
   macOS.
8. **Does the Mac Kitty aspect still reproduce?** File it only if it does.
9. **Should any config knobs ship,** and which? The original question
   about exposing `font-thicken` and hinting still stands.
10. **One mini session for all the Mac measurements?** Item 2a (which faces
    macOS picks), item 12 (color dingbats) and chrome C10 (Inter vs Finder)
    all need the same dev build and 2x captures.
11. **Should `docs/reference/fonts.md` list the known limits now,** before
    any fix: no decorations (item 1) and no ligatures (item 14)?

Chrome questions are in [Key questions (chrome)](#key-questions-chrome).

---

## Sequencing

1. **SGR decorations (item 1).** Standalone. It touches `roost-vt`
   (`Style`, `DrawCell`), the widget, and `tab.dump_resolved` plus a
   roosttest. The S part can ship alone.
2. **Small, independent:** braille sprites (item 4) and the Linux `⏺`
   comment fix (item 2d). They could ride with step 1.
3. **Measure on the mini (item 2a),** then the macOS fallback list (#592)
   and, if the spinner jitter is confirmed, Tier 1.5 sizing.
4. **The antialiased sprite route (item 3),** then the curly underline
   moves onto it, and powerline and Legacy Computing follow.
5. **Nerd Font bundling (item 5)** whenever the audit is done. It is
   independent of everything else.
6. **Linux metrics and the knobs (items 6 and 8)** only on demand.
7. **Tier 2** only on #309's trigger.

**Added in the afternoon:**
- Item 12 rides with step 3: measure it in the same mini session, then fold
  the ranges into #592's design. Item 13 can ride along.
- Item 14's doc line can ship with step 1 or any docs change.
- Chrome work runs in parallel and doesn't block any of this. Its order is
  in [Sequencing (chrome)](#sequencing-chrome) and
  [Where to start](#where-to-start-when-you-come-back).

---

## References and licenses

| Reference | Revision | License | How it was used |
|---|---|---|---|
| ghostty (`../thirdparty/ghostty`) | `eb895f612` (Roost pins `6ba14415` for libghostty-vt) | MIT | In depth: `src/font/{Metrics,Collection,SharedGridSet,embedded}.zig`, `src/font/sprite/draw/*`, `src/renderer/{generic,cell}.zig`, `src/config/Config.zig` |
| wezterm (`../thirdparty/wezterm`, shallow clone 2026-11-08) | `458a100` | MIT | `wezterm-gui/src/customglyph.rs`, `glyphcache.rs`, `assets/fonts/` |
| alacritty (`../thirdparty/alacritty`, shallow clone 2026-11-08) | `aa5d942` | Apache-2.0 or MIT | `builtin_font.rs`, coverage only |
| cosmic-text (registry 0.15.0, which Roost builds; `../thirdparty/cosmic-text` 0.19) | `59089955` | MIT or Apache-2.0 | In depth: fallback, system, shape, buffer, glyph_cache |
| iced_graphics, iced_core, iced_wgpu 0.14.0 (registry) | | MIT | `text.rs`, `image.rs`, the wgpu render order |
| swash (vendored `third_party/swash`) | 0.2.10 plus deltas | MIT or Apache-2.0 | Hinting delta, embolden |
| cosmic-term (`../thirdparty/cosmic-term`) | `9129277` | **GPL-3.0** | Not read again. Only this note's earlier conceptual summary is used; no code is quoted or paraphrased. |
| cmux (`../thirdparty/cmux`) | `a79cbde73ba` | **GPL-3.0-or-later** (its server excepted) | Not consulted |
| ghostling | `63842bf` | MIT | Not needed this round |
| Claude Code (installed binary 2.1.319) | | proprietary | Only the strings for its glyph and spinner sets were read |
| JetBrains Mono (bundled), Noto Color Emoji, Noto Sans CJK (system) | | OFL-1.1 | Font tables parsed for metrics only |
| Inter (bundled `third_party/inter`) | 4.1 (`4.001;git-9221beed3`) | OFL-1.1, no Reserved Font Name | Tables, features, coverage and digit advances via fontTools; `calt` and `kern` behavior via uharfbuzz |
| libcosmic (`../thirdparty/libcosmic`, shallow clone 2026-11-08; `iced` submodule not checked out) | `dd108aa` | MPL-2.0 | `src/widget/text.rs` type scale, `src/app/settings.rs:102`, `cosmic-theme/src/model/font.rs:9` |
| cosmic-text, chrome additions | 0.15.0 (registry); `59089955` (0.19) | MIT or Apache-2.0 | ASCII fast path `shape.rs:666-680` and `shape_skip` (0.15); fix `8cd21a31` and CHANGELOG 0.16.0 and 0.18.0; `Attrs::letter_spacing`, `font_features`; `wght` handling in `swash.rs`; `Hinting` in `layout.rs` |
| iced 0.14 (`iced_core`, `iced_graphics` 0.14.0, `iced_widget` 0.14.2; registry) | 0.14.0 is the newest on crates.io (2026-11-08) | MIT | `to_shaping`, `to_attributes`, text `draw` clipping, `default_text_size`, `crisp`, `rich_text` shaping |
| WezTerm, chrome additions | `458a100` | MIT | `docs/config/lua/config/{window_frame,command_palette_font,command_palette_font_size}.md`; `wezterm-char-props/data/emoji-data.txt` for item 12's counts |
| Zed (sparse clone of `assets/settings/default.json`) | `d2a4fb6` | app crates GPL-3.0-or-later; gpui Apache-2.0 | Setting names and defaults only (design level) |
| Open Sans, Noto Sans, Fira Sans, DejaVu Sans (system, Pop!_OS) | | OFL, Apache-2.0, Bitstream Vera | Metrics and coverage via fontTools and `fc-list` |
| Apple HIG text styles, macOS sidebar sizes, macOS accent colors, GNOME Adwaita Sans and libadwaita classes | | n/a | *From memory*, not re-read; marked inline |
| WCAG 2.x relative luminance and contrast formula | | W3C | Ratios computed from `ChromePalette` hex values, twice, independently |

---

## Original discovery (2026-09-27, references as of `474e6b5`)

### How Roost draws terminal text today (as of `474e6b5`)

**Pipeline.** Each non-blank cell is one `renderer.fill_text` call
(`crates/roost-iced/src/terminal_widget.rs:940`). From there iced's
standard text path takes over: iced_wgpu builds a cosmic-text buffer per
string, cryoglyph rasterizes it through swash into a glyph atlas, and a
shader blends it. Grouping cells into runs was tried and rejected (commit
`7681eaf`, "E4 no-go"): runs drift off the grid because cells use a
rounded advance while shaped text uses the fractional one.

| Setting | Today | Where |
|---|---|---|
| Point size | pt × 96/72, so 13pt draws at 17.33 px on every platform. Mac apps (and the Swift app) draw 13pt at 13 px. | `terminal_widget.rs:27` |
| Cell size | Width is the floor of the advance of "M"; height is the floor of 1.2 × size. | `terminal_widget.rs:28`, `:74-75` |
| Default font | `"JetBrains Mono, Monospace"` at 13pt on every platform, not bundled. If it's missing, cosmic-text maps the generic to "Noto Sans Mono", which macOS doesn't have, and falls back to an arbitrary monospace font. | `crates/roost-ui-model/src/typography.rs` |
| Shaping | `Shaping::Auto`: ASCII without shaping, everything else shaped. Each cell is shaped alone, so no ligatures. | `terminal_widget.rs` `cell_text` |
| Hinting | On. swash hints like FreeType's LCD mode. Core Text doesn't hint. | cosmic-text `swash.rs`; `third_party/swash/src/scale/mod.rs:400` |
| Blending | Linear light on an sRGB surface, because iced is built without `web-colors`. No gamma, contrast or stem-darkening correction. | `crates/roost-iced/Cargo.toml` iced features; `iced_graphics` `color.rs` |
| Antialiasing | Grayscale only. | cosmic-text `Format::Alpha` |
| Bold / italic | Real faces requested. cosmic-text 0.15 never fakes bold, and requesting italic from a family with no italic face can pick another family's italic. | `terminal_widget.rs:201-210` |
| Fallback | A hand-routed fix sends text-style symbols without VS16 to "STIX Two Math" on macOS and "DejaVu Sans" on Linux (commit `7f8c6de`, the ⏺ bug). Otherwise cosmic-text's fallback lists. | `terminal_widget.rs:105-184` |
| Draw order | Within a layer iced draws quads, then images, then text, so the selection tint and block cursor sit *under* the glyphs. The glyph under a block cursor isn't inverted. | iced_wgpu `lib.rs` |
| Box drawing | Geometry for U+2500–259F only. Curves and diagonals are stamped rectangles without antialiasing, snapped to logical pixels. | `crates/roost-ui-model/src/sprite.rs` |
| Software fallback | tiny-skia blends in sRGB space, so the same text looks different under the wgpu and tiny-skia renderers. | iced_tiny_skia `text.rs` |

**Performance.** libghostty's dirty rows let unchanged rows be reused on
the CPU side, but every frame re-sends every cell to the GPU and cryoglyph
re-prepares them all. The widget draw measured about 172 µs in release in
the worst case (commit `7681eaf`).

The vendored swash (`third_party/swash`) only carries malformed-font
guards; it doesn't change rendering output. (Plan 073 later added the
macOS hinting delta.)

### Side-by-side evidence (2026-09-27)

**Setup.** The installed v0.0.19 apps on the mac-mini, with identical
config: JetBrains Mono, `font-size = 13`, the GitHub Dark Default theme,
one tab each. The same sample text in both, captured with each app's own
in-process `roostctl screenshot` at 1x and 2x.

| What | Swift (Core Text) | iced |
|---|---|---|
| Size | 8 px cells, 18 px lines | 10 px cells, 20 px lines: 25% larger. 80 columns take 640 px vs 800 px. |
| Letter shapes | Smooth and unhinted, as designed | Snapped to the pixel grid: squarer "o", "b", "w", like hinted FreeType text on Linux |
| Stroke weight (2x, first line, normalized for size) | 24.2% of pixels inked, mean luminance delta 40.9 | 26.7% inked, delta 42.9: about 5% heavier |
| Bold / italic / dim | Bold and italic render as plain text (Swift only swaps a bold color and has no italic face) | Bold and italic render. **Dim does not, and neither does the sample's underline** (corrected 2026-11-08; the original said all three render). |
| Fallback symbols | ⏺ and ● the same size | ⏺ much smaller than ● |
| Box drawing, blocks, braille, powerline, CJK, emoji, colors | fine | fine |

**Reading it.** After size, the pixel-grid snapping from hinting is what
makes iced look like Linux text rather than Mac text. The weight
difference is modest, in the direction linear blending predicts for light
text on a dark background.

**Caveats.**

- Swift's in-process screenshot draws the view into a bitmap, which may
  skip Core Text's font smoothing, so on screen Swift may be slightly
  heavier. Its chrome also renders partly white in the capture, so only the
  terminal area is comparable.
- A true on-screen capture needs Screen Recording permission for the
  capturing process; `screencapture` over ssh produced nothing.

#### Capture recipe

Run on a Mac. With no arguments it captures both installed apps; pass
`roost` or `roost-iced` to capture one. Set `OUT` to keep before/after runs
apart. After the cutover, `roost` is the iced build under the production
identity, still reached with `--target mac`. Every `roostctl` call
is wrapped in a timeout that also fails if the command can't start: a
wedged socket (see #573) otherwise hangs forever.

For the 2026-11-08 items, add a decoration line and a symbol line to the
sample: `printf '\033[2mdim\033[0m \033[4:3m\033[58:2::255:0:0mcurl\033[0m \033[9mstrike\033[0m'`
and `'·✢✳✶✻✽ ⏺●⎿ ⠋⠙⣿ '`.

```bash
#!/bin/bash
# Usage: capture.sh [roost] [roost-iced]   (default: both)
# Production apps only; quits each app after capturing it.
set -u
T() { perl -e 'alarm 8; exec @ARGV or die "exec failed: $!\n"' "$@"; }
OUT=${OUT:-/tmp/roost-render-cmp}; mkdir -p "$OUT"

printf '%s\n' \
  'The quick brown fox jumps over the lazy dog  0123456789' \
  'fn main() { let x = a != b && c >= d; } // -> => ==  www' \
  "$(printf '\033[1mBold weight\033[0m  \033[3mItalic style\033[0m  \033[2mDim text\033[0m  \033[4munderline\033[0m')" \
  '╭──────┬──────╮  ▁▂▃▄▅▆▇█  ░▒▓  ⠿⣿' \
  '│ box  │ tree │    ✓ ✗ ⏺ ● →' \
  '╰──────┴──────╯  日本語テキスト 🙂🚀' \
  "$(printf '\033[48;5;24m\033[38;5;231m powerline \033[0m\033[38;5;24m\356\202\260\033[0m')" \
  "$(printf '\033[38;5;214mansi orange\033[0m \033[38;5;39mblue\033[0m \033[38;5;114mgreen\033[0m \033[38;5;203mred\033[0m')" \
  > "$OUT/sample.txt"

capture() { # $1 = roost | roost-iced
  local name app target
  case "$1" in
    roost)      name=Roost;      target=mac ;;
    roost-iced) name=Roost-Iced; target=iced ;;
    *) echo "unknown app: $1" >&2; return 1 ;;
  esac
  app="/Applications/$name.app"
  local ctl="$app/Contents/Resources/bin/roostctl"
  open -a "$app"
  for _ in $(seq 1 20); do
    T "$ctl" --target "$target" tab list --json >/dev/null 2>&1 && break
    perl -e 'select(undef,undef,undef,0.5)'
  done
  local tab
  tab=$(T "$ctl" --target "$target" tab list --json | jq -re '.projects[0].tabs[0].id') \
    || { echo "$1: no tab" >&2; osascript -e "quit app \"$name\""; return 1; }
  local rc=0
  T "$ctl" --target "$target" tab send --tab "$tab" \
    --bytes "clear; cat '$OUT/sample.txt'\n" || rc=1
  perl -e 'select(undef,undef,undef,2.5)'
  for s in 1 2; do
    [ "$rc" -eq 0 ] || break
    T "$ctl" --target "$target" screenshot --scale "$s" --out "$OUT/$1-${s}x.png" || rc=1
  done
  osascript -e "quit app \"$name\""
  return "$rc"
}

[ $# -eq 0 ] && set -- roost roost-iced
status=0
for a in "$@"; do capture "$a" || { echo "$a: capture failed" >&2; status=1; }; done
exit "$status"
```

**Measuring.** `tools/screenshot/pngtool.py crop` cuts matching regions
(the first text line, the symbol row). The weight number is the share of
pixels whose luminance differs from the background (`#0d1117`) by more than
8, and the mean luminance difference, over a box covering the first line.
Cropping the same line at the same scale keeps the comparison fair even
when cell sizes differ. Nearest-neighbor upscaling of the crops makes
hinting visible.

### What the references teach

#### Ghostty (`../thirdparty/ghostty`), the primary reference

Ghostty's macOS build uses Core Text for font discovery, shaping and
rasterization, and Metal for drawing. Its native look comes from, in order
of visual impact:

1. **Blending in display (gamma) space.** The `alpha-blending` option has
   three modes: `native` (the default on macOS), `linear`, and
   `linear-corrected` (the default elsewhere). Ghostty's own docs say
   `linear` "makes dark text look much thinner than normal and light text
   much thicker", and `linear-corrected` looks "nearly or completely
   identical to `native`" without its dark fringes. The corrected mode
   adjusts each pixel's alpha from the known foreground and background
   luminance (`src/renderer/shaders/shaders.metal`). **Roost at the time was
   in the uncorrected `linear` mode** (plan 073 then enabled `web-colors`).
2. **An unhinted Core Text mask.** Glyphs are drawn into an 8-bit
   alpha-only bitmap with font smoothing off by default, subpixel
   positioning on, and subpixel quantization off, with the fractional
   pixel offset baked into the bitmap (`src/font/face/coretext.zig`).
   `font-thicken` turns Core Text's smoothing on, which dilates strokes to
   look closer to system apps; `font-thicken-strength` tunes it. Ghostty's
   default is therefore lighter than Terminal.app.
3. **Whole-pixel metrics.** Cell width is the rounded maximum ASCII
   advance; cell height is the rounded ascent − descent + line gap, with
   the gap split above and below; the baseline is rounded and centered.
   Each glyph is copied 1:1 with nearest filtering (`src/font/Metrics.zig`).
   `adjust-cell-width`, `adjust-cell-height`, `adjust-baseline` and friends
   are user options.
4. **Fallback glyphs resized to match the primary font,** like CSS
   `font-size-adjust`: the first available of the width of 水, x-height,
   cap height, or line height (`src/font/Collection.zig`). Core Text's own
   `CTFontCreateForString` finds fallback fonts, so the system cascade is
   respected.

Also worth copying:

- **Sprites** drawn as geometry for box drawing, block elements, braille,
  powerline, branch symbols and the Legacy Computing ranges, plus
  underline, strikethrough and cursors (`src/font/sprite/`).
- **Emoji** fit to fill two cells, centered, with 2.5% padding.
- **Colors** converted to Display P3 with the drawing surface tagged P3,
  so they match other apps on P3 displays.
- **A pluggable face design.** Each backend provides metrics and a
  `renderGlyph` into the shared atlas, and the grid math only sees the
  metrics. That is what makes a Core Text rasterizer next to swash
  feasible.

#### cosmic-term and cosmic-text (`../thirdparty/cosmic-term`, `../thirdparty/cosmic-text`)

cosmic-term is GPL-3.0; this section describes its design only.
cosmic-term does nothing macOS-specific: its default font is Noto Sans
Mono, and it has no gamma handling or built-in box drawing. Its value is
structural:

- **One cosmic-text buffer per terminal,** one line per visible row,
  styles as attribute spans, drawn with a single `renderer.fill_raw` call.
  Stock iced 0.14 supports this path on wgpu and tiny-skia.
- **The raw path exposes settings iced's `fill_text` hides:** cosmic-text
  cache-key flags (`DISABLE_HINTING`, `FAKE_ITALIC`), font features (turn
  ligatures on or off), and exact numeric weights.
- **Useful cosmic-text features that already exist in 0.15:**
  `monospace_fallback` (prefers monospace fallback fonts; its rescale to
  whole cells needs a buffer monospace width, which only the raw path can
  set) and `shape-run-cache`. A custom fallback list can put Menlo, SF
  Mono and Apple Symbols ahead of the system UI font on macOS.
- **Separate weight settings** for normal, bold and dim text.
- **Keep Roost's own approach** for per-column glyph placement (cosmic-term
  uses cumulative advances, which drift with non-monospace fallback),
  integer cell width, and sprites.

**Version lock:** iced 0.14 pins cosmic-text 0.15. Later versions (0.16's
metrics-hinting option, 0.17's automatic fake italic) need an iced release
built on them. None of the versions through 0.19 change antialiasing,
gamma or emoji rendering.

#### Ghostling (`../thirdparty/ghostling`)

A single-file C demo on raylib with one embedded font and no shaping or
fallback, so nothing on text quality. It does draw **Kitty graphics**
(inline images) through libghostty's C API in about 100 lines. *2026-11-08:*
Roost now draws Kitty images through those calls (plan 088), and session
snapshots carry image state.

### Tuning options

#### Tier 0: small, try together

| Status | Option | Cost | Effect | Notes |
|---|---|---|---|---|
| Shipped, macOS | 1 pt = 1 px on macOS | tiny | Matches Mac convention, the Swift app, Ghostty and Terminal.app | Existing Roost-Iced users on macOS see text shrink 25% unless they raise `font-size`. Linux keeps 96/72. |
| Shipped, macOS | Hinting off on macOS | one line | Smooth, unhinted shapes like Core Text | Cheapest route: force `hint(false)` under `cfg(target_os = "macos")` in the vendored swash (`third_party/swash/src/scale/mod.rs:400`). That unhints chrome text too, as native Mac text is. It bends the vendored copy's "malformed-font guards only" rule; the cleaner route is the raw path's `DISABLE_HINTING` flag (Tier 1). |
| Shipped, both OSes | Enable iced's `web-colors` | one line plus a look at the chrome | Blending in display space, like AppKit and Ghostty's `native` | Solid colors stay the same; translucent chrome blends differently and needs a visual check. Also makes the wgpu and tiny-skia renderers agree. |
| Shipped, as the generic, not Menlo | Bundle JetBrains Mono as the default | small | Same look on every machine; no silent fallback to an arbitrary monospace font | Ghostty embeds it too. OFL license, like the bundled Inter. Also map the generic monospace family to Menlo on macOS. |
| Shipped | Apply the theme's bold color | small | Matches the theme author's intent | iced ignores `bold_color` today. |

#### Tier 1: medium, independent of each other

- **Shipped: cursor and selection above the glyphs.** Draw them in a later layer
  and invert the character under a block cursor. A gap you see every day.
  *Shipped:* an opaque block cursor that inverts its glyph, and Ghostty's
  selection colors (an opaque `selection-background`, text in
  `selection-foreground`).
  *Plan 074 shipped `cursor-text`:* the glyph under a block cursor takes
  the theme's `cursor-text` color (Ghostty parity), unless it is absent or
  equals the cursor fill, in which case it keeps the background. The
  `cell-foreground` / `cell-background` keywords are ignored.
- **Shipped (macOS only): Ghostty-style cell metrics:** round instead of floor, take line height
  and baseline from the font's own metrics, and add `adjust-cell-width` and
  `adjust-cell-height` options.
  *Shipped on macOS only, with Swift's rule rather than Ghostty's:* cells
  come from the font's own metrics, rounded **up**. **Still open:** Linux
  keeps floor and 1.2, and the `adjust-*` options don't exist (item 6
  above).
- **A macOS fallback list** with Menlo, Apple Symbols and SF Mono ahead of
  the system UI font, swapped in once at startup. Tracked in #592 (item 2
  above).
- **More sprite ranges:** powerline, braille and Legacy Computing (#591), with
  antialiased curves and diagonals and snapping to physical pixels (#590)
  (items 3 and 4 above; neither needs Tier 2).
- **The raw drawing path** (`fill_raw` with widget-owned cosmic-text
  buffers). It enables `DISABLE_HINTING`, synthesized italic for families
  without an italic face, font features and exact weights. It is also a
  step toward Tier 2's per-frame savings. It must keep per-column placement
  to avoid the E4 drift (item 9 above).
- *Added 2026-11-08:* **SGR decorations** (item 1 above). The Tier list
  missed them because the side-by-side table said dim rendered.

**Known limitation (plan 073).** A glyph drawn in a fallback face
(CJK, emoji) is centered by its own metrics, so its baseline isn't the
primary font's. This is part of the fallback-glyph sizing above, which is
still open (#585, item 2c above).

#### Tier 2: a terminal renderer of Roost's own

An iced shader widget with its own glyph atlas and an instanced cell
shader, rasterizing with swash directly, placing every glyph by column,
and drawing backgrounds, glyphs, decorations and the cursor in explicit
passes. About one to two plans. It unlocks:

- hinting fully under Roost's control;
- Ghostty's `linear-corrected` blending, using each cell's known
  background;
- optional stroke thickening (swash's embolden) as a stand-in for
  `font-thicken`;
- fallback glyphs resized to the primary font, and emoji fitted to two
  cells (*2026-11-08:* mostly reachable without it; see item 2c);
- ligatures as an option, shaped per run but snapped to the grid;
- ~~Kitty inline images~~ (shipped without Tier 2 in plan 088);
- Display P3 surface tagging on macOS;
- far less per-frame work: only changed cells are re-uploaded.

The tiny-skia software renderer would need a CPU path or keep today's
per-cell drawing as its fallback.

#### Tier 3: a Core Text rasterizer on macOS

Behind a rasterizer trait, next to swash: create the font from the same
bytes, draw each glyph with `CTFontDrawGlyphs` into an alpha-only
`CGBitmapContext` using Ghostty's flags, and put the mask in Tier 2's
atlas. Shaping stays in cosmic-text, so glyph ids match. Crates:
`objc2-core-text`, `objc2-core-graphics`. **Needs Tier 2 first**, because
today cryoglyph rasterizes internally. Only worth it if Tiers 0–2 don't
satisfy.

### Suggested order (2026-09-27)

1. Tier 0 together: the size factor, hinting off on macOS, `web-colors`.
   Re-shoot with the capture recipe and use it daily for a while.
2. Cursor and selection above the glyphs (Tier 1).
3. Fallback sizing (#585) and the macOS fallback list (#592).
4. Decide on Tier 2 from the results and from performance needs.

The size change fits naturally with the identity cutover in
[`swift-retirement.md`](swift-retirement.md): users coming from the Swift
app already expect 13pt to render at 13 px.

Steps 1 and 2 shipped in plan 073, ahead of the cutover. The 2026-11-08
[Sequencing](#sequencing) replaces steps 3 and 4.

### Findings from plan 073

Checked against the code at `5c01a9b`, 2026-10-02.

- **Swift rounds cells up; Ghostty rounds to nearest.** Swift's cell width
  is the ceiling of the advance of "M", and its height the ceiling of
  ascent − descent + leading (`TerminalView.swift:462-466`). Ghostty rounds.
  For JetBrains Mono (upm 1000, advance 600, ascent 1020, descent −300):

  | Size | Swift (ceil) | Ghostty (round) | iced before 073 |
  |---|---|---|---|
  | 13 px | 8×18 | 8×17 | |
  | 14 px | 9×19 | 8×18 | |
  | 13pt at 17.33 px | | | 10×20 |

  Side-by-side parity with Swift won the choice, so plan 073 uses ceil, on
  macOS only. The baseline sits at cell top + ascent and the slack goes to
  the bottom, as in Swift. *2026-11-08:* with Swift retired, round-to-nearest
  (Ghostty) is now an open choice too; it would make macOS cells one px
  narrower at 14pt (8 instead of 9).
- **Swift's default is 14pt and SF Mono.** Swift's unset `font-size` is 14
  (`App.swift:346,493,502`), and with `font-family` unset it uses the system
  monospace font (SF Mono). iced's default was 13pt and `"JetBrains Mono,
  Monospace"`. After plan 073 the macOS default size is 14 and Linux stays
  13, each following its own OS's convention. The generic monospace family
  maps to the bundled JetBrains Mono on both OSes, not to Menlo as the Tier
  0 row first proposed: that mapping assumed the font wasn't bundled. Users
  who never set `font-family` now get JetBrains Mono instead of whatever was
  installed, which is a release note, not a regression.
- **The draw-order note, corrected.** The cursor and selection are drawn
  *after* the text in the widget, but iced paints every quad in a layer
  before any text, so they landed under the glyphs regardless. The fix is
  not "draw later": rows are sparse, so a cursor on a blank cell gets no
  quad, and sprites are quads too. The order is now pinned as cell
  backgrounds, selection, cursor, then sprites and glyphs in role colors.
- **The default theme hides the bold color.** `roost-dark` sets its
  foreground, `bold-color` and `selection-foreground` all to `#ffffff`, so
  no shipped theme shows a bold-color difference unless something recolors
  the default foreground with OSC 10 (base16 and tinted-shell scripts do).
  Headless dumps keep the plain foreground.
- **Unhinting is a swash delta, and it reaches chrome text.** The vendored
  swash turns hinting off on macOS, which also unhints the sidebar and tab
  strip, as native Mac text is. It goes away only when terminal and chrome
  text can both turn hinting off, or when iced or cosmic-text expose the
  flag (Tier 1's raw path would do it).

---

## Constraints

- iced 0.14 pins cosmic-text 0.15; newer cosmic-text needs an iced upgrade.
- Grouping cells into runs drifted off the grid (commit `7681eaf`). Any
  run-based drawing must place glyphs per column.
- The tiny-skia fallback blends differently from wgpu, and under the GL
  fallback renderer `roostctl screenshot` can return geometry without text
  (#496). Check which renderer is in use before judging a capture.
- Changes to the vendored swash should stay documented in its
  `README.roost.md`, including their removal condition.
- One config file serves both platforms, so per-platform defaults (size
  factor, hinting) must be explicit and documented in
  `docs/reference/fonts.md`.
- *Added 2026-11-08:*
  - **The `RenderedRow` cache.** Anything new that `RenderedRow::build`
    reads must be added to the guards in `refresh_snapshot`
    (`crates/roost-iced/src/app/terminal_tab.rs:1810-1815`).
  - **Wire compatibility.** `tab.dump_resolved` fields are wire format,
    with one-back compatibility.
  - **No per-image tint.** iced images take no tint, so any sprite drawn
    as an image bakes its color into the cache key.

---

## Open questions (2026-09-27)

These predate the [Key questions](#key-questions) above, which are the
current ones.

- ~~How to introduce the macOS size change for existing Roost-Iced users.~~
  **Settled:** the CHANGELOG notice. No one-time in-app notice.
- ~~What `web-colors` does to translucent chrome.~~ **Partly settled:** it
  shipped on **both** OSes, and exact-color pixel probes still pass. Whether
  translucent chrome looks right is Charlie's V2 verdict; if he rejects it, the fallback is `web-colors` on macOS
  only, a small follow-up.
- Whether to get Screen Recording permission for one capturing tool on the
  mini, for true on-screen comparisons.
- ~~Whether Linux should stay hinted.~~ **Settled: yes.** Hinting is a
  per-platform default, off only on macOS.
- Whether to expose `font-thicken`-style and hinting options to users, or
  keep them as platform defaults.
