# EXIF fixture coverage

How far our EXIF + MakerNote reader (`crates/core/src/exif/`) has been checked
against real camera files, and against what. The reference is ExifTool 13.59.
The check is `crates/core/tests/exif_golden.rs`.

## Result

All 189 files agree with ExifTool on every compared field.

| Root | Files | Agree with ExifTool |
|---|---|---|
| Public corpus (`fixtures`, 51 files, sources below) | 51 | 51 |
| User-supplied Canon EOS 2000D folder (`user-2000d`), camera-original JPEGs with MakerNotes | 36 | 36 |
| User-supplied Canon EOS 6D folder (`user-6d`), Capture One exports with no MakerNotes | 102 | 102 |

Running the test against the real files found no parser bugs. To check that
the test can fail, the golden was edited by hand nine times, one edit per kind
of comparison: capture time (SubSec), exposure time, orientation, lens, UTC
offset, Canon EV, Sony shot number, Nikon ShootingMode, and a file missing from
the golden. That one run reported exactly those nine mismatches; it
is not repeated by the test suite. The checks of fields that were not edited,
such as Make, Model, FNumber, ISO or ExposureMode, were not shown to fire.

### What is compared

These rules are in `exif_golden.rs`.

- **Standard fields.** Each must equal ExifTool's value in the same group:
  `IFD0:` for Make, Model and Orientation, and `ExifIFD:` for the rest.
  ExifTool reports a CR3's CMT1 as IFD0 and its CMT2 as ExifIFD.
  - Strings are trimmed.
  - Numbers must agree within 1e-3 absolute and 1e-5 relative. Exposure times
    are far below 1e-3, so the absolute bound alone would accept a wrong one.
  - The capture time must agree to the millisecond, SubSecTimeOriginal
    included.
  - The UTC offset (OffsetTimeOriginal) must agree.
  - Orientation is compared as a number, and counts as 1 when it is absent
    or outside 1–8.
- **Bracket.** `mode`, `shot_number` and `value_ev` must equal our documented
  vendor mapping, applied to ExifTool's numeric (`-n`) MakerNote values. The
  mapping is in the doc comments of `makernote/{canon,nikon,sony,fujifilm}.rs`.
  When no vendor field decides, the expected value is the EXIF
  ExposureMode = 2 fallback. `value_ev` must agree within 1e-3.

  A wrong offset, framing, byte order or base in our MakerNote location
  therefore shows up as a mismatch on the file. Every file with a vendor
  note has an expected decision, so none can pass by finding nothing.

## Per-brand coverage

Columns:

- **Single shots read OK:** real files with bracketing off, plus single frames of real brackets.
- **Bracket-ON set read OK:** a real, complete sequence.
- **On-path:** whether the vendor bracket-ON code path ran on real bytes, or only on synthetic testkit bytes.

| Brand | Single shots read OK | Bracket-ON set read OK | On-path |
|---|---|---|---|
| Canon | **Yes.**<br>• CR2: 5D Mk IV, 400D, G1 X Mk III (an AEB frame, FileInfo AEB)<br>• CR3 (CMT1–3): R6 Mk II, R6 Mk III, R10 (portrait)<br>• JPEG: R6<br>• 36 user 2000D JPEGs<br>• 102 user 6D exports (no MakerNote) | **Yes.** Empa 450D CR2 ×3 (0/−2/+2 EV) and 1D Mk IV CR2 ×7 (−6…+6 EV, portrait), both through FileInfo BracketMode = AEB | **Real:** FileInfo AEB with ShotInfo Off (450D, 1D Mk IV, 2000D JPEG); ShotInfo AEB On with FileInfo Off (400D); both On (G1 X Mk III); ShotInfo AEBBracketValue EV.<br>**Synthetic-only:** CR3 AEB; ShotInfo with an unreadable FileInfo; a ShotInfo shot number (1–3); FEB/ISO/WB (Other); big-endian notes |
| Nikon | **Yes.** NEF: D80 and D5 (AEB frames), Z 30 (portrait), Z 8 (HE\*) | **Yes.** Empa D200 NEF ×5 and D200 JPEG ×7; Commons D80 JPEG ×3 | **Real:** ShootingMode bit 4 + ExposureBracketValue; type-3 notes with **both** embedded byte orders (`MM`: D80/D200; `II`: D5, Z 30, Z 8).<br>**Synthetic-only:** WB / Active D-Lighting bracketing (Other) |
| Sony | **Yes.** ARW: ILCE-7M4, ILCE-6400A (portrait), RX100M3 (AEB frame, seq 5), RX100M6 (AEB frame, seq 4). JPEG: RX100M3 ×2 (one portrait) | **Partial.** ILCE-7RM3 ×4 is Pixel Shift, not AEB (see below). No true AEB set exists under a free licence | **Real:** ReleaseMode 5 + SequenceNumber (two real AEB frames, and the Pixel Shift set); `SONY DSC` header.<br>**Synthetic-only:** WB/DRO bracketing (Other); the header-less IFD |
| Fujifilm | **Yes.** RAF: X-H2 (portrait), X-T50, X-S20. JPEG: X-T3 ×2 (one portrait) | **No.** No free AEB set or frame was found | **Real:** AutoBracketing 0 (Off) and 1, but the 1s are bursts (see below); `FUJIFILM` header in RAF and JPEG.<br>**Synthetic-only:** a true AEB sequence; flash/no-flash and pixel shift (Other) |

The ExposureMode = 2 fallback (no vendor decision) is **synthetic-only**.
Every real file with ExposureMode 2 also carries a vendor decision, and that
decision takes precedence.

## Known misclassifications (kept in M1)

The golden test passes on these files: it asserts our documented mapping.
It prints them as `KNOWN (R19)` notes.

1. **Fujifilm burst with AutoBracketing on.** The X-T50 and X-S20 RAFs read
   AutoBracketing (0x1100) = 1 and SequenceNumber = 1, but DriveSettings
   (0x1103) DriveMode = Continuous Low. They are a burst, and we report them
   as `Exposure`, shot 1.
2. **Sony Pixel Shift.** The ILCE-7RM3 set reads ReleaseMode (0xB049) = 5
   (Exposure Bracketing) and SequenceNumber 1–4, but its PixelShiftInfo is
   "Shot n/4": four identical exposures. We report it as `Exposure`, shots 1–4.

These are related observations, not misclassifications of the M1 mapping:

- **Canon flags disagree, so any "on" signal wins.** Neither table alone
  is reliable on real files:
  - The 400D frame has FileInfo BracketMode = Off, but ShotInfo
    AutoExposureBracketing = On (−1) and EXIF ExposureMode = Auto bracket.
  - The 450D and 1D Mk IV sets are the opposite: FileInfo AEB, ShotInfo
    Off.

  The mapping is therefore Exposure when either field says AEB, and Off only
  when every readable field says off. Both cases read as `Exposure`; the 400D
  has no shot number, because −1 is not a position.
  - One frame of the user's 2000D folder reads FileInfo AEB with shot 0 and
    EV 0, while ShotInfo says AEB off. It still maps to `Exposure`, with no
    shot number and no EV, like the 0 EV frame of the 450D set.
- **Canon BracketShotNumber is 0 on every real frame.** We map 0 to no shot
  number, so Stage 1 buckets these frames by the time window. The per-frame
  offset is only in AEBBracketValue.
- **Sony and Fujifilm `value_ev` is always `None`.** No unencrypted vendor
  tag records it. Sony AEB frames do carry the offset in EXIF
  ExposureCompensation, which the bracket does not use.
- **Commons D80 JPEGs** were losslessly rotated by the uploader: IFD0
  Orientation = 1, while the Nikon MakerNote says Rotate 270 CW. We read IFD0,
  as ExifTool does.

## Sources, licences and attribution

The public corpus is listed in `scripts/fixtures.tsv`: path, sha256, URL and
licence for each file. The fixtures sit outside the repository and are never
committed or redistributed; we download them only for testing.

- **raw.pixls.us**, CC0 1.0: the `rawsamples/` RAW files and the ILCE-7RM3
  Pixel Shift set.
- **Empa HDR Image Database**, Empa, doi:10.5281/zenodo.7861544, CC BY 4.0:
  - the 450D and 1D Mk IV CR2 sets
  - the D200 NEF and JPEG sets
- **Wikimedia Commons**:
  - Lacroix-1972-Farbleiter-4632/33/34 (D80): Welt-der-Form, CC BY-SA 3.0.
  - RX100M3 JPEGs: Tdorante10, CC BY-SA 4.0.
  - X-T3 JPEGs: Dktue, CC0.
  - R6 JPEG: Governo do Estado de São Paulo, via Flickr, CC BY 2.0.
- **User-supplied Canon 2000D and 6D folders**: private, read in place, never
  copied or committed. Their golden entries live only in the golden file
  outside the repository.

## How to reproduce

1. **Fetch the public corpus** into a folder outside the repository. The
   script needs bash, curl, sha256sum and python3 (Linux, macOS, or WSL):

   ```sh
   export PHOTOSTACKER_FIXTURES=<fixtures folder>
   scripts/fetch-raw-samples.sh            # or: scripts/fetch-raw-samples.sh DEST [PATH-REGEX]
   ```

   - Files that are already present with the right sha256 are skipped.
   - Every download is verified before it is kept.
   - The Empa files are single entries of 80–210 MB Zenodo zips. They are
     extracted with HTTP Range requests (`scripts/zip-range-extract.py`,
     which checks CRC32).
   - Requests to Wikimedia are spaced out and send a descriptive User-Agent.
   - The script refuses a destination inside the repository.

2. **Generate the golden,** where ExifTool 13.59 is on the PATH:

   ```sh
   python scripts/gen_exif_golden.py fixtures=<fixtures folder> \
       user-2000d=<2000D folder> user-6d=<6D folder>
   ```

   It writes `<fixtures>/golden/exif-golden.json`, which holds:
   - the ExifTool version
   - one entry per root label
   - per file, keyed by its path relative to the root, the `-j -n -G1 -a`
     values of the compared tags

   The user roots are optional.

3. **Run the test:**

   ```sh
   PHOTOSTACKER_FIXTURES=<fixtures folder> \
   PHOTOSTACKER_USER_PHOTOS_2000D=<2000D folder> \
   PHOTOSTACKER_USER_PHOTOS_6D=<6D folder> \
   just test -E 'test(exif_golden)' --no-capture
   ```

   - Without `PHOTOSTACKER_FIXTURES`, the test prints
     `SKIPPED: PHOTOSTACKER_FIXTURES not set` and passes, so `just check`
     needs no fixtures.
   - A user root whose variable is unset is skipped with a message.
   - If the golden doesn't list exactly the files on disk, the test fails as
     stale.
