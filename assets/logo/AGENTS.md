# AGENTS.md: assets/logo

How the app's logo and icons are made. The rules in the root [AGENTS.md](../../AGENTS.md) still apply.

**Prime rule: the `*.svg` masters here and everything in `../icons/` are build outputs.** Never hand-edit them. Change a constant in `build_logo.py` and rerun; the two scripts are the single source of truth.

## The mark
Three frames stacked on a dark rounded tile, going from dark at the back to bright at the front: an exposure bracket turned into layers, a warm amber accent on a dark neutral background. There is no raster source: `build_logo.py` draws every shape as an exact `<rect>` from the constants at its top, in a 1024-unit space, under 1 KB per master.

- Tile: a rounded square, corner radius 212, `#16181D`.
- Frames: 520 x 380, corner radius 44, back `#7A4E0C`, middle `#D08A1C`, front `#FFC85A`; each one 76 right of and 90 below the one behind it, the stack centred on the tile.
- Each frame has a 20-unit stroke in the tile's colour, so a band of tile separates it from the frame behind and the frames read as separate sheets.
- The small variant (16 to 32 px) keeps two frames only (`#D08A1C` behind, `#FFC85A` in front), larger and with a band one pixel wide at 16 px, because three frames blur together at that size. Its edges sit on half pixels of the 16 px grid, so the band falls on whole pixels at 16 and 32 px.
- The transparent mark has no tile to paint the bands with: it draws each frame shrunk by half the band and masks the frames in front of it out of it, so the bands stay transparent on any background.

The offsets, sizes and band width may be tuned by eye. The idea may not: three frames, dark at the back to bright at the front, amber on the dark tile.

## Toolchain
| Tool | Install | Role |
|---|---|---|
| resvg | `cargo install resvg` | SVG to PNG |
| uv + Pillow | `uv` on PATH | generating, ICO/ICNS writing: `uv run --with pillow python <script>.py` |

## Pipeline
```sh
cd assets/logo
uv run --with pillow python build_logo.py       # writes the four SVG masters
uv run --with pillow python export_icons.py     # writes ../icons/ and build/compare.png
```
After every run, look at `build/compare.png`: the four masters, then 16, 24, 32, 48 and 64 px at 1x and enlarged 4x. The 16 px icon must read as two sheets, the 256 px one as three.

## Outputs
| File | Use |
|---|---|
| `logo.svg` | full-bleed tile: Linux `hicolor/scalable`, the source for 48 px and up |
| `logo-small.svg` | two-frame variant, the source for 16, 24 and 32 px |
| `logo-macos.svg` | 824 px tile in a 1024 canvas, Apple's icon grid (macOS applies no mask of its own) |
| `logo-mark.svg` | the frames alone, transparent, for use inside the app |
| `../icons/icon-<n>.png` | 16 to 1024 px: hicolor sizes and the window icon |
| `../icons/icon.ico` | Windows, 16 to 256 px, each frame from its own render |
| `../icons/icon.icns` | macOS, from `logo-macos.svg` |

`build/` is disposable and gitignored.
