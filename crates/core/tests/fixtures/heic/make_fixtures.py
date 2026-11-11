# /// script
# requires-python = ">=3.12"
# dependencies = ["pillow==12.2.0", "pillow-heif==1.3.0"]
# ///
"""Regenerates the HEIC fixtures of this folder. All content is drawn here.

Run from the repo root with uv, which reads the dependencies above (pillow-heif
bundles libheif with an HEVC encoder, in a throwaway environment only):

    uv run crates/core/tests/fixtures/heic/make_fixtures.py

- sample.heic: 640x384 stored pixels, 8-bit, with an EXIF block (Make, Model,
  Orientation 6, DateTimeOriginal, SubSecTimeOriginal). pillow-heif also
  writes the orientation as an `irot` transform, so a viewer shows it 384x640.
  The EXIF keeps Orientation 6, as iPhones do: a reader that applies both
  turns the image twice.
- deep-no-exif.heic: 64x48, 10-bit, no EXIF block.
- alpha.heic: 64x48, 8-bit RGBA, no EXIF block. Every pixel is
  (200, 100, 50); the left half is opaque (alpha 255), the right half has
  alpha 64.

Stored pixels of sample.heic: a gray ramp, a red 96x96 block at the top-left
corner and a green one at the top-right corner. Shown upright (turned 90
degrees clockwise) the red block is at the top-right, the green one at the
bottom-right.
"""

from pathlib import Path

import pillow_heif
from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent

pillow_heif.register_heif_opener()


def sample() -> None:
    width, height, block = 640, 384, 96
    img = Image.new("RGB", (width, height))
    draw = ImageDraw.Draw(img)
    for x in range(width):
        v = 40 + x * 160 // width
        draw.line([(x, 0), (x, height - 1)], fill=(v, v, v))
    draw.rectangle([0, 0, block - 1, block - 1], fill=(255, 0, 0))
    draw.rectangle([width - block, 0, width - 1, block - 1], fill=(0, 255, 0))

    exif = Image.Exif()
    exif[0x010F] = "Photostacker"
    exif[0x0110] = "Fixture HEIC"
    exif[0x0112] = 6
    sub = exif.get_ifd(0x8769)
    sub[0x9003] = "2026:11:08 12:34:56"
    sub[0x9291] = "25"
    img.save(HERE / "sample.heic", format="HEIF", quality=60, exif=exif.tobytes())


def deep_no_exif() -> None:
    width, height = 64, 48
    data = bytearray()
    for y in range(height):
        for x in range(width):
            # A 16-bit ramp per channel; pillow-heif stores it as 10-bit.
            r = (x * 65535) // (width - 1)
            g = (y * 65535) // (height - 1)
            b = 0x8000
            for v in (r, g, b):
                data += v.to_bytes(2, "little")
    heif = pillow_heif.from_bytes(mode="RGB;16", size=(width, height), data=bytes(data))
    heif.save(HERE / "deep-no-exif.heic", quality=80)


def alpha() -> None:
    width, height = 64, 48
    img = Image.new("RGBA", (width, height), (200, 100, 50, 255))
    ImageDraw.Draw(img).rectangle([width // 2, 0, width - 1, height - 1], fill=(200, 100, 50, 64))
    img.save(HERE / "alpha.heic", format="HEIF", quality=90)


if __name__ == "__main__":
    sample()
    deep_no_exif()
    alpha()
    print("wrote", sorted(p.name for p in HERE.glob("*.heic")))
