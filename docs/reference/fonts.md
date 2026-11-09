# Fonts

Roost reads font settings from `~/.config/roost/config.conf` (more
precisely `$XDG_CONFIG_HOME/roost/config.conf`) on both platforms. Family
changes take effect on the next launch — same model as themes. Font
*size* also responds to runtime hotkeys, which resize every tab and save
the new size to `font-size`; see
[Keybindings](../getting-started/keybindings.md#font-sizing).

```conf
# ~/.config/roost/config.conf
font-family = "JetBrains Mono"
font-size = 13
```

Keys use Ghostty-style hyphens (`font-family`, not `font_family`); a
misspelled key is ignored, with a warning in the log that names the key
and, for a slip like `font_size`, the key you likely meant.

## Available settings

| Key           | Default                 | Effect                                                                                 |
|---------------|-------------------------|----------------------------------------------------------------------------------------|
| `font-family` | `JetBrains Mono, Monospace`, which is always [JetBrains Mono](#the-bundled-terminal-font) | Terminal cell font. Quote values containing spaces (`"JetBrains Mono"`). |
| `font-size`   | `13` (Linux) / `14` (macOS) | Point size for the terminal font (see [Font size](#font-size)). Must be `> 0`. Adjustable at runtime via `Cmd-+` / `Cmd--` (`Alt-+` / `Alt--` on Linux), which resizes every tab and saves the new value here. |

### How `font-family` resolves

The value is a comma-separated fallback chain, matched
case-insensitively against the installed families left-to-right; the
first installed one wins. `monospace` anywhere in the chain (and an
unmatched chain) resolves to the `Monospace` generic
(`resolve_family_name` in `crates/roost-ui-model/src/typography.rs`),
which the UI maps to its bundled JetBrains Mono. An unresolvable family
never stops Roost from launching: it falls back to the bundled
JetBrains Mono.

## Font size

`font-size` is in points, and each OS turns points into pixels its own
way, so a size matches that OS's other terminals:

- **macOS:** one point is one pixel, as in AppKit. The width of a cell
  is the "M" advance and the height is the font's ascent + descent +
  line gap, each rounded up. JetBrains Mono at 13 gives 8×18 cells.
  The default is `14`.
- **Linux:** 96 dpi, so `13` draws at about 17.3 px. A cell is
  the "M" advance wide and 1.2 lines tall, each rounded down. The
  default is `13`.

## The bundled terminal font

The iced UI carries [JetBrains Mono](https://www.jetbrains.com/lp/mono/)
v2.304 (Regular, Bold, Italic, Bold Italic) inside its binary
(`third_party/jetbrains-mono`, loaded by `crates/roost-iced/src/fonts.rs`).
So the default `font-family` renders the same font on every machine,
whether or not JetBrains Mono is installed, and the picker always lists
it. A family you name that is installed resolves as before, and a
system-wide JetBrains Mono install coexists with the bundled copy.

Before the font was bundled, an unset `font-family` on a machine without
JetBrains Mono fell back to whatever monospace font the system offered.
Those machines now show JetBrains Mono; set `font-family` to keep the
font you had.

## Picking a font from the UI

**Select a font…** is in the command palette
(`Cmd-Shift-P` / `Alt-Shift-P`). It lists the monospaced families the
system reports (plus the bundled JetBrains Mono),
previews the highlighted one live, and writes the choice back to
`config.conf` as a `font-family =` line when you confirm — so the picker
and the config file are the same setting, not two.

## Chrome vs. terminal cells

Only the **terminal cell font** is configurable. The window chrome —
sidebar rows, tab pill labels, palette rows — uses its own font,
[Inter](https://rsms.me/inter/) v4.1, bundled into the
  binary (`third_party/inter`, embedded by `crates/roost-iced/src/main.rs`
  and served through the single `chrome::chrome_font()` seam). Bundling it
  makes the chrome render identically on every distro and OS instead of
  inheriting whatever the desktop's default sans happens to be.

## Limitations

- **Only family and size are configurable.** There is no separate bold or
  italic family, no OpenType feature list, no hinting/antialias knobs and
  no cell-metric adjusters. Earlier Roost builds documented a set of
  Cairo/Pango-era keys (`font_family_bold`, `font_feature`,
  `hint_metrics`, `hint_style`, `antialias`, `adjust_cell_width`,
  `adjust_cell_height`, `adjust_font_baseline`, `font_thicken`) — those
  belonged to the retired GTK renderer and no shipping UI reads them.
  Leaving them in your config is harmless: unknown keys are dropped, with
  a warning in the log.
- **Chrome fonts are not configurable.** See above.
- **Family changes need a relaunch.** Only `Cmd-+` / `Cmd--` / `Cmd-0`
  (`Alt-` on Linux) rescale live, and they rescale every tab.
