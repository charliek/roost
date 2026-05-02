# Fonts

Roost reads font settings from `~/.config/roost/config.conf` (more precisely `$XDG_CONFIG_HOME/roost/config.conf`) on both platforms. Font family, features, and Cairo rendering options take effect on the next launch — same model as themes. Font *size* also responds to runtime hotkeys per tab; see [Keybindings](../getting-started/keybindings.md#font-sizing).

```conf
# ~/.config/roost/config.conf
font_family = JetBrains Mono, Iosevka, Monaco, monospace
font_size = 13
font_feature = -calt
```

## Available settings

| Key                | Default                                   | Effect                                                                                  |
|--------------------|-------------------------------------------|-----------------------------------------------------------------------------------------|
| `font_family`      | `JetBrains Mono, Monaco, monospace`       | Comma-separated family list. The first installed family wins. The fallback chain matters because Pango's macOS fallback is unreliable when the head of the list is missing. |
| `font_size`        | `12`                                      | Point size. Adjustable per-tab at runtime via `Cmd-+` / `Cmd--` (`Ctrl-+` / `Ctrl--` on Linux). |
| `font_family_bold` | (inherits `font_family`)                  | Override family used for bold text. Useful when pairing fonts (e.g. Iosevka regular + Berkeley Mono Bold). When unset, Pango synthesizes bold from the regular family. |
| `font_feature`     | (none)                                    | OpenType feature tag. Repeatable: each line appends one entry. Joined with commas at render time. |
| `hint_metrics`     | `on`                                      | One of `on`, `off`, `default`. Snaps glyph advance widths to integer pixels. Keep `on` for monospace crispness — without it, cells look soft. |
| `hint_style`       | `none` (macOS) / `slight` (Linux)         | One of `none`, `slight`, `medium`, `full`, `default`. macOS fonts are not designed for hinting; FreeType `slight` is the typical Linux setup. |
| `antialias`        | `gray`                                    | One of `none`, `gray`, `subpixel`, `default`. macOS uses grayscale natively; `subpixel` is meaningful on RGB-stripe LCDs (most desktop monitors on Linux). |

Empty string and `default` both mean "use the platform default for this setting."

## Cell tuning

Four additional knobs adjust the cell grid and glyph rendering. Useful when matching the look of another terminal (cmux, ghostty, iTerm) where the natural Pango/Cairo metrics feel a touch tight or thin. All take effect on the next launch.

| Key                    | Value syntax                              | Effect                                                                                  |
|------------------------|-------------------------------------------|-----------------------------------------------------------------------------------------|
| `adjust_cell_height`   | `2`, `2px`, `10%`, `-1`, `-5%`            | Add or subtract from the natural cell height. Positive values add line spacing; glyphs auto-center in the enlarged cell. |
| `adjust_cell_width`    | same syntax                               | Add or subtract from the natural cell width (letter spacing).                           |
| `adjust_font_baseline` | same syntax                               | Shift glyphs vertically inside the cell. A fine-tune *after* `adjust_cell_height` — leave it unset until you need to bias the glyph up or down. |
| `font_thicken`         | `true` / `false` (default `false`)        | Render each glyph twice with a 0.5 px horizontal offset, fattening strokes. Approximates Apple Core Text stem darkening for pipelines that don't apply it natively (notably Cairo on macOS). Not a perfect parity with Apple's algorithm. |

A bare integer means pixels (`2` is the same as `2px`). A trailing `%` means a signed percentage of the natural metric. Negative values shrink. The cell metrics are clamped to a minimum of 1 px so a runaway negative can't crash the geometry.

### Targeting the cmux look on macOS

The default config aims at ghostty-style polish with modern programming fonts. To get closer to cmux/Terminal.app's look — Menlo at a smaller size with a touch more line spacing and Apple-like stem weight — try:

```conf
font_family = Menlo
font_size = 11
adjust_cell_height = 2px
font_thicken = true
```

Eyeball alongside cmux and adjust `adjust_cell_height` and `font_size` to taste.

## Tuning for crisp text

The defaults aim at the cmux/ghostty look: cell-snapped metrics, grayscale AA, light-or-no hinting depending on platform. Tweak from there:

- If text looks soft, verify `hint_metrics = on` is set (or left as default).
- On Linux with a standard RGB-stripe panel, try `antialias = subpixel`.
- To disable programming ligatures: `font_feature = -calt`.
- To stack multiple OpenType features, add additional `font_feature` lines:

  ```conf
  font_feature = -calt
  font_feature = +ss01
  font_feature = +cv01
  ```

- Pair fonts when the regular weight you like has a thin bold:

  ```conf
  font_family = Iosevka SS04
  font_family_bold = Berkeley Mono Bold
  ```

## Limitations

- **Italic family is not configurable yet.** `font_family_italic` is reserved.
- **Cursor / underline / strikethrough thickness adjusters are not exposed.** Only the cell, baseline, and stem-thicken knobs land here; the TUI-alignment family of `adjust_cursor_*`, `adjust_underline_*`, `adjust_strikethrough_*`, and `adjust_box_thickness` are deferred.
- **Sidebar and tab-label fonts use GTK's UI font.** Only the terminal cell font is configurable.
- **All restart-required except size hotkeys.** `Cmd-+/-/0` rescales live; every other knob (family, features, AA, hint, cell adjusters, font-thicken) takes effect on next launch.
- **Cairo font option control is implemented via a small cgo wrapper** (`internal/pangoextra`) because gotk4's `pangocairo.ContextSetFontOptions` binding crashes. See [Architecture](architecture.md) for the package layout.
