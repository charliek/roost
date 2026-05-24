"""Generate Roost app-icon assets from the owl SVG.

Renders the owl via cairosvg as a white silhouette (keeping the SVG's baked
yellow irises + white pupils), composes it full-bleed on a Roost-Violet rounded
square, and writes every packaging target in one run:

  - packaging/icons/hicolor/256x256/apps/roost.png   (Linux .deb hicolor)
  - packaging/icons/hicolor/512x512/apps/roost.png   (Linux .deb hicolor)
  - mac/AppIcon.icon/                                 (macOS Tahoe glass icon —
        Icon Composer source; bundle.sh compiles it with actool)
  - mac/Resources/AppIcon.icns                        (macOS flat fallback for
        pre-Tahoe / no-actool builds — needs iconutil)

The color is a CLI arg, not a code edit, so iterating between versions is one
command. See ./regenerate.sh and ./README.md.

Roost brand palette (first pass — "twilight owl"):
  Roost Violet #6C4FD6 (default background)   Twilight #4B36A6
  Amber #F4B63F (echoes the owl's eyes)  Ink #1B1830   Mist #F1EEFB

Usage:
  ./packaging/icon/regenerate.sh                 # default Roost Violet
  ./packaging/icon/regenerate.sh --color '#1F6FEB'
"""

import argparse
import io
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import cairosvg
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent.parent
ICON_DIR = Path(__file__).resolve().parent
REF_DIR = ICON_DIR / "reference"

HICOLOR = ROOT / "packaging" / "icons" / "hicolor"
ICNS_OUT = ROOT / "mac" / "Resources" / "AppIcon.icns"
ICON_COMPOSER_OUT = ROOT / "mac" / "AppIcon.icon"

DEFAULT_COLOR = "#FFFFFF"  # owl body — white, sits on the violet background
DEFAULT_BG = "#6C4FD6"     # Roost Violet — the icon background

# Full-bleed: the rounded square fills the whole canvas (no transparent margin).
# macOS 26 (Tahoe) masks every icon to its own squircle and draws a system tile
# behind it — any transparent margin lets that tile show through as a gray frame,
# so we fill edge-to-edge. The baked CORNER_PCT rounding is what Linux (which
# doesn't mask) renders; on Tahoe the system re-masks the corners. ~22.37%
# matches Apple's icon-grid corner radius.
MARGIN_PCT = 0.0
CORNER_PCT = 0.2237
OWL_PAD_PCT = 0.085  # padding of the owl inside the square (small = big owl)


def hex_to_rgb(s: str) -> tuple[int, int, int]:
    s = s.lstrip("#")
    if len(s) != 6:
        raise ValueError(f"expected #RRGGBB, got {s!r}")
    return (int(s[0:2], 16), int(s[2:4], 16), int(s[4:6], 16))


def render_owl(svg_path: Path, color: tuple[int, int, int], preserve_colors: bool) -> Image.Image:
    """Render the owl SVG to a high-res RGBA image with the body recolored.

    Substitutes the body fill (`currentColor` / `black`) with the brand color;
    explicit fills (yellow irises, white pupils) survive when preserve_colors.
    """
    svg_text = svg_path.read_text()
    hex_color = "#{:02x}{:02x}{:02x}".format(*color)
    svg_text = svg_text.replace('fill="currentColor"', f'fill="{hex_color}"')
    svg_text = svg_text.replace('fill="black"', f'fill="{hex_color}"')

    png = cairosvg.svg2png(bytestring=svg_text.encode(), output_width=2048)
    assert isinstance(png, bytes)
    owl = Image.open(io.BytesIO(png)).convert("RGBA")

    if preserve_colors:
        return owl
    flat = Image.new("RGBA", owl.size, (*color, 255))
    flat.putalpha(owl.split()[3])
    return flat


def compose(owl: Image.Image, size: int, bg: tuple[int, int, int], transparent: bool) -> Image.Image:
    """Compose the owl on a (white) rounded-square at the requested pixel size."""
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    margin = int(size * MARGIN_PCT)
    inner = size - 2 * margin  # rounded-square side

    if not transparent:
        radius = int(inner * CORNER_PCT)
        draw = ImageDraw.Draw(canvas)
        draw.rounded_rectangle(
            [margin, margin, margin + inner - 1, margin + inner - 1],
            radius=radius,
            fill=(*bg, 255),
        )

    # Fit the owl inside the rounded square, preserving aspect ratio.
    owl_box = int(inner * (1 - 2 * OWL_PAD_PCT))
    aspect = owl.width / owl.height
    if aspect >= 1:
        fit_w, fit_h = owl_box, max(1, int(owl_box / aspect))
    else:
        fit_w, fit_h = max(1, int(owl_box * aspect)), owl_box
    resized = owl.resize((fit_w, fit_h), Image.Resampling.LANCZOS)

    ox = (size - fit_w) // 2
    oy = (size - fit_h) // 2
    canvas.paste(resized, (ox, oy), resized)
    return canvas


def write_png(img: Image.Image, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    img.save(path, "PNG")
    print(f"  wrote {path.relative_to(ROOT)}")


def build_icns(owl: Image.Image, bg: tuple[int, int, int], transparent: bool) -> None:
    """Render the iconset sizes and assemble AppIcon.icns via iconutil (macOS)."""
    if not shutil.which("iconutil"):
        print("  iconutil not found (non-macOS) — skipping AppIcon.icns; "
              "regenerate on a Mac to refresh it")
        return
    sizes = [
        ("icon_16x16.png", 16), ("icon_16x16@2x.png", 32),
        ("icon_32x32.png", 32), ("icon_32x32@2x.png", 64),
        ("icon_128x128.png", 128), ("icon_128x128@2x.png", 256),
        ("icon_256x256.png", 256), ("icon_256x256@2x.png", 512),
        ("icon_512x512.png", 512), ("icon_512x512@2x.png", 1024),
    ]
    with tempfile.TemporaryDirectory() as td:
        iconset = Path(td) / "Roost.iconset"
        iconset.mkdir()
        for name, px in sizes:
            compose(owl, px, bg, transparent).save(iconset / name, "PNG")
        ICNS_OUT.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            ["iconutil", "-c", "icns", str(iconset), "-o", str(ICNS_OUT)],
            check=True,
        )
    print(f"  wrote {ICNS_OUT.relative_to(ROOT)}")


def build_icon_composer(owl: Image.Image, bg: tuple[int, int, int]) -> None:
    """Write the AppIcon.icon (Icon Composer) source bundle for the Mac app.

    macOS 26 (Tahoe) masks every Dock/Cmd-Tab icon to its own squircle and
    draws a glass tile behind it. A loose .icns is treated as legacy and gets
    *inset* on that tile — a gray frame around our art. Only a compiled .icon
    catalog fills the tile edge-to-edge (what ghostty/cmux ship). We emit the
    .icon *source* here — a solid brand-color fill with the white owl as the
    foreground layer; mac/scripts/bundle.sh compiles it with `actool` into the
    Assets.car the app ships. The Linux PNGs are unaffected (no such tile).

    Hand-authored JSON (not Icon Composer.app) so the whole icon regenerates
    from the SVG with one command. 1024 = Icon Composer's design canvas; the
    owl rides on a transparent canvas so the fill paints the tile background.
    """
    # Rebuild from scratch so a changed layer set (e.g. a future richer icon)
    # can't leave stale PNGs behind in the committed bundle.
    if ICON_COMPOSER_OUT.exists():
        shutil.rmtree(ICON_COMPOSER_OUT)
    assets = ICON_COMPOSER_OUT / "Assets"
    assets.mkdir(parents=True, exist_ok=True)
    compose(owl, 1024, bg, transparent=True).save(assets / "owl.png", "PNG")
    r, g, b = bg
    icon = {
        "fill": {"solid": f"srgb:{r / 255:.5f},{g / 255:.5f},{b / 255:.5f},1.00000"},
        "groups": [
            {
                "layers": [{"image-name": "owl.png", "name": "owl"}],
                "shadow": {"kind": "neutral", "opacity": 0.5},
                "translucency": {"enabled": True, "value": 0.5},
            }
        ],
        "supported-platforms": {"circles": ["watchOS"], "squares": "shared"},
    }
    (ICON_COMPOSER_OUT / "icon.json").write_text(json.dumps(icon, indent=2) + "\n")
    print(f"  wrote {(ICON_COMPOSER_OUT / 'icon.json').relative_to(ROOT)}")
    print(f"  wrote {(assets / 'owl.png').relative_to(ROOT)}")


def main() -> None:
    ap = argparse.ArgumentParser(description="Generate Roost icon assets.")
    ap.add_argument("--color", default=DEFAULT_COLOR, help="owl body color (#RRGGBB)")
    ap.add_argument("--bg", default=DEFAULT_BG, help="background color (#RRGGBB)")
    ap.add_argument("--source", choices=["colored", "plain"], default="colored",
                    help="colored = owl with yellow eyes; plain = monochrome silhouette")
    ap.add_argument("--transparent", action="store_true",
                    help="no rounded-square background (transparent canvas)")
    args = ap.parse_args()

    svg = REF_DIR / ("owl_logo_colored.svg" if args.source == "colored" else "owl_logo.svg")
    if not svg.exists():
        print(f"error: source SVG not found: {svg}", file=sys.stderr)
        sys.exit(1)

    color = hex_to_rgb(args.color)
    bg = hex_to_rgb(args.bg)
    # The colored SVG carries explicit eye fills that must survive; the plain
    # silhouette is flattened to the brand color.
    owl = render_owl(svg, color, preserve_colors=(args.source == "colored"))

    print(f"Generating Roost icons (color={args.color}, bg={args.bg}, source={args.source})")
    write_png(compose(owl, 256, bg, args.transparent),
              HICOLOR / "256x256" / "apps" / "roost.png")
    write_png(compose(owl, 512, bg, args.transparent),
              HICOLOR / "512x512" / "apps" / "roost.png")
    build_icns(owl, bg, args.transparent)
    if not args.transparent:
        build_icon_composer(owl, bg)
    print("Done.")


if __name__ == "__main__":
    main()
