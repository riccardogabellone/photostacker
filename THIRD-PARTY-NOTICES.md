# Third-party notices

This project contains code ported from, and assets taken from, the following
third-party projects. Each of the first three sections (Pillow, Inter,
fastframe and ZapFast) names the original files, where they live in this tree,
and the original licence, reproduced verbatim.

The sections after them reproduce no licence text. The imagehash section
credits a method this project follows, from which no code is ported. The
project's packages also hold parts that are not in this tree, or that need
more than a licence text: from rawler on, each section says what the part is,
how it reaches the packages, where its source is, and where its licence text
is.
The licence of every Rust crate built into the app and the command-line tool,
with its text, is in `THIRD-PARTY-LICENSES.md`, which a package holds beside
this file in its `licenses` folder (the app's About card opens that folder).

## Pillow

Ported from Pillow 12.2.0 (https://github.com/python-pillow/Pillow):

| Original file | Ported to |
|---|---|
| `src/libImaging/Resample.c` | `crates/core/src/imageops.rs`: `pillow_resample`, `precompute_coeffs`, `resample_horizontal`, `resample_vertical`, `clip8`, `PRECISION_BITS`, the BICUBIC and LANCZOS kernels |
| `src/libImaging/Reduce.c` | `crates/core/src/imageops.rs`: `reduce`, `pillow_reciprocal` |
| `src/PIL/Image.py` (`Image.thumbnail`, `Image.resize`) | `crates/core/src/imageops.rs`: `thumbnail_size`, `round_aspect`, `reduce_factor`, the box arithmetic of `pillow_thumbnail` |

The licence below is Pillow's `LICENSE` file at tag 12.2.0, identical to the
first section of `pillow-12.2.0.dist-info/licenses/LICENSE` in the installed
wheel (the rest of that file covers libraries bundled in the wheel, none of
which is ported here).

```text
The Python Imaging Library (PIL) is

    Copyright © 1997-2011 by Secret Labs AB
    Copyright © 1995-2011 by Fredrik Lundh and contributors

Pillow is the friendly PIL fork. It is

    Copyright © 2010 by Jeffrey 'Alex' Clark and contributors

Like PIL, Pillow is licensed under the open source MIT-CMU License:

By obtaining, using, and/or copying this software and/or its associated
documentation, you agree that you have read, understood, and will comply
with the following terms and conditions:

Permission to use, copy, modify and distribute this software and its
documentation for any purpose and without fee is hereby granted,
provided that the above copyright notice appears in all copies, and that
both that copyright notice and this permission notice appear in supporting
documentation, and that the name of Secret Labs AB or the author not be
used in advertising or publicity pertaining to distribution of the software
without specific, written prior permission.

SECRET LABS AB AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
SOFTWARE, INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS.
IN NO EVENT SHALL SECRET LABS AB OR THE AUTHOR BE LIABLE FOR ANY SPECIAL,
INDIRECT OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE
OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

## Inter

The UI typeface is Inter 4.1 (font version 4.001, https://github.com/rsms/inter):

| Original file | In this tree |
|---|---|
| `InterVariable.ttf` from the `Inter-4.1.zip` release | `assets/fonts/InterVariable.ttf`, with its `tnum` feature frozen into the character map (see `assets/fonts/README.md`); names, axes and metrics are unchanged |

The licence below is Inter's licence (SIL Open Font License 1.1), reproduced
verbatim from `assets/fonts/Inter-LICENSE.txt`, which ships beside the font.

```text
Copyright (c) 2016 The Inter Project Authors (https://github.com/rsms/inter)

This Font Software is licensed under the SIL Open Font License, Version 1.1.
This license is copied below, and is also available with a FAQ at:
http://scripts.sil.org/OFL

-----------------------------------------------------------
SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007
-----------------------------------------------------------

PREAMBLE
The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded,
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION AND CONDITIONS
Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION
This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.
```

## fastframe and ZapFast

The desktop app's theme follows fastframe (https://github.com/crmne/fastframe)
and ZapFast (https://github.com/crmne/zapfast), by the same author, closely
enough to carry their notice:

| Original | Followed in |
|---|---|
| fastframe-theme: the colour roles and the recipe that derives them from a background, a text colour and an accent | `crates/ui/src/theme.rs`: `Palette`, its derivation, the role names |
| fastframe-text: the text rendering matched to the desktop | `crates/ui/src/theme.rs`: the text-rendering adjustments |
| ZapFast's `src/theme.rs`: the mapping of the roles onto egui's visuals | `crates/ui/src/theme.rs`: the widget states, outlines, selection, corner radii and spacing |

Both projects carry the same licence, the MIT licence below, reproduced
verbatim from their `LICENSE` files.

```text
MIT License

Copyright (c) 2026 Carmine Paolino

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## imagehash

The perceptual hashes (`crates/core/src/hash.rs`) follow the published method
of Python's imagehash (https://github.com/JohannesBuchner/imagehash, by
Johannes Buchner, under the BSD 2-Clause licence): its pHash and dHash, so
that a distance between two hashes means here what it means there. No code is
ported from it; the tests compare our hashes with the ones it computes
(`crates/core/tests/hash_parity.rs`).

## rawler

RAW photos are decoded by rawler, a Rust crate of the dnglab project
(https://github.com/dnglab/dnglab), built into the app and the command-line
tool. rawler and its use are covered by the GNU Lesser General Public License,
version 2.1. Its text ships beside this file as `LGPL-2.1.txt`, as the Free
Software Foundation publishes it, and in `THIRD-PARTY-LICENSES.md` as rawler's
own copy. The version built in is the one `Cargo.lock` pins.

The LGPL lets you use a changed copy of rawler with Photostacker.
Photostacker's complete source is public under the MIT licence, at
https://github.com/riccardogabellone/photostacker, and builds with the
commands its `AGENTS.md` documents, so the app can be rebuilt with your copy
of rawler in place of the original (a `[patch.crates-io]` entry in
`Cargo.toml` points cargo at it).

## libheif and libde265

HEIC and HEIF photos are decoded by libheif
(https://github.com/strukturag/libheif), whose HEVC decoder is libde265
(https://github.com/strukturag/libde265). Both libraries and their use are
covered by the GNU Lesser General Public License, version 3 or (at your
option) any later version. That licence is
written as additions to the GNU General Public License, version 3, so both
texts ship beside this file, as the Free Software Foundation publishes them:
`LGPL-3.0.txt` and `GPL-3.0.txt`.

How each reaches the packages:

| Library | Linux | Windows | macOS |
|---|---|---|---|
| libheif | built into the app and the command-line tool, from the source the `libheif-sys` crate carries | built in, from vcpkg's `libheif` port with libde265 as its only codec (`libheif[core,libde265]`) | built in, from the source the `libheif-sys` crate carries |
| libde265 | the system's shared library, `libde265.so.0`: the Debian package depends on it, and the AppImage carries a copy | built in, from vcpkg | built in |

On Linux and macOS the libheif built in is the one the `libheif-sys` version
in `Cargo.lock` carries (its version is the part after the `+`). libheif is
built with libde265 as its only codec, so no codec under the GPL, such as x265
or x264, is in the packages.

The LGPL lets you use changed copies of these libraries with Photostacker.
Where a library is built in, the app is rebuilt with your copy in its place:
Photostacker's complete source is public under the MIT licence, at
https://github.com/riccardogabellone/photostacker, and builds with the
commands its `AGENTS.md` documents. Case by case:

- **libheif on Linux and macOS** is built from the source inside the
  `libheif-sys` crate: put your libheif in a copy of that crate, and point
  cargo at the copy with a `[patch.crates-io]` entry in `Cargo.toml`.
- **libheif on Windows** is built from vcpkg's `libheif` port: give vcpkg your
  libheif as that port, and build the app against it.
- **libde265 on Linux** is the system's shared library: put your copy in place
  of the system's `libde265.so.0` (or of the copy inside the AppImage), and
  the app loads it, with no rebuild.
- **libde265 on Windows and macOS** is built in: build your copy the same way
  the project builds libde265 for that system (on Windows as vcpkg's
  `libde265` port, on macOS as a static library), and build the app against
  it.

HEVC, the codec of HEIC photos, may be covered by patents in some countries:
decoding it is the same grey area GIMP and ImageMagick ship with.

## KDE's Wayland protocol files

This section concerns the Linux packages only: the crate it is about is not
built for Windows or macOS, nor into the command-line tool.

On Linux the desktop app includes Rust code that the
`wayland-protocols-plasma` crate (declared MIT, part of the wayland-rs
project, https://github.com/smithay/wayland-rs) generates from KDE's Wayland
protocol files. The crate is there for Wayland: winit, the window library under the
app's toolkit (eframe), brings it in with its `wayland` feature. The version
built in is the one `Cargo.lock` pins.

The crate carries KDE's plasma-wayland-protocols
(https://invent.kde.org/libraries/plasma-wayland-protocols; the crate itself
names its mirror, https://github.com/KDE/plasma-wayland-protocols) and
generates code from all 30 protocol files in its folder
`plasma-wayland-protocols/src/protocols/`. Their authors mark 17 of them
`LGPL-2.1-or-later`: `appmenu.xml`, `blur.xml`, `contrast.xml`, `dpms.xml`,
`fake-input.xml`, `idle.xml`, `kde-lockscreen-overlay-v1.xml`, `keystate.xml`,
`org-kde-plasma-virtual-desktop.xml`, `plasma-shell.xml`,
`plasma-window-management.xml`, `remote-access.xml`,
`server-decoration-palette.xml`, `server-decoration.xml`, `shadow.xml`,
`slide.xml` and `zkde-screencast-unstable-v1.xml`. Of the other 13, ten are
marked `MIT-CMU`, one `BSD-3-Clause` (`surface-extension.xml`), one `MIT`
(`wayland-eglstream-controller.xml`), and one carries no licence marking
(`fullscreen-shell.xml`). The texts of those licences are in the crate's
`plasma-wayland-protocols/LICENSES/` folder; the LGPL-2.1's text ships beside
this file as `LGPL-2.1.txt`.

To use changed protocol files with Photostacker, rebuild the app with a copy
of `wayland-protocols-plasma` that holds them: Photostacker's complete source
is public under the MIT licence, at
https://github.com/riccardogabellone/photostacker, and builds with the
commands its `AGENTS.md` documents; a `[patch.crates-io]` entry in
`Cargo.toml` points cargo at your copy.

## ONNX Runtime

The visual check runs its model on Microsoft's ONNX Runtime
(https://github.com/microsoft/onnxruntime), under the MIT licence. It is a
shared library the package carries, unchanged: beside the executable on
Windows, in `usr/lib/Photostacker` on Linux, in the app's `Frameworks` folder
on macOS. The build each system gets is pinned in `scripts/onnxruntime.tsv`.
Its own `LICENSE` and `ThirdPartyNotices.txt` ship beside this file as
`onnxruntime-LICENSE.txt` and `onnxruntime-ThirdPartyNotices.txt`.

## The Visual C++ runtime

The Windows packages carry `msvcp140.dll`, `msvcp140_1.dll`,
`vcruntime140.dll` and `vcruntime140_1.dll` beside the app, unchanged: the
redistributable files of Microsoft's Visual C++ runtime, which ONNX Runtime
and the app need. They remain Microsoft's, redistributed under the licence
terms of Microsoft Visual Studio.

## Material Symbols

The icons inside the app's window are Google's Material Symbols
(https://github.com/google/material-design-icons), under the Apache License
2.0: the font `MaterialSymbolsRounded_Filled-Regular.ttf`, which the
`egui_material_icons` crate embeds in the app. That crate is under the MIT
licence (listed in `THIRD-PARTY-LICENSES.md`) and carries no licence file for
the font. The Apache License 2.0's text is in `THIRD-PARTY-LICENSES.md`, and
at https://www.apache.org/licenses/LICENSE-2.0.

## The DINOv2 models

The visual check's models, DINOv2 small and DINOv2 base, are Meta's DINOv2
(https://github.com/facebookresearch/dinov2) as Xenova converted them to ONNX
on Hugging Face, under the Apache License 2.0. They are not in the packages:
the app or the command-line tool downloads one only when the user asks, from
the addresses in the model registry (`crates/core/src/models/registry.rs`), or
imports a file the user chooses.
