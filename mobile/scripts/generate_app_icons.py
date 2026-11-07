"""Generate the mobile app's icon PNGs from Roost's owl.

The owl is the desktop icon's own SVG (packaging/icon/reference/), so the two
apps share one source. flutter_launcher_icons then deploys the PNGs into the
android/ and macos/ icon slots. Run `make mobile-icons`.

Outputs (mobile/assets/icon/):
    app_icon.png             1024  Android base       : violet owl on white
    app_icon_foreground.png  1024  Android adaptive fg: violet owl, transparent
    app_icon_desktop.png     1024  macOS              : white owl on a violet squircle,
                                                        the desktop app's look
"""

import io
from pathlib import Path

import cairosvg
from PIL import Image, ImageDraw

ROOST_VIOLET = "#6C4FD6"  # packaging/icon/generate_icons.py's DEFAULT_BG
WHITE = "#FFFFFF"

ICON_SIZE = 1024
RENDER_PX = 2048  # rasterize large, then LANCZOS down
SQUIRCLE_CORNER_PCT = 0.2237
DESKTOP_MARGIN_PCT = 0.10  # the transparent margin macOS app icons carry
DESKTOP_OWL_PAD = 0.16
MOBILE_OWL_PAD = 0.10
# flutter_launcher_icons insets the adaptive foreground a further 16%.
MOBILE_FG_PAD = 0.12

MOBILE = Path(__file__).resolve().parent.parent
SVG = MOBILE.parent / "packaging" / "icon" / "reference" / "owl_logo_colored.svg"
OUT = MOBILE / "assets" / "icon"


def hex_to_rgb(h: str) -> tuple[int, int, int]:
    h = h.lstrip("#")
    return (int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16))


def render_owl(body_hex: str) -> Image.Image:
    svg = SVG.read_text().replace('fill="currentColor"', f'fill="{body_hex}"')
    png = cairosvg.svg2png(bytestring=svg.encode(), output_width=RENDER_PX)
    assert isinstance(png, bytes)
    return Image.open(io.BytesIO(png)).convert("RGBA")


def compose(
    owl: Image.Image,
    *,
    bg: str | None,
    owl_pad: float,
    margin_pct: float = 0.0,
    rounded: bool = False,
) -> Image.Image:
    canvas = Image.new("RGBA", (ICON_SIZE, ICON_SIZE), (0, 0, 0, 0))
    margin = round(ICON_SIZE * margin_pct)
    inner = ICON_SIZE - 2 * margin
    if bg is not None:
        box = [margin, margin, margin + inner - 1, margin + inner - 1]
        fill = (*hex_to_rgb(bg), 255)
        draw = ImageDraw.Draw(canvas)
        if rounded:
            draw.rounded_rectangle(box, radius=round(inner * SQUIRCLE_CORNER_PCT), fill=fill)
        else:
            draw.rectangle(box, fill=fill)

    box = round(inner * (1 - 2 * owl_pad))
    aspect = owl.width / owl.height
    size = (box, max(1, round(box / aspect))) if aspect >= 1 else (max(1, round(box * aspect)), box)
    fitted = owl.resize(size, Image.Resampling.LANCZOS)
    canvas.paste(fitted, ((ICON_SIZE - fitted.width) // 2, (ICON_SIZE - fitted.height) // 2), fitted)
    return canvas


def write(img: Image.Image, name: str) -> None:
    img.save(OUT / name)
    print(f"  {(OUT / name).relative_to(MOBILE)}")


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    violet = render_owl(ROOST_VIOLET)
    write(compose(violet, bg=WHITE, owl_pad=MOBILE_OWL_PAD), "app_icon.png")
    write(compose(violet, bg=None, owl_pad=MOBILE_FG_PAD), "app_icon_foreground.png")
    write(
        compose(
            render_owl(WHITE),
            bg=ROOST_VIOLET,
            owl_pad=DESKTOP_OWL_PAD,
            margin_pct=DESKTOP_MARGIN_PCT,
            rounded=True,
        ),
        "app_icon_desktop.png",
    )


if __name__ == "__main__":
    main()
