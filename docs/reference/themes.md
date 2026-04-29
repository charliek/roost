# Themes

Roost ships with a small set of bundled color themes. Pick one with `theme = NAME` in `config.conf`. Themes control colors only; font choice stays on `font_family` / `font_size`.

```conf
# ~/.config/roost/config.conf
theme = Dracula+
```

The setting takes effect on the next launch (themes are not hot-reloaded today). Unknown names fall back to `roost-dark` with a warning logged at startup.

## Bundled themes

| Name                | Style                                                       |
|---------------------|-------------------------------------------------------------|
| `roost-dark`        | The default. Roost's built-in dark palette (cmux-derived).  |
| `Dracula+`          | Higher-contrast Dracula variant with `cursor-text` set.     |
| `Dracula`           | The original Dracula palette.                               |
| `Catppuccin Mocha`  | Pastel dark; the "mocha" Catppuccin flavor.                 |
| `Gruvbox Dark Hard` | Warm, high-contrast retro palette.                          |
| `TokyoNight`        | Cool blues + softer accent colors.                          |
| `Atom One Dark`     | Atom editor's familiar dark scheme.                         |

The six themes other than `roost-dark` are byte-identical copies of files from [Ghostty's bundled set](https://github.com/ghostty-org/ghostty/tree/main/src/config/themes). If you want a Ghostty theme that's not on this list, it usually parses cleanly in roost — see *File format* below.

## File format

Theme files are plain key=value text with `#` comments — the same syntax as `config.conf` (and as Ghostty's themes). Roost honors:

| Key                    | Required | Effect                                                                |
|------------------------|----------|-----------------------------------------------------------------------|
| `palette = N=#RRGGBB`  | yes (0–15) | One of the 16 ANSI colors. Indices 16–255 are computed (xterm 6×6×6 cube + 24-step gray ramp). |
| `background`           | yes      | Default background color.                                             |
| `foreground`           | yes      | Default foreground color.                                             |
| `cursor-color`         | yes      | Cursor block fill.                                                    |
| `cursor-text`          | no       | Glyph color when the cursor sits over a non-empty cell. Defaults to `background`. |
| `bold-color`           | no       | Color for bold text drawn with the *default* foreground. Bold-with-explicit-color (e.g. bold red) is unaffected. Defaults to `foreground`. |
| `selection-background` | no       | Selection overlay color (rendered at 35% alpha).                       |
| `selection-foreground` | no       | Parsed but currently unused (see *Limitations*).                       |

Unknown keys are silently ignored. A theme file is typically ~22 lines.

Example (the bundled `Dracula+`):

```conf
palette = 0=#21222c
palette = 1=#ff5555
palette = 2=#50fa7b
# ... palette = 3..14 ...
palette = 15=#f8f8f2
background = #212121
foreground = #f8f8f2
cursor-color = #eceff4
cursor-text = #282828
selection-background = #f8f8f2
selection-foreground = #545454
```

## Limitations

- **No user override directory.** Only the bundled themes are loadable. To use a theme that isn't bundled, drop the file into `cmd/roost/themes/` and rebuild — the directory is embedded into the binary at compile time.
- **No hot-reload.** Theme is loaded once when each tab is created. To switch, edit `config.conf` and restart roost.
- **`selection-foreground` is parsed but not yet rendered.** Roost's selection is currently a 35%-alpha overlay over the existing text; honoring `selection-foreground` requires switching to opaque selection + re-rendering glyphs, which is a render-order change tracked separately.
- **Indices 16–255 of the palette are not customizable** — they're filled by the standard xterm formula. Bundled themes only set indices 0–15, matching every Ghostty theme.

## Trying a Ghostty theme that isn't bundled

The format is identical, so you can usually copy a file in:

```bash
cp "/Applications/Ghostty.app/Contents/Resources/ghostty/themes/Solarized Dark - Patched" \
   /Users/charliek/projects/roost/cmd/roost/themes/
./build/build.sh
```

Then set `theme = Solarized Dark - Patched` in `config.conf`. If the theme uses keys roost doesn't honor (e.g. `palette-generate`), they're silently dropped — the rest still works.
