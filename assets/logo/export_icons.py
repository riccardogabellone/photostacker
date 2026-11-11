"""Render the SVG masters into every raster the app and its packages need.

Usage (from assets/logo/, after build_logo.py):  uv run --with pillow python export_icons.py
Requires resvg on PATH (`cargo install resvg`).

Writes ../icons/:
- icon-<n>.png for the Linux hicolor sizes and the window icon. Sizes up to
  SMALL_MAX come from logo-small.svg, larger ones from logo.svg;
- icon.ico (Windows, one frame per size, each frame from its own render);
- icon.icns (macOS, from logo-macos.svg, which follows Apple's 824-in-1024 grid);
and build/compare.png: the four masters, plus every small size at 1x and
enlarged 4x, to check by eye.
"""

import subprocess
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
BUILD = HERE / "build"
ICONS = HERE.parent / "icons"
SIZES = [16, 24, 32, 48, 64, 128, 256, 512, 1024]
SMALL_MAX = 32
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]
ICNS_SIZES = [16, 32, 64, 128, 256, 512, 1024]


def render(svg, size, out):
    subprocess.run(
        ["resvg", "-w", str(size), "-h", str(size), str(HERE / svg), str(out)],
        check=True,
        capture_output=True,
    )
    return Image.open(out).convert("RGBA")


def master_for(size):
    return "logo-small.svg" if size <= SMALL_MAX else "logo.svg"


BUILD.mkdir(exist_ok=True)
ICONS.mkdir(exist_ok=True)

pngs = {size: render(master_for(size), size, ICONS / f"icon-{size}.png") for size in SIZES}

largest_ico = pngs[ICO_SIZES[-1]]
largest_ico.save(
    ICONS / "icon.ico",
    sizes=[(size, size) for size in ICO_SIZES],
    append_images=[pngs[size] for size in ICO_SIZES[:-1]],
)

macos = {size: render("logo-macos.svg", size, BUILD / f"macos-{size}.png") for size in ICNS_SIZES}
macos[1024].save(ICONS / "icon.icns", append_images=[macos[size] for size in ICNS_SIZES[:-1]])
print(f"wrote {len(SIZES)} PNGs, icon.ico, icon.icns to {ICONS}")

# Comparison sheet.
panel = 400
masters = [
    render(name, panel, BUILD / f"preview-{name}.png")
    for name in ("logo.svg", "logo-small.svg", "logo-macos.svg", "logo-mark.svg")
]
ladder = [16, 24, 32, 48, 64]
ladder_height = 64 * 4 + 80
sheet = Image.new("RGBA", (len(masters) * (panel + 20), panel + 20 + ladder_height), (230, 230, 230, 255))
for index, image in enumerate(masters):
    sheet.alpha_composite(image, (index * (panel + 20), 0))
x = 10
for size in ladder:
    image = pngs[size]
    sheet.alpha_composite(image, (x, panel + 30))
    sheet.alpha_composite(image.resize((size * 4, size * 4), Image.NEAREST), (x, panel + 30 + 70))
    x += size * 4 + 20
sheet.save(BUILD / "compare.png")
print(f"wrote {BUILD / 'compare.png'}")
