"""Generate the EXIF golden file of the EXIF reader's golden test.

Usage, from the repo root (standard library only; ExifTool must be on the PATH)::

    python scripts/gen_exif_golden.py fixtures=<fixtures folder> \\
        user-2000d=<folder of Canon 2000D files> user-6d=<folder of Canon 6D files>

Each argument is ``<label>=<folder>``. The label names the root in the golden
file; the Rust test (``crates/core/tests/exif_golden.rs``) maps each label to
an environment variable that holds the same folder, as the machine that runs
the tests sees it:

* ``fixtures``   -> ``PHOTOSTACKER_FIXTURES``
* ``user-2000d`` -> ``PHOTOSTACKER_USER_PHOTOS_2000D``
* ``user-6d``    -> ``PHOTOSTACKER_USER_PHOTOS_6D``

The output goes to ``<fixtures root>/golden/exif-golden.json`` (``--out``
overrides it). It sits outside the repository: it names private files, so it
is never committed.

The reference is the ExifTool on the PATH (13.59 when the golden was made).
Every image file under each root (the photostacker extensions, recursively,
skipping folders named ``golden`` or starting with ``_``) is read with::

    exiftool -j -n -G1 -a <TAGS>

``-G1`` keeps the family-1 group in each key (``IFD0:Make``,
``ExifIFD:FNumber``, ``Canon:BracketMode``), so a MakerNote value or a
Composite tag can never stand in for the standard EXIF tag of the same name.
``-n`` gives the numeric (ValueConv) values that the Rust test maps with our
documented vendor rules. The JSON is parsed with the original number text, so
string tags keep their exact spelling (a SubSecTimeOriginal of ``05`` or a
LensModel that looks like a number stays a string).

Per file, the golden keeps only the requested tags that ExifTool found:

    {"exiftool_version": "13.59",
     "roots": {"<label>": {"files": {"<relative/path>": {"IFD0:Make": "Canon", ...}}}}}
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

# The photostacker extensions (lowercase, no dot): stills, then RAW.
STILL_EXTENSIONS = ["jpg", "jpeg", "png", "tif", "tiff", "heic", "heif"]
RAW_EXTENSIONS = ["cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2"]
RAW_EXTENSIONS += ["pef", "dng", "srw", "x3f", "3fr", "iiq", "erf", "kdc", "mrw", "mos"]
EXTENSIONS = {*STILL_EXTENSIONS, *RAW_EXTENSIONS}

# Standard tags, in the groups our reader takes them from. For a CR3, ExifTool
# reports CMT1 as IFD0 and CMT2 as ExifIFD.
STRING_TAGS = [
    "IFD0:Make",
    "IFD0:Model",
    "ExifIFD:LensModel",
    "ExifIFD:DateTimeOriginal",
    "ExifIFD:SubSecTimeOriginal",
    "ExifIFD:OffsetTimeOriginal",
]
NUMERIC_TAGS = [
    "IFD0:Orientation",
    "ExifIFD:FocalLength",
    "ExifIFD:ExposureTime",
    "ExifIFD:FNumber",
    "ExifIFD:ISO",
    "ExifIFD:ExposureCompensation",
    "ExifIFD:ExposureMode",
    # Canon: FileInfo (0x0093) words 3 and 5, ShotInfo (0x0004) words 16 and 17.
    "Canon:BracketMode",
    "Canon:BracketShotNumber",
    "Canon:AEBBracketValue",
    "Canon:AutoExposureBracketing",
    # Nikon: 0x0089 and 0x0019.
    "Nikon:ShootingMode",
    "Nikon:ExposureBracketValue",
    # Sony: 0xB049 and 0xB04A.
    "Sony:ReleaseMode",
    "Sony:SequenceNumber",
    # Fujifilm: 0x1100, 0x1101, and DriveMode from 0x1103 (trap: a burst with
    # AutoBracketing on).
    "FujiFilm:AutoBracketing",
    "FujiFilm:SequenceNumber",
    "FujiFilm:DriveMode",
]
# Informational only (trap: Sony Pixel Shift reuses ReleaseMode 5).
TEXT_INFO_TAGS = ["Sony:PixelShiftInfo"]

ALL_TAGS = STRING_TAGS + NUMERIC_TAGS + TEXT_INFO_TAGS
EXIFTOOL_ARGS = ["-j", "-n", "-G1", "-a", "-charset", "filename=utf8"]


def image_files(root: Path) -> list[Path]:
    files = []
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root)
        if any(part == "golden" or part.startswith("_") for part in rel.parts[:-1]):
            continue
        if path.is_file() and path.suffix[1:].lower() in EXTENSIONS:
            files.append(path)
    return files


def numeric(text: str) -> int | float | str:
    """ExifTool's number text as a number; other text (``inf``, ``undef``) unchanged."""
    try:
        return int(text)
    except ValueError:
        pass
    try:
        return float(text)
    except ValueError:
        return text


def no_duplicates(pairs: list[tuple[str, object]]) -> dict:
    out: dict = {}
    for key, value in pairs:
        if key in out and out[key] != value:
            raise SystemExit(f"duplicate tag {key} with different values: {out[key]!r} vs {value!r}")
        out[key] = value
    return out


def run_exiftool(files: list[Path]) -> list[dict]:
    """ExifTool's JSON for `files`, with every number kept as its text."""
    with tempfile.TemporaryDirectory() as tmp:
        argfile = Path(tmp) / "args.txt"
        lines = EXIFTOOL_ARGS + [f"-{tag}" for tag in ALL_TAGS] + [str(f) for f in files]
        argfile.write_text("\n".join(lines) + "\n", encoding="utf-8")
        result = subprocess.run(["exiftool", "-@", str(argfile)], capture_output=True, check=False)
    if result.returncode not in (0, 1) or not result.stdout:
        raise SystemExit(f"exiftool failed ({result.returncode}): {result.stderr.decode(errors='replace')}")
    if result.stderr.strip():
        print(result.stderr.decode(errors="replace").strip(), file=sys.stderr)
    return json.loads(
        result.stdout.decode("utf-8"),
        parse_int=str,
        parse_float=str,
        object_pairs_hook=no_duplicates,
    )


def golden_tags(record: dict) -> dict:
    tags = {}
    for tag in STRING_TAGS + TEXT_INFO_TAGS:
        if tag in record:
            tags[tag] = str(record[tag])
    for tag in NUMERIC_TAGS:
        if tag in record:
            tags[tag] = numeric(str(record[tag]))
    return tags


def exiftool_version() -> str:
    out = subprocess.run(["exiftool", "-ver"], capture_output=True, check=True, text=True)
    return out.stdout.strip()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("roots", nargs="+", metavar="LABEL=FOLDER")
    parser.add_argument("--out", type=Path, help="default: <fixtures root>/golden/exif-golden.json")
    args = parser.parse_args()

    roots: dict[str, Path] = {}
    for spec in args.roots:
        label, sep, folder = spec.partition("=")
        if not sep or not label or not folder:
            parser.error(f"expected LABEL=FOLDER, got {spec!r}")
        path = Path(folder)
        if not path.is_dir():
            parser.error(f"{label}: {folder} is not a folder")
        roots[label] = path.resolve()
    out = args.out
    if out is None:
        if "fixtures" not in roots:
            parser.error("--out is required without a 'fixtures' root")
        out = roots["fixtures"] / "golden" / "exif-golden.json"

    golden: dict = {"exiftool_version": exiftool_version(), "exiftool_args": EXIFTOOL_ARGS, "roots": {}}
    for label, root in roots.items():
        files = image_files(root)
        if not files:
            raise SystemExit(f"{label}: no image files under {root}")
        records = run_exiftool(files)
        by_key = {}
        for record in records:
            rel = Path(record["SourceFile"]).resolve().relative_to(root).as_posix()
            by_key[rel] = golden_tags(record)
        expected = {f.relative_to(root).as_posix() for f in files}
        if set(by_key) != expected:
            raise SystemExit(f"{label}: exiftool skipped {sorted(expected - set(by_key))}")
        golden["roots"][label] = {"files": dict(sorted(by_key.items()))}
        print(f"{label}: {len(by_key)} files")

    out.parent.mkdir(parents=True, exist_ok=True)
    with open(out, "w", encoding="utf-8", newline="\n") as f:
        json.dump(golden, f, indent=1, sort_keys=True, ensure_ascii=False)
        f.write("\n")
    print(f"ExifTool {golden['exiftool_version']} -> {out}")


if __name__ == "__main__":
    main()
