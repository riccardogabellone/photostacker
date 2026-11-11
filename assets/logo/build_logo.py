"""Generate the logo SVG masters from the constants below.

Usage (from assets/logo/):  uv run --with pillow python build_logo.py

The mark is three frames stacked on a dark rounded tile, dark amber at the back
to bright amber at the front: an exposure bracket turned into layers. There is no
raster source; every shape is an exact <rect> drawn from the constants here.

Each frame on a tile carries a stroke in the tile's colour, so where a frame lies
on the one behind it a thin band of tile shows and the frames read as separate
sheets. The transparent mark (logo-mark.svg) has no tile to paint that band with,
so it cuts the same band out of the frames behind with masks instead.

Everything is written in a 1024-unit coordinate space.
"""

from pathlib import Path
from typing import NamedTuple

HERE = Path(__file__).resolve().parent

CANVAS = 1024
TILE_COLOUR = "#16181D"
TILE_RADIUS = 212  # on the full-bleed tile; scaled with the tile
MACOS_INSET = 100  # Apple's grid: an 824 px tile in the 1024 canvas
MARK_MARGIN = 32  # between the frames and the canvas edge in logo-mark.svg


class Stack(NamedTuple):
    """Frames drawn back to front, each `step` right of and below the one behind it."""

    colours: list
    width: float
    height: float
    radius: float
    step_x: float
    step_y: float
    gap: float  # stroke width in the tile's colour; the band between two sheets


# The full mark, the source for 48 px and up.
STACK = Stack(
    colours=["#7A4E0C", "#D08A1C", "#FFC85A"],
    width=520,
    height=380,
    radius=44,
    step_x=76,
    step_y=90,
    gap=20,
)
# The 16-32 px mark: two frames, larger, the band one pixel wide at 16 px. Every
# frame edge sits on a half pixel of the 16 px grid (a multiple of 64 units, plus
# 32), so the stroke centred on it fills whole pixels at 16 and 32 px: the fills
# span 2-10 and 6-14 px across, 3-9 and 7-13 px down.
SMALL_STACK = Stack(
    colours=["#D08A1C", "#FFC85A"],
    width=576,
    height=448,
    radius=96,
    step_x=256,
    step_y=256,
    gap=64,
)


class Frame(NamedTuple):
    x: float
    y: float
    width: float
    height: float
    radius: float
    gap: float
    colour: str


def fmt(value):
    text = f"{value:.2f}".rstrip("0").rstrip(".")
    return "0" if text in ("", "-0") else text


def rect(x, y, width, height, radius, attributes):
    return (
        f'<rect x="{fmt(x)}" y="{fmt(y)}" width="{fmt(width)}" height="{fmt(height)}" '
        f'rx="{fmt(radius)}" {attributes}/>'
    )


def extent(stack):
    """Width and height of the whole stack, frame edges to frame edges."""
    count = len(stack.colours)
    return (
        stack.width + (count - 1) * stack.step_x,
        stack.height + (count - 1) * stack.step_y,
    )


def place(stack, scale, centre):
    """The frames, back to front, scaled and with the stack's extent centred on `centre`."""
    width, height = extent(stack)
    left = centre - width * scale / 2
    top = centre - height * scale / 2
    return [
        Frame(
            left + index * stack.step_x * scale,
            top + index * stack.step_y * scale,
            stack.width * scale,
            stack.height * scale,
            stack.radius * scale,
            stack.gap * scale,
            colour,
        )
        for index, colour in enumerate(stack.colours)
    ]


def icon(inset, stack):
    """A tile inset from the canvas edge, the frames scaled with it."""
    side = CANVAS - 2 * inset
    scale = side / CANVAS
    body = [rect(inset, inset, side, side, TILE_RADIUS * scale, f'fill="{TILE_COLOUR}"')]
    for frame in place(stack, scale, CANVAS / 2):
        body.append(
            rect(
                frame.x,
                frame.y,
                frame.width,
                frame.height,
                frame.radius,
                f'fill="{frame.colour}" stroke="{TILE_COLOUR}" stroke-width="{fmt(frame.gap)}"',
            )
        )
    return body


def mark(stack):
    """The frames alone on a transparent canvas, the same sheets as on the tile.

    On the tile, a stroke of width `gap` centred on each frame's edge hides half
    the gap inside the frame and half outside it. Here each frame is drawn shrunk
    by half the gap, and the frames in front of it, grown by half the gap, are
    masked out of it: the same shapes, with nothing painted in the bands.
    """
    width, height = extent(stack)
    visible = max(width - stack.gap, height - stack.gap)
    frames = place(stack, (CANVAS - 2 * MARK_MARGIN) / visible, CANVAS / 2)
    body = []
    for index, frame in enumerate(frames):
        half = frame.gap / 2
        attributes = f'fill="{frame.colour}"'
        in_front = frames[index + 1 :]
        if in_front:
            body.append(f'<mask id="behind-{index}">')
            body.append("  " + rect(0, 0, CANVAS, CANVAS, 0, 'fill="#fff"'))
            for other in in_front:
                body.append(
                    "  "
                    + rect(
                        other.x - half,
                        other.y - half,
                        other.width + other.gap,
                        other.height + other.gap,
                        other.radius + half,
                        'fill="#000"',
                    )
                )
            body.append("</mask>")
            attributes += f' mask="url(#behind-{index})"'
        body.append(
            rect(
                frame.x + half,
                frame.y + half,
                frame.width - frame.gap,
                frame.height - frame.gap,
                frame.radius - half,
                attributes,
            )
        )
    return body


def write(name, body):
    lines = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {CANVAS} {CANVAS}">']
    lines += [f"  {element}" for element in body]
    lines.append("</svg>")
    (HERE / name).write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"{name}: {len(body)} lines")


# Full-bleed tile: Linux hicolor, Windows .ico, the window icon.
write("logo.svg", icon(0, STACK))
# macOS draws no mask of its own: Apple's grid puts an 824 px tile in a 1024 canvas.
write("logo-macos.svg", icon(MACOS_INSET, STACK))
# Two larger frames so 16-32 px icons stay legible.
write("logo-small.svg", icon(0, SMALL_STACK))
# The mark alone, transparent, for use inside the app on any background.
write("logo-mark.svg", mark(STACK))
