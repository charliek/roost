# Roost icon toolchain

The Roost app icon is the owl mascot (shared lineage with the sibling projects
lumen and tapper), recolored to Roost's brand. The SVG sources + the generator
live here so the icon can be **recolored between versions with one command** —
no binary editing.

## Files

- `reference/owl_logo_colored.svg` — owl with baked yellow irises + white pupils;
  the body (`fill="currentColor"`) is recolored to the brand color. **Source of
  truth — edit this to change the artwork.**
- `reference/owl_logo.svg` — monochrome silhouette (no eyes); used with
  `--source plain` if you want a flat one-color mark.
- `generate_icons.py` — renders + composes every packaging target.
- `regenerate.sh` — runs the generator in an ephemeral `uv` env (cairosvg + Pillow).

## Outputs (committed; CI does not regenerate)

- `packaging/icons/hicolor/256x256/apps/roost.png` — Linux `.deb` hicolor icon
- `packaging/icons/hicolor/512x512/apps/roost.png` — Linux `.deb` hicolor icon
- `mac/Resources/AppIcon.icns` — macOS `.app` icon (built via `iconutil`)

The release CI ships these committed assets, so it needs no SVG-render toolchain.
Regenerating is a deliberate local step — re-run, then commit the changed files.

## Regenerate

```sh
# Default: Roost Violet (#6C4FD6) owl on white
./packaging/icon/regenerate.sh

# Try a different brand color (then commit the regenerated PNGs + .icns)
./packaging/icon/regenerate.sh --color '#1F6FEB'

# Flat silhouette, transparent background
./packaging/icon/regenerate.sh --source plain --transparent
```

Run on **macOS** to refresh `AppIcon.icns` (`iconutil` is macOS-only; on Linux
the script writes the PNGs and skips the `.icns`). On a Mac dev box the wrapper
adds Homebrew's `lib` to the dyld path so cairosvg finds `libcairo`.

## Brand palette (first pass — "twilight owl")

Roost is the nocturnal sibling to lumen's daytime blue (`#2E5AAB`) — a violet
identity in the same cool family, distinct, not matching.

| Token        | Hex       | Use                                            |
|--------------|-----------|------------------------------------------------|
| Roost Violet | `#6C4FD6` | primary / owl body (icon default)              |
| Twilight     | `#4B36A6` | deeper shade (depth, hover)                    |
| Amber        | `#F4B63F` | accent; harmonizes with the owl's yellow eyes  |
| Ink          | `#1B1830` | near-black, violet-tinted (dark UI / text)     |
| Mist         | `#F1EEFB` | pale violet off-white (backgrounds)            |

The owl's eyes are baked into the colored SVG as `#F4C430` (irises) / `#FFFFFF`
(pupils); change them in `reference/owl_logo_colored.svg` if the palette shifts.
