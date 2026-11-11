# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "pillow==12.2.0",
#     "imagehash==4.3.2",
#     "numpy==2.4.6",
#     "scipy==1.17.1",
#     "pywavelets==1.9.0",
# ]
# ///
"""Generate the pHash/dHash golden fixtures of the hash parity test (spec §12.3).

Usage, from the repo root, with uv, which reads the pinned dependencies above::

    uv run scripts/gen_hash_golden.py crates/core/tests/fixtures/hash

Writes 40 deterministic, text-free images (seeded random shapes over
gradients) into the output folder, then runs ``fingerprint`` below (Python's
``imagehash`` on Pillow's upright 512 px thumbnail) on each and writes
``golden.json``:

* ``hashes``: ``{file: {phash, dhash, phash_bits, dhash_bits}}``. The hex is
  ``str(imagehash.ImageHash)``; the bits are the hash's row-major 8x8 boolean
  array as a 64-character ``0``/``1`` string, so the Rust side can check its
  bit order independently of pixel parity.
* ``fractional_box_thumbnail``: sampled pixels of Pillow's
  ``thumbnail((512, 512))`` of an in-memory integer-formula image whose size
  is not a multiple of the reduce factor, which pins the fractional-box path
  of the Rust ``pillow_thumbnail``.
* ``luma_resizes``: for each PNG (no JPEG decoder in the way), the hex bytes
  of imagehash's two inputs, ``convert("L").resize(..., LANCZOS)`` of the
  thumbnail at 9x8 and 32x32, to check each resize stage on its own.

Fixtures:

* 20 PNGs and 20 JPEGs (quality 90), from 300x200 up to 2400x1600.
* Six JPEGs carry an EXIF Orientation of 3, 6 or 8.
* Two pairs are "rotated pixels + orientation 1" vs "original pixels +
  orientation 6": after EXIF transposition both are the same upright image.
"""

from __future__ import annotations

import json
import math
import random
import sys
from dataclasses import dataclass
from pathlib import Path

import imagehash
import numpy as np
from PIL import Image, ImageDraw, ImageFilter, ImageOps

THUMBNAIL_SIZE = (512, 512)


@dataclass(frozen=True)
class Fingerprint:
    phash: imagehash.ImageHash
    dhash: imagehash.ImageHash

    def hamming_to(self, other: Fingerprint) -> int:
        return (self.phash - other.phash) + (self.dhash - other.dhash)


def fingerprint(path: Path) -> Fingerprint:
    """pHash and dHash of the EXIF-oriented 512 px thumbnail: the reference the Rust hashes match."""
    with Image.open(path) as raw:
        thumb = ImageOps.exif_transpose(raw).copy()
        thumb.thumbnail(THUMBNAIL_SIZE)
        return Fingerprint(phash=imagehash.phash(thumb), dhash=imagehash.dhash(thumb))

ORIENTATION_TAG = 0x0112
JPEG_QUALITY = 90

# (name, (width, height), orientation or None). Sizes mix exact and odd
# multiples of Pillow's reduce factor so both resample paths are covered.
PNG_FIXTURES: list[tuple[str, tuple[int, int]]] = [
    ("p00.png", (300, 200)),
    ("p01.png", (320, 240)),
    ("p02.png", (400, 300)),
    ("p03.png", (480, 320)),
    ("p04.png", (511, 383)),
    ("p05.png", (512, 512)),
    ("p06.png", (600, 400)),
    ("p07.png", (640, 480)),
    ("p08.png", (700, 467)),
    ("p09.png", (800, 533)),
    ("p10.png", (800, 600)),
    ("p11.png", (901, 601)),
    ("p12.png", (1000, 667)),
    ("p13.png", (1024, 768)),
    ("p14.png", (1100, 700)),
    ("p15.png", (1200, 800)),
    ("p16.png", (1280, 720)),
    ("p17.png", (400, 600)),
    ("p18.png", (600, 900)),
    ("p19.png", (1025, 1025)),
]

JPEG_FIXTURES: list[tuple[str, tuple[int, int], int | None]] = [
    ("j00.jpg", (300, 200), None),
    ("j01.jpg", (640, 427), None),
    ("j02.jpg", (800, 600), None),
    ("j03.jpg", (1024, 683), None),
    ("j04.jpg", (1200, 900), None),
    ("j05.jpg", (1500, 1000), 3),
    ("j06.jpg", (1600, 1067), None),
    ("j07.jpg", (1777, 1185), 8),
    ("j08.jpg", (1920, 1280), None),
    ("j09.jpg", (2047, 1365), None),
    ("j10.jpg", (2048, 1365), None),
    ("j11.jpg", (2100, 1400), None),
    ("j12.jpg", (2399, 1599), 3),
    ("j13.jpg", (2400, 1600), None),
    ("j14.jpg", (1200, 1800), None),
    ("j15.jpg", (1600, 2400), 8),
]

# Each pair: (rotated-pixels file, original-pixels file, original size).
# Sizes are multiples of 16 so the JPEG block grid survives the rotation.
ORIENTATION_PAIRS: list[tuple[str, str, tuple[int, int]]] = [
    ("pair1_rotated_o1.jpg", "pair1_original_o6.jpg", (1600, 1072)),
    ("pair2_rotated_o1.jpg", "pair2_original_o6.jpg", (800, 528)),
]

# The fractional-box pixel golden: 3001x2001 thumbnails to 512x341 through
# reduce((2, 2)) and the box (0, 0, 1500.5, 1000.5).
FRACTIONAL_SIZE = (3001, 2001)
SAMPLE_STEP = 16


def _seed(name: str) -> int:
    return sum((i + 1) * ord(c) for i, c in enumerate(name))


def _color(rng: random.Random) -> tuple[int, int, int]:
    return (rng.randrange(256), rng.randrange(256), rng.randrange(256))


def make_image(name: str, size: tuple[int, int]) -> Image.Image:
    """A seeded gradient covered with random shapes, softly blurred."""
    rng = random.Random(_seed(name))
    width, height = size
    angle = rng.uniform(0.0, 2.0 * math.pi)
    yy, xx = np.mgrid[0:height, 0:width].astype(np.float64)
    t = xx * math.cos(angle) + yy * math.sin(angle)
    t = (t - t.min()) / max(t.max() - t.min(), 1.0)
    c0 = np.array(_color(rng), dtype=np.float64)
    c1 = np.array(_color(rng), dtype=np.float64)
    pixels = c0 + (c1 - c0) * t[..., None]
    img = Image.fromarray(np.round(pixels).astype(np.uint8), "RGB")

    draw = ImageDraw.Draw(img)
    for _ in range(rng.randint(6, 14)):
        kind = rng.choice(("ellipse", "rectangle", "polygon", "line"))
        x0 = rng.uniform(-0.1, 0.9) * width
        y0 = rng.uniform(-0.1, 0.9) * height
        x1 = x0 + rng.uniform(0.08, 0.6) * width
        y1 = y0 + rng.uniform(0.08, 0.6) * height
        fill = _color(rng)
        if kind == "ellipse":
            draw.ellipse((x0, y0, x1, y1), fill=fill)
        elif kind == "rectangle":
            draw.rectangle((x0, y0, x1, y1), fill=fill)
        elif kind == "polygon":
            points = [(rng.uniform(x0, x1), rng.uniform(y0, y1)) for _ in range(rng.randint(3, 6))]
            draw.polygon(points, fill=fill)
        else:
            line_width = max(1, round(rng.uniform(0.005, 0.03) * width))
            draw.line((x0, y0, x1, y1), fill=fill, width=line_width)
    radius = rng.uniform(0.5, 2.0) * max(width, height) / 1000.0
    return img.filter(ImageFilter.GaussianBlur(radius))


def save_jpeg(img: Image.Image, path: Path, orientation: int | None) -> None:
    if orientation is None:
        img.save(path, quality=JPEG_QUALITY)
        return
    exif = Image.Exif()
    exif[ORIENTATION_TAG] = orientation
    img.save(path, quality=JPEG_QUALITY, exif=exif.tobytes())


def fractional_box_source() -> Image.Image:
    """Integer-only formula, rebuilt identically by the Rust test."""
    width, height = FRACTIONAL_SIZE
    yy, xx = np.mgrid[0:height, 0:width].astype(np.int64)
    r = xx * 255 // (width - 1)
    g = yy * 255 // (height - 1)
    b = (xx ^ yy) & 0xFF
    return Image.fromarray(np.stack([r, g, b], axis=-1).astype(np.uint8), "RGB")


def fractional_box_golden() -> dict:
    thumb = fractional_box_source()
    thumb.thumbnail((512, 512))
    width, height = thumb.size
    coords = {(x, y) for x in range(0, width, SAMPLE_STEP) for y in range(0, height, SAMPLE_STEP)}
    # The last column and row come from the partial reduce blocks.
    coords |= {(width - 1, y) for y in range(height)}
    coords |= {(x, height - 1) for x in range(width)}
    samples = [[x, y, *thumb.getpixel((x, y))] for x, y in sorted(coords)]
    return {
        "source_size": list(FRACTIONAL_SIZE),
        "formula": "r = x*255 // (w-1); g = y*255 // (h-1); b = (x ^ y) & 0xFF",
        "thumbnail_size": [width, height],
        "samples": samples,
    }


def luma_resizes(path: Path) -> dict:
    """The thumbnail's 9x8 and 32x32 Lanczos resizes, as imagehash makes them."""
    with Image.open(path) as img:
        thumb = img.convert("RGB")
    thumb.thumbnail((512, 512))
    luma = thumb.convert("L")
    return {
        "9x8": luma.resize((9, 8), Image.Resampling.LANCZOS).tobytes().hex(),
        "32x32": luma.resize((32, 32), Image.Resampling.LANCZOS).tobytes().hex(),
    }


def bits(hash_) -> str:
    return "".join("1" if b else "0" for b in hash_.hash.flatten())


def main() -> None:
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)

    for name, size in PNG_FIXTURES:
        make_image(name, size).save(out / name)
    for name, size, orientation in JPEG_FIXTURES:
        save_jpeg(make_image(name, size), out / name, orientation)
    for rotated_name, original_name, size in ORIENTATION_PAIRS:
        original = make_image(original_name, size)
        # Orientation 6 means "rotate 90 degrees clockwise to display";
        # Pillow's exif_transpose applies it as ROTATE_270.
        save_jpeg(original.transpose(Image.Transpose.ROTATE_270), out / rotated_name, 1)
        save_jpeg(original, out / original_name, 6)

    names = [n for n, _ in PNG_FIXTURES] + [n for n, _, _ in JPEG_FIXTURES]
    names += [n for pair in ORIENTATION_PAIRS for n in pair[:2]]
    hashes = {}
    for name in names:
        fp = fingerprint(out / name)
        hashes[name] = {
            "phash": str(fp.phash),
            "dhash": str(fp.dhash),
            "phash_bits": bits(fp.phash),
            "dhash_bits": bits(fp.dhash),
        }
    for rotated_name, original_name, _ in ORIENTATION_PAIRS:
        a, b = fingerprint(out / rotated_name), fingerprint(out / original_name)
        print(f"{rotated_name} vs {original_name}: python distance {a.hamming_to(b)}")

    golden = {
        "hashes": hashes,
        "fractional_box_thumbnail": fractional_box_golden(),
        "luma_resizes": {name: luma_resizes(out / name) for name, _ in PNG_FIXTURES},
    }
    with open(out / "golden.json", "w", encoding="utf-8", newline="\n") as f:
        json.dump(golden, f, indent=1, sort_keys=True)
        f.write("\n")
    total = sum(p.stat().st_size for p in out.iterdir())
    print(f"{len(names)} fixtures, {total / 1e6:.2f} MB in {out}")


if __name__ == "__main__":
    main()
