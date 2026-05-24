# Roost icon toolchain

The Roost app icon is the owl mascot (shared lineage with the sibling projects
lumen and tapper), recolored to Roost's brand. The SVG sources + the generator
live here so the icon can be **recolored between versions with one command** —
no binary editing.

## Files

- `reference/owl_logo_colored.svg` — owl with baked yellow irises + white pupils;
  the body (`fill="currentColor"`) is recolored to white and composed full-bleed
  on the brand-color background. **Source of truth — edit this to change the
  artwork.**
- `reference/owl_logo.svg` — monochrome silhouette (no eyes); used with
  `--source plain` if you want a flat one-color mark.
- `generate_icons.py` — renders + composes every packaging target.
- `regenerate.sh` — runs the generator in an ephemeral `uv` env (cairosvg + Pillow).

## Outputs (committed; CI does not regenerate)

- `packaging/icons/hicolor/256x256/apps/roost.png` — Linux `.deb` hicolor icon
- `packaging/icons/hicolor/512x512/apps/roost.png` — Linux `.deb` hicolor icon
- `mac/AppIcon.icon/` — macOS **Tahoe glass icon** (Icon Composer source:
  `icon.json` + `Assets/owl.png`). `mac/scripts/bundle.sh` compiles it with
  `actool` into the `Assets.car` the `.app` ships.
- `mac/Resources/AppIcon.icns` — macOS **flat fallback** (built via `iconutil`)
  for pre-Tahoe and for `bundle.sh` runs without `actool` (Command-Line-Tools-
  only dev boxes).

The release CI ships these committed assets, so it needs no SVG-render toolchain
(it does need full Xcode on the Mac runner for `actool`). Regenerating is a
deliberate local step — re-run, then commit the changed files.

### Why two Mac forms?

On macOS 26 (Tahoe) the OS masks every Dock/Cmd-Tab icon to its own squircle and
draws a glass tile behind it. A loose `.icns` is treated as legacy and *inset*
on that tile — a gray frame around the art. A compiled Icon Composer catalog
fills the tile edge-to-edge with the glass treatment (parity with ghostty/cmux),
so that's the primary Mac icon; the flat `.icns` is only the fallback. Linux
desktops don't have this tile, so the flat PNG is correct there.

## Regenerate

```sh
# Default: white owl, full-bleed on Roost Violet (#6C4FD6)
./packaging/icon/regenerate.sh

# Try a different background color (then commit the regenerated PNGs + .icns)
./packaging/icon/regenerate.sh --bg '#1F6FEB'

# Flat silhouette, transparent background
./packaging/icon/regenerate.sh --source plain --transparent
```

Run on **macOS** to refresh `AppIcon.icns` (`iconutil` is macOS-only; on Linux
the script writes the PNGs and skips the `.icns`). On a Mac dev box the wrapper
adds Homebrew's `lib` to the dyld path so cairosvg finds `libcairo`.

## macOS glass icon: how it works (and the gotchas)

macOS 26 (Tahoe) renders every Dock / Cmd-Tab icon on a system "glass tile" and
masks it to one squircle. How you *ship* the icon decides whether it **fills**
that tile or gets **inset** on it (a gray frame around the art). The points
below are non-obvious and cost real iteration to pin down — read them before
touching the Mac icon.

### What does NOT work (don't re-try these)

- **A loose `.icns`** (what we shipped before Tahoe): treated as a legacy icon
  and inset on the tile → gray frame. Independent of the art — full-bleed vs.
  inset, rounded vs. square corners are all still framed.
- **A classic compiled `.appiconset` catalog**: also inset. Compiling alone
  isn't enough; the *source format* is what matters.

### What works

A compiled **Icon Composer `.icon`** catalog (the format ghostty/cmux ship).
Tahoe then fills the tile edge-to-edge and applies the glass treatment (sheen,
depth, automatic dark-mode + user-tinted variants). `generate_icons.py` authors
the `.icon` *source* from the SVG (`mac/AppIcon.icon/` — a solid Roost-Violet
fill + the white owl as a foreground layer); `mac/scripts/bundle.sh` compiles
it. `CFBundleIconName=AppIcon` (Info.plist) routes the OS to the catalog.

### actool gotchas

- **Pass the `.icon` as a direct input**, not nested in an `.xcassets`:
  `actool mac/AppIcon.icon --compile <out> --platform macosx --app-icon AppIcon
  --minimum-deployment-target <ver> --output-partial-info-plist <out>/p.plist`.
  Nesting `AppIcon.icon` inside `Assets.xcassets` silently emits an empty
  partial plist and **no `Assets.car`**.
- **Needs Xcode 26+.** The `.icon` format is new in the Tahoe-era Xcode; bare
  Command Line Tools have no `actool`, and older Xcode won't understand it.
  `bundle.sh` falls back to the flat `.icns` when `actool` is missing (bundle
  still launches, just framed on Tahoe). The release workflow runs on
  `macos-26` so Xcode 26 is present.
- **Min-deployment can stay low.** Compiling at `LSMinimumSystemVersion` (15.0)
  still yields the glass-capable catalog — the glass is applied by the *end
  user's* Tahoe OS at render time, so the app keeps its 15.0 floor.

### Verifying a Mac icon change (the cache trap)

LaunchServices/iconservices caches the Dock icon per bundle-id **and** path.
Rebuilding `mac/build/Roost.app` in place and relaunching keeps showing the
**stale** icon — it looks like the change failed when it didn't. To actually
see a changed icon, launch a copy under a *fresh* bundle id:

```sh
cp -R mac/build/Roost.app /tmp/RoostVerify.app
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier ai.stridelabs.RoostVerify" \
  /tmp/RoostVerify.app/Contents/Info.plist
codesign --force --deep --sign - /tmp/RoostVerify.app
open /tmp/RoostVerify.app          # then look at the Dock
```

### The fallback chain

`mac/AppIcon.icon` (glass, Tahoe) → `mac/Resources/AppIcon.icns` (flat, pre-
Tahoe or no-`actool` builds) → Linux uses the hicolor PNGs above (no glass tile
there, so the flat full-bleed violet owl is correct).

## Brand palette (first pass — "twilight owl")

Roost is the nocturnal sibling to lumen's daytime blue (`#2E5AAB`) — a violet
identity in the same cool family, distinct, not matching.

| Token        | Hex       | Use                                            |
|--------------|-----------|------------------------------------------------|
| Roost Violet | `#6C4FD6` | primary / icon background (icon default)       |
| Twilight     | `#4B36A6` | deeper shade (depth, hover)                    |
| Amber        | `#F4B63F` | accent; harmonizes with the owl's yellow eyes  |
| Ink          | `#1B1830` | near-black, violet-tinted (dark UI / text)     |
| Mist         | `#F1EEFB` | pale violet off-white (backgrounds)            |

The owl's eyes are baked into the colored SVG as `#F4C430` (irises) / `#FFFFFF`
(pupils); change them in `reference/owl_logo_colored.svg` if the palette shifts.
