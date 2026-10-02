# Fonts

Roost reads font settings from `~/.config/roost/config.conf` (more
precisely `$XDG_CONFIG_HOME/roost/config.conf`) on both platforms. Family
changes take effect on the next launch — same model as themes. Font
*size* also responds to runtime hotkeys per tab; see
[Keybindings](../getting-started/keybindings.md#font-sizing).

```conf
# ~/.config/roost/config.conf
font-family = "JetBrains Mono"
font-size = 13
```

Keys use Ghostty-style hyphens (`font-family`, not `font_family`); a
misspelled key is silently ignored.

## Available settings

| Key           | Default                 | Effect                                                                                 |
|---------------|-------------------------|----------------------------------------------------------------------------------------|
| `font-family` | Swift `Roost.app`: the system monospaced font. iced UI (Linux, and `Roost-Iced.app` on macOS): `JetBrains Mono, Monospace`, which is always [JetBrains Mono](#the-bundled-terminal-font) | Terminal cell font. Quote values containing spaces (`"JetBrains Mono"`). |
| `font-size`   | `13` (Linux) / `14` (macOS) | Point size for the terminal font. Must be `> 0`. Adjustable per tab at runtime via `Cmd-+` / `Cmd--` (`Alt-+` / `Alt--` on Linux). |

### How `font-family` resolves

The two UIs resolve the value differently, so a config file that has to
work on both should name a single installed family:

- **Linux (iced).** The value is a comma-separated fallback chain, matched
  case-insensitively against the installed families left-to-right; the
  first installed one wins. `monospace` anywhere in the chain (and an
  unmatched chain) resolves to the `Monospace` generic
  (`resolve_family_name` in `crates/roost-ui-model/src/typography.rs`),
  which the iced UI maps to its bundled JetBrains Mono. The same rules
  apply to `Roost-Iced.app` on macOS.
- **macOS (Swift).** The value is a single family name handed to
  `NSFont(name:size:)`. An unknown or empty name falls back to
  `NSFont.monospacedSystemFont` — a comma-separated *list* is not parsed,
  so it will not match a family and you get the system monospace instead.

Either way an unresolvable family never stops Roost from launching: the
iced UI falls back to its bundled JetBrains Mono, and the Swift app to the
system monospace.

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

Both UIs expose **Select a font…** in the command palette
(`Cmd-Shift-P` / `Alt-Shift-P`). It lists the monospaced families the
system reports (plus the bundled JetBrains Mono in the iced UI),
previews the highlighted one live, and writes the choice back to
`config.conf` as a `font-family =` line when you confirm — so the picker
and the config file are the same setting, not two.

## Chrome vs. terminal cells

Only the **terminal cell font** is configurable. The window chrome —
sidebar rows, tab pill labels, palette rows — uses its own font:

- **macOS:** the system UI font.
- **Linux (iced):** [Inter](https://rsms.me/inter/) v4.1, bundled into the
  binary (`third_party/inter`, embedded by `crates/roost-iced/src/main.rs`
  and served through the single `chrome::chrome_font()` seam). Bundling it
  makes the chrome render identically on every distro instead of
  inheriting whatever the desktop's default sans happens to be.

## Limitations

- **Only family and size are configurable.** There is no separate bold or
  italic family, no OpenType feature list, no hinting/antialias knobs and
  no cell-metric adjusters. Earlier Roost builds documented a set of
  Cairo/Pango-era keys (`font_family_bold`, `font_feature`,
  `hint_metrics`, `hint_style`, `antialias`, `adjust_cell_width`,
  `adjust_cell_height`, `adjust_font_baseline`, `font_thicken`) — those
  belonged to the retired GTK renderer and no shipping UI reads them.
  Leaving them in your config is harmless; unknown keys are dropped.
- **Chrome fonts are not configurable.** See above.
- **Family changes need a relaunch.** Only `Cmd-+` / `Cmd--` / `Cmd-0`
  (`Alt-` on Linux) rescale live, per tab.
