# Themes

Roost ships with a small set of bundled color themes. Pick one with `theme = NAME` in `config.conf`. Themes control colors only; font choice stays on `font_family` / `font_size`.

```conf
# ~/.config/roost/config.conf
theme = Dracula+
```

The setting takes effect on the next launch (themes are not hot-reloaded today). Unknown names fall back to `roost-dark` with a warning logged at startup.

## Bundled themes

| Name                     | Style                                                       |
|--------------------------|-------------------------------------------------------------|
| `roost-dark`             | The default. Roost's built-in dark palette (cmux-derived).  |
| `0x96f`                  | Muted dark with vivid accents.                              |
| `Atom`                   | The classic Atom editor palette.                           |
| `Atom One Dark`          | Atom editor's familiar dark scheme.                        |
| `Atom One Light`         | Atom One on a light background.                            |
| `Ayu Light`              | Ayu's bright, warm light variant.                          |
| `Ayu Mirage`             | Ayu's muted mid-dark variant.                              |
| `Catppuccin Frappe`      | Catppuccin's softer mid-dark flavor.                       |
| `Catppuccin Macchiato`   | Catppuccin's deeper mid-dark flavor.                       |
| `Catppuccin Mocha`       | Pastel dark; the darkest Catppuccin flavor.               |
| `Dracula`                | The original Dracula palette.                              |
| `Dracula+`               | Higher-contrast Dracula variant.                          |
| `Everforest Dark Hard`   | Warm, low-saturation forest greens.                       |
| `GitHub Dark Default`    | GitHub's current dark UI palette.                         |
| `Gruvbox Dark`           | Warm retro palette, medium contrast.                      |
| `Gruvbox Dark Hard`      | Warm, high-contrast retro palette.                        |
| `Kanagawa Wave`          | Muted indigo/teal inspired by Hokusai.                    |
| `Nord`                   | Cool, desaturated arctic blues.                           |
| `One Half Dark`          | The One Half dark scheme.                                 |
| `Rose Pine`              | Soft rosy mauves on deep base.                            |
| `Solarized Dark Patched` | Solarized Dark with the common ANSI patch.               |
| `TokyoNight`             | Cool blues + softer accent colors.                       |
| `TokyoNight Night`       | The darkest TokyoNight variant.                          |
| `TokyoNight Storm`       | TokyoNight on a slightly lighter base.                  |

Every theme other than `roost-dark` is a byte-identical copy of a file from [Ghostty's bundled set](https://github.com/ghostty-org/ghostty/tree/main/src/config/themes). If you want a Ghostty theme that's not on this list, it usually parses cleanly in roost — see *File format* below. (`Atom One Light` and `Ayu Light` are light-background themes; they render a light terminal area inside Roost's dark window chrome.)

## File format

Theme files are plain key=value text with `#` comments — the same syntax as `config.conf` (and as Ghostty's themes). Roost starts from the built-in `roost-dark` palette and overlays whatever keys a file sets; any key a file omits keeps its `roost-dark` value. Nothing is required — a file with only `palette` lines is valid. Roost honors:

| Key                    | Effect                                                                |
|------------------------|-----------------------------------------------------------------------|
| `background`           | Default background color.                                             |
| `foreground`           | Default foreground color.                                             |
| `palette = N=#RRGGBB`  | One of the 16 ANSI colors (`N` in `0..15`). Indices 16–255 are computed (xterm 6×6×6 cube + 24-step gray ramp). |
| `cursor-color`         | Cursor block fill.                                                    |
| `bold-color`           | Color for bold text drawn with the *default* foreground. Bold-with-explicit-color (e.g. bold red) is unaffected. |
| `selection-background` | Selection overlay color (rendered at 35% alpha).                     |
| `selection-foreground` | Parsed but currently unused (see *Limitations*).                     |

Any other key — including Ghostty's `cursor-text`, `link-color`, and `palette-generate` — is silently ignored. A theme file is typically ~22 lines.

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

- **No user override directory.** Only the bundled themes are loadable. To use a theme that isn't bundled, add the file to the bundled set and rebuild (see below) — themes are embedded into the binary at compile time.
- **No hot-reload.** Theme is loaded once when each tab is created. To switch, edit `config.conf` and restart roost.
- **`selection-foreground` is parsed but not yet rendered.** Roost's selection is currently a 35%-alpha overlay over the existing text; honoring `selection-foreground` requires switching to opaque selection + re-rendering glyphs, which is a render-order change tracked separately.
- **Indices 16–255 of the palette are not customizable** — they're filled by the standard xterm formula. Bundled themes only set indices 0–15, matching every Ghostty theme.

## Trying a Ghostty theme that isn't bundled

The format is identical, so you can add one to the bundled set. Themes
live in two byte-identical trees — the Rust crate (Linux UI) and the Mac
SwiftPM bundle — kept in sync by `make themes-check`:

```bash
# Run from your roost checkout. Copy into BOTH trees.
SRC="/Applications/Ghostty.app/Contents/Resources/ghostty/themes/Solarized Osaka Night"
cp "$SRC" crates/roost-linux/src/resources/themes/
cp "$SRC" mac/Sources/Roost/Resources/themes/
```

The Mac UI discovers themes by listing its bundle directory, so it picks
the new file up automatically. The Linux UI embeds them explicitly: add a
matching entry to the `BUNDLED_THEMES` array in
`crates/roost-linux/src/theme.rs`. Then rebuild (`cargo build` /
`swift build`) and set `theme = Solarized Osaka Night` in `config.conf`.
If the theme uses keys roost doesn't honor (e.g. `palette-generate`),
they're silently dropped — the rest still works.
