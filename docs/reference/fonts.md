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
- **Ghostty's `adjust-cell-*` metric tweaks are not exposed.** Cell width / height / cursor thickness adjustments come later if needed.
- **Sidebar and tab-label fonts use GTK's UI font.** Only the terminal cell font is configurable.
- **`hint_metrics`, `hint_style`, and `antialias` require restart.** Only size responds to runtime hotkeys.
- **Cairo font option control is implemented via a small cgo wrapper** (`internal/pangoextra`) because gotk4's `pangocairo.ContextSetFontOptions` binding crashes. See [Architecture](architecture.md) for the package layout.
