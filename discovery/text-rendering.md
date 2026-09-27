# Terminal text rendering in iced (discovery)

Status: **discovery**, an options list for tuning, not a commitment.
Written 2026-09-27. Companion to [`swift-retirement.md`](swift-retirement.md);
neither blocks the other. File and line references are as of `main` @
`474e6b5`.

iced's terminal text is good. The question here is which knobs exist to
make it better, especially on macOS once the Core Text-based Swift app is
gone, and what each costs.

---

## Summary

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

## How Roost draws terminal text today

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
guards; it doesn't change rendering output.

---

## Side-by-side evidence (2026-09-27)

**Setup.** The installed v0.0.19 apps on the mac-mini, with identical
config: JetBrains Mono, `font-size = 13`, the GitHub Dark Default theme,
one tab each. The same sample text in both, captured with each app's own
in-process `roostctl screenshot` at 1x and 2x.

| What | Swift (Core Text) | iced |
|---|---|---|
| Size | 8 px cells, 18 px lines | 10 px cells, 20 px lines: 25% larger. 80 columns take 640 px vs 800 px. |
| Letter shapes | Smooth and unhinted, as designed | Snapped to the pixel grid: squarer "o", "b", "w", like hinted FreeType text on Linux |
| Stroke weight (2x, first line, normalized for size) | 24.2% of pixels inked, mean luminance delta 40.9 | 26.7% inked, delta 42.9: about 5% heavier |
| Bold / italic / dim | Bold and italic render as plain text (Swift only swaps a bold color and has no italic face) | All three render properly |
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

### Capture recipe

Run on a Mac. With no arguments it captures both installed apps; pass
`roost` or `roost-iced` to capture one. Set `OUT` to keep before/after runs
apart. After the cutover, `roost` is the iced build under the production
identity, still reached with `--target mac`. Every `roostctl` call
is wrapped in a timeout that also fails if the command can't start: a
wedged socket (see #573) otherwise hangs forever.

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

---

## What the references teach

### Ghostty (`../thirdparty/ghostty`), the primary reference

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
   luminance (`src/renderer/shaders/shaders.metal`). **Roost today is in
   the uncorrected `linear` mode.**
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

### cosmic-term and cosmic-text (`../thirdparty/cosmic-term`, `../thirdparty/cosmic-text`)

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
  `monospace_fallback` (prefers monospace fallback fonts and rescales them
  to whole cells) and `shape-run-cache`. A custom fallback list can put
  Menlo, SF Mono and Apple Symbols ahead of the system UI font on macOS.
- **Separate weight settings** for normal, bold and dim text.
- **Keep Roost's own approach** for per-column glyph placement (cosmic-term
  uses cumulative advances, which drift with non-monospace fallback),
  integer cell width, and sprites.

**Version lock:** iced 0.14 pins cosmic-text 0.15. Later versions (0.16's
metrics-hinting option, 0.17's automatic fake italic) need an iced release
built on them. None of the versions through 0.19 change antialiasing,
gamma or emoji rendering.

### Ghostling (`../thirdparty/ghostling`)

A single-file C demo on raylib with one embedded font and no shaping or
fallback, so nothing on text quality. It does draw **Kitty graphics**
(inline images) through libghostty's C API in about 100 lines. Roost's
pinned libghostty header already exposes those calls (about 50 symbols),
and Roost doesn't use them. Host-session snapshots don't carry Kitty image
state either (a known limitation in `docs/development/host-sessions.md`).

---

## Tuning options

### Tier 0: small, try together

| Option | Cost | Effect | Notes |
|---|---|---|---|
| 1 pt = 1 px on macOS | tiny | Matches Mac convention, the Swift app, Ghostty and Terminal.app | Existing Roost-Iced users on macOS see text shrink 25% unless they raise `font-size`. Linux keeps 96/72. |
| Hinting off on macOS | one line | Smooth, unhinted shapes like Core Text | Cheapest route: force `hint(false)` under `cfg(target_os = "macos")` in the vendored swash (`third_party/swash/src/scale/mod.rs:400`). That unhints chrome text too, as native Mac text is. It bends the vendored copy's "malformed-font guards only" rule; the cleaner route is the raw path's `DISABLE_HINTING` flag (Tier 1). |
| Enable iced's `web-colors` | one line plus a look at the chrome | Blending in display space, like AppKit and Ghostty's `native` | Solid colors stay the same; translucent chrome blends differently and needs a visual check. Also makes the wgpu and tiny-skia renderers agree. |
| Bundle JetBrains Mono as the default | small | Same look on every machine; no silent fallback to an arbitrary monospace font | Ghostty embeds it too. OFL license, like the bundled Inter. Also map the generic monospace family to Menlo on macOS. |
| Apply the theme's bold color | small | Matches the theme author's intent | iced ignores `bold_color` today. |

### Tier 1: medium, independent of each other

- **Cursor and selection above the glyphs.** Draw them in a later layer
  and invert the character under a block cursor. A gap you see every day.
- **Ghostty-style cell metrics:** round instead of floor, take line height
  and baseline from the font's own metrics, and add `adjust-cell-width` and
  `adjust-cell-height` options.
- **A macOS fallback list** with Menlo, Apple Symbols and SF Mono ahead of
  the system UI font, swapped in once at startup.
- **More sprite ranges:** powerline, braille and Legacy Computing, with
  antialiased curves and diagonals and snapping to physical pixels.
- **The raw drawing path** (`fill_raw` with widget-owned cosmic-text
  buffers). It enables `DISABLE_HINTING`, synthesized italic for families
  without an italic face, font features and exact weights. It is also a
  step toward Tier 2's per-frame savings. It must keep per-column placement
  to avoid the E4 drift.

### Tier 2: a terminal renderer of Roost's own

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
  cells;
- ligatures as an option, shaped per run but snapped to the grid;
- Kitty inline images;
- Display P3 surface tagging on macOS;
- far less per-frame work: only changed cells are re-uploaded.

The tiny-skia software renderer would need a CPU path or keep today's
per-cell drawing as its fallback.

### Tier 3: a Core Text rasterizer on macOS

Behind a rasterizer trait, next to swash: create the font from the same
bytes, draw each glyph with `CTFontDrawGlyphs` into an alpha-only
`CGBitmapContext` using Ghostty's flags, and put the mask in Tier 2's
atlas. Shaping stays in cosmic-text, so glyph ids match. Crates:
`objc2-core-text`, `objc2-core-graphics`. **Needs Tier 2 first**, because
today cryoglyph rasterizes internally. Only worth it if Tiers 0–2 don't
satisfy.

---

## Suggested order

1. Tier 0 together: the size factor, hinting off on macOS, `web-colors`.
   Re-shoot with the capture recipe and use it daily for a while.
2. Cursor and selection above the glyphs (Tier 1).
3. Fallback sizing and the macOS fallback list.
4. Decide on Tier 2 from the results and from performance needs.

The size change fits naturally with the identity cutover in
[`swift-retirement.md`](swift-retirement.md): users coming from the Swift
app already expect 13pt to render at 13 px.

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

---

## Open questions

- How to introduce the macOS size change for existing Roost-Iced users:
  with the cutover release notes, or with a one-time notice.
- What `web-colors` does to translucent chrome (overlays, hover states).
  It may bring it closer to the Swift look, since AppKit blends the same
  way; it needs checking.
- Whether to get Screen Recording permission for one capturing tool on the
  mini, for true on-screen comparisons.
- Whether Linux should stay hinted. Probably yes: hinted text is the Linux
  convention, so hinting becomes a per-platform default rather than a
  global change.
- Whether to expose `font-thicken`-style and hinting options to users, or
  keep them as platform defaults.
