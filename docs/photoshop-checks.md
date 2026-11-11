# Photoshop checks

What other programs make of the files the writers produce (spec §12.5). There are two kinds of check:

- **The stand-ins:** GIMP, PhotoCraft and Photopea, three readers written independently of ours. A script opens every sample in each of them and compares what they read (see "The stand-ins"). They are run after every change to a writer.
- **Photoshop itself,** by hand, on Windows: once before the first public release (Adobe's trial, or someone who has Photoshop), then before each release. Only Photoshop can say whether Photoshop warns. The stand-ins say whether readers built from the specification, and from Photoshop's own files, agree with ours.

There is one table per checkpoint and one row per file:

1. the PSD/PSB encoder is done; this also decides the 16-bit layer compression;
2. the layered TIFF is done;
3. before each release.

Checkpoints 1 and 2 are run together, in one session, since both writers were done before either was checked.

## Making the set

At the repo root:

```sh
PHOTOSTACKER_FIXTURES=<fixtures folder> just ps-samples --bigtiff
```

- The recipe clears `dist/ps-check/` and writes the set into it. It removes only the regular files directly in that folder, and those directly in its `sources/` folder. It refuses any folder whose path does not end in `dist/ps-check`, any path with a link (or a `..`) in it, and a `sources` that is a link.
- The files two samples are stacked from stay in `dist/ps-check/sources/`, so they can be opened beside the stacks: the gray PNGs of `psd-8bit-gray-source` and the JPEGs of `psd-8bit-adobe-rgb`.
- Every file is read back before the recipe ends: PSDs and PSBs with testkit's strict PSD reader, and TIFFs with the `tiff` crate plus that reader's layer-block parser. Besides the header and the layer names, the check compares the embedded ICC profile with the one expected (none for the 8-bit synthetic samples and the gray sources, the generated Adobe RGB profile for the Adobe RGB sources), and a TIFF's ExtraSamples with its alpha. A profile Photostacker generates must also carry its name. One line per file gives its path, container, layers and size. The exit status is non-zero if any file fails a check.
- The real bracket is skipped, with a note, when `PHOTOSTACKER_FIXTURES` is not set. It is the public Empa D200 NEF ×5 set (Empa HDR Image Database, CC BY 4.0; see `docs/exif-fixture-coverage.md`). It is read in place and is never copied into the repo.
- `--bigtiff` adds the BigTIFF sample. It needs about 3 GB of memory and about a minute, so it is opt-in.

## The stand-ins

At the repo root, after the set is made. The script is written for Windows (it looks for GIMP and Edge in their usual folders and ends processes with `taskkill`) and needs Node 22 or later:

```sh
node scripts/standins/standins.mjs --photocraft <folder>/photocraft-cli.exe
```

What it needs:

- **GIMP 3.2** (`--gimp <gimp-console exe>` when it is not in its default folder). GIMP 3.0.0 is not usable: on the development machine its Python did not start, it showed an error on every layered TIFF, and it read the layers of a document with a non-sRGB profile as sRGB.
- **PhotoCraft's portable Windows build,** from its GitHub releases (github.com/storytold/photocraft; nothing is installed). `--photocraft` names its `photocraft-cli.exe`; without it that reader is skipped.
- **Edge or Chrome** for Photopea (`--browser <exe>` when neither is where it is expected), and a network connection, since Photopea is a web page. The samples stay on the machine: the script serves them on 127.0.0.1 to a headless browser, and Photopea reads them inside the browser.

What each reader is asked:

- **GIMP**, through its own Python, with no window:
  - the file opens;
  - the layers of "The files" are there, with their exact names, in order (a layered TIFF shows them above the TIFF's own image, which GIMP names `Background`);
  - every layer is visible, Normal, at 100 %, unlocked and without a mask;
  - the embedded profile has the name expected, or there is none; the resolution is 72 ppi;
  - every layer's stored samples, digested channel by channel, are the same in every container that holds the same pictures;
  - a PSD's or PSB's merged image, which GIMP can load without the layers, is the top layer: sample for sample without alpha, and within 2 where the top layer is at least half opaque once GIMP has undone the blend against white (item 6);
  - a TIFF's own image is the top layer;
  - a sample stacked from the files in `sources` holds their samples, as GIMP reads both: a gray source is the layer's red, green and blue, sample for sample; a JPEG source, which two decoders read, matches within 3 at the four points compared.
- **PhotoCraft**, through its command line:
  - `info`: the layers, names, order and flags, the depth and the resolution, and an empty `warnings` list;
  - `convert` to PNG: the same pixels from every container that holds the same pictures;
  - a re-save as PSD or PSB (files under 64 MB), whose merged image is compared with ours: equal where the top layer is opaque or clear, and within 1 wherever both store the same alpha. Elsewhere PhotoCraft also shows the layers below, and ours is the top layer alone (spec §5.6).
- **Photopea**, through its script interface:
  - the layers, names, order, flags and bounds, and the resolution;
  - its PNG export of an 8-bit sample without transparency is GIMP's top layer, sample for sample;
  - the same picture from every container.

The results stay in `dist/ps-check-standins/` (`gimp.jsonl`, `photocraft.json`, `photopea.json`); `--report` prints the report again from them, and `--only gimp,photocraft,photopea` runs some of the readers. The exit status is 1 when a reader disagrees with what is expected and the difference is not in the script's `KNOWN` list, which holds what a reader is known not to do (the findings below).

## The files

The synthetic samples all use one testkit scene, exposed per layer. The layers of `psd-8bit-gray-source` are that scene in 8-bit gray, written as PNG files with a Gray ICC profile and decoded the way the writers decode files. The layers of `psd-8bit-adobe-rgb` are that scene with a row of saturated patches (red, green, blue, cyan, magenta, yellow) across its upper part and a 16-step gray ramp across its lower part, written as JPEG files with no ICC profile and the EXIF of a DCF camera set to Adobe RGB, and decoded the way the writers decode files. The top layer is the first one listed below, and it is also the composite. The `−` in the names is U+2212, in the Unicode (`luni`) name; the Pascal fallback name uses an ASCII `-`.

| File | What it tests | Layers, top first |
|---|---|---|
| `psd-8bit-rle_3.psd` | PSD, 8-bit RGB, PackBits layers, no ICC profile | `psd-8bit-rle (0 EV)`, `psd-8bit-rle-2 (−1 EV)`, `psd-8bit-rle-3 (+1 EV)` |
| `psd-8bit-rgba_3.psd` | PSD, 8-bit RGBA, negative layer count. Each layer is opaque inside a soft ellipse that shrinks down the stack, and transparent in the corners. | `psd-8bit-rgba (0 EV)`, `psd-8bit-rgba-2 (−1 EV)`, `psd-8bit-rgba-3 (+1 EV)` |
| `psd-16bit-zip_3.psd` | PSD, 16-bit, ZIP-with-prediction layers (the default), `Lr16`, linear ICC profile | `psd-16bit-zip (0 EV)`, `psd-16bit-zip-2 (−1 EV)`, `psd-16bit-zip-3 (+1 EV)` |
| `psd-16bit-rle_3.psd` | the same with PackBits 16-bit layers (`Compression16::Rle`) | `psd-16bit-rle (0 EV)`, `psd-16bit-rle-2 (−1 EV)`, `psd-16bit-rle-3 (+1 EV)` |
| `psd-16bit-rgba_3.psd` | PSD, 16-bit RGBA: `Lr16` with a negative layer count, ZIP layers, linear ICC profile, the ellipses of `psd-8bit-rgba` | `psd-16bit-rgba (0 EV)`, `psd-16bit-rgba-2 (−1 EV)`, `psd-16bit-rgba-3 (+1 EV)` |
| `psd-8bit-gray-source_3.psd` | PSD, 8-bit RGB stacked from gray PNG sources that carry a Gray ICC profile: the document embeds no ICC profile | `psd-8bit-gray-source (0 EV)`, `psd-8bit-gray-source-2 (−1 EV)`, `psd-8bit-gray-source-3 (+1 EV)` |
| `psd-8bit-adobe-rgb_3.psd` | PSD, 8-bit RGB stacked from JPEG sources with no ICC profile whose EXIF marks them as Adobe RGB (ColorSpace 0xFFFF, Interop index `R03`): the document embeds the generated profile "Photostacker Adobe RGB (1998)" | `psd-8bit-adobe-rgb (0 EV)`, `psd-8bit-adobe-rgb-2 (−1 EV)`, `psd-8bit-adobe-rgb-3 (+1 EV)` |
| `psb-wide_2.psb` | PSB, 30,016 × 64 px (past the 30,000 px PSD limit), 8-bit, PSB widths | `psb-wide (0 EV)`, `psb-wide-2 (−1 EV)` |
| `psb-wide-16bit_2.psb` | the same at 16 bits: ZIP layers, `Lr16` with PSB widths (8-byte length), linear ICC profile | `psb-wide-16bit (0 EV)`, `psb-wide-16bit-2 (−1 EV)` |
| `tiff-8bit_3.tif` | layered TIFF, 8-bit, no ICC profile | `tiff-8bit (0 EV)`, `tiff-8bit-2 (−1 EV)`, `tiff-8bit-3 (+1 EV)` |
| `tiff-16bit_3.tif` | layered TIFF, 16-bit, ZIP layers, `Lr16`, linear ICC profile | `tiff-16bit (0 EV)`, `tiff-16bit-2 (−1 EV)`, `tiff-16bit-3 (+1 EV)` |
| `tiff-8bit-rgba_3.tif` | layered TIFF, 8-bit RGBA: ExtraSamples 2 (unassociated alpha), `Layr` with a negative layer count, the ellipses of `psd-8bit-rgba` | `tiff-8bit-rgba (0 EV)`, `tiff-8bit-rgba-2 (−1 EV)`, `tiff-8bit-rgba-3 (+1 EV)` |
| `bigtiff-16bit_2.tif` | BigTIFF (`--bigtiff` only): 15,000 × 15,000 px, 16-bit, estimate 4.05 GB, PSB widths, `V0002` signature | `bigtiff-16bit (0 EV)`, `bigtiff-16bit-2 (−1 EV)` |
| `DSC_0039_5.psd` | the real bracket as PSD: a 16-bit RAW develop in sRGB tone with Photostacker's sRGB profile | `DSC_0039 (0 EV)`, `DSC_0040 (−2 EV)`, `DSC_0041 (−1 EV)`, `DSC_0042 (+1 EV)`, `DSC_0043 (+2 EV)` |
| `DSC_0039_5.tif` | the real bracket as layered TIFF | the same as the PSD |

The 16-bit synthetic layers hold linear-light values and carry the linear profile. In a colour-managed viewer they should look like the 8-bit samples. If they look much brighter or flatter, the profile was ignored or misread.

## What to look at

For every file, check three things:

- **No warning on open.** This includes "some data could not be read", "the file may be damaged" and a prompt to flatten or discard layers. A colour-settings prompt, such as *Embedded Profile Mismatch* or *Missing Profile*, is not a file defect. Note it if it appears, but it does not fail the check.
- **Layers, names and order.** The Layers panel shows the layers in the table above, top first. Each has its exact Unicode name, is visible and unlocked, uses *Normal* blending, and has 100 % opacity and fill.
- **Composite and preview.** The composite is the top layer. You can see it in a viewer that reads only the composite (Explorer or Bridge thumbnails, the Windows Photos app for a TIFF), and in Photoshop's own preview when it opens the file.

Then check these items, which were written from the specification without a Photoshop file to compare against:

1. **Flags 0 and empty blending ranges.** Every layer record has flags 0 (visible, not protected) and an empty blending-ranges block (length 0).
   - Check that no layer is hidden or locked.
   - Check that *Layer Style → Blending Options → Blend If* shows the default full ranges (0–255 for Gray and for each channel).
2. **The `Lr16` length rounding.** This is in all 16-bit files: `psd-16bit-*` (`psd-16bit-rgba` included), `psb-wide-16bit`, `tiff-16bit`, `bigtiff-16bit` and the real bracket.
   - The `Lr16` length counts the data rounded up to even, but not the padding to 4 that follows. This matches Photoshop's own CS3 and CC 2017 files.
   - If 16-bit files lose their layers or warn while 8-bit ones do not, suspect this.
3. **The count-0 `curv` in the linear ICC profile.** This is in `psd-16bit-*` (`psd-16bit-rgba` included), `psb-wide-16bit`, `tiff-16bit` and `bigtiff-16bit`.
   - The TRC is an empty curve, which ICC defines as the identity.
   - *Edit → Assign Profile* (or the document-profile status bar) must show **Photostacker linear sRGB**, with no "profile is invalid / ignored" message.
   - The image must look like the 8-bit samples (see above).
4. **The BigTIFF `V0002` signature.** This is in `bigtiff-16bit_2.tif`.
   - Its ImageSourceData starts `Adobe Photoshop Document Data V0002` and carries the layers with PSB widths. This is inferred from ExifTool, not from Adobe's 2002 notes.
   - Record whether Photoshop opens a BigTIFF at all, and whether it shows the 2 layers or only the composite.
5. **No tag 34377 in the TIFF.** Every `.tif` has no Photoshop image-resources tag (the example asserts that it is absent). Photoshop must still show the layers, and *Image → Image Size* must show 72 ppi from X/YResolution.
6. **The negative layer count with alpha.** This is in `psd-8bit-rgba_3.psd`.
   - The count is −3: the merged image's alpha is its transparency.
   - Photoshop shows the transparency checkerboard in the corners. Each layer has its own soft ellipse, the largest on top.
   - The composite (thumbnail) is the top layer with transparent corners, not black or white ones.
   - The merged image stores the colour blended against white, Photoshop's convention for a document with transparency (spec §6.2), and a reader of the merged image undoes it. A reader that shows only the merged image (GIMP's "load merged" import, ImageMagick) must show the same soft edges as the top layer, not darker ones.
7. **The big-endian TIFF.** Every `.tif` is `MM` (Motorola byte order), because its layer blocks are big-endian.
   - Photoshop must reopen the layers, not a single *Background* layer.
   - Do not save over the samples: rerun `just ps-samples` to restore them.
8. **The negative layer count in `Lr16`.** This is in `psd-16bit-rgba_3.psd`.
   - The count in the `Lr16` block is −3, as in item 6 but at 16 bits.
   - Photoshop shows the same transparency as `psd-8bit-rgba_3.psd`: the checkerboard in the corners, and each layer's own soft ellipse, the largest on top.
   - As in item 6, the merged image stores the colour blended against white, at 16 bits. A reader of the merged image (GIMP's "load merged" import, ImageMagick) must show the same soft edges as the top layer, not darker ones.
9. **Alpha in a layered TIFF.** This is in `tiff-8bit-rgba_3.tif`.
   - The composite has 4 samples per pixel with ExtraSamples 2 (unassociated alpha), and the `Layr` block has the count −3 (the example asserts ExtraSamples).
   - Photoshop reopens the 3 layers with their transparency, as in `psd-8bit-rgba_3.psd`, not a single layer plus an alpha channel.
   - A composite-only viewer (the Windows Photos app, Explorer thumbnails) shows the top layer with transparent corners.
10. **No Gray profile in an RGB document.** This is in `psd-8bit-gray-source_3.psd`.
    - Its sources carry a Gray ICC profile, but their pixels are decoded to RGB, so the profile must not travel (the example asserts that the PSD has no ICC resource).
    - Photoshop opens it as an untagged 8-bit RGB document. A *Missing Profile* prompt is expected and is not a file defect; an *Embedded Profile Mismatch* naming a Gray profile, or any "profile is invalid" message, fails the check.
    - The layers look gray, like the scene of the other samples without its tint.
11. **The generated Adobe RGB profile.** This is in `psd-8bit-adobe-rgb_3.psd`.
    - Its sources, `sources/psd-8bit-adobe-rgb.jpg`, `-2.jpg` and `-3.jpg`, embed no ICC profile. Their EXIF marks them as Adobe RGB the way a DCF camera does (ColorSpace 0xFFFF, uncalibrated, with the Interop index `R03`), so the document embeds the profile Photostacker generates for them (spec §5.5). The example asserts that the document embeds exactly that profile and that the sources embed none.
    - Photoshop asks about profiles on open only when *Ask When Opening* is ticked in *Edit → Color Settings*. Whether it asks or not, the RGB policy there should be *Preserve Embedded Profiles* (Photoshop's default): a policy that converts to the working space changes the RGB values compared below.
    - Open the document. If Photoshop asks its usual *Embedded Profile Mismatch* question (your working space is not Adobe RGB (1998)), choose *Use the embedded profile (instead of the working space)*. That question is not a file defect; any other message about the profile, such as "the embedded profile is invalid" or that it is ignored, fails the check.
    - *Edit → Assign Profile* (or the document-profile status bar) shows **Photostacker Adobe RGB (1998)**.
    - Then open the top layer's source, `sources/psd-8bit-adobe-rgb.jpg`, directly. Photoshop may read its EXIF and open it as Adobe RGB (1998). If it asks the mismatch question, choose the embedded profile again; if it asks about a *Missing Profile*, choose *Assign profile: Adobe RGB (1998)*, which is what the EXIF means. Then look at *Edit → Assign Profile*: if it shows anything other than Adobe RGB (1998) (*Don't Color Manage This Document*, or your working space), assign **Adobe RGB (1998)** there before comparing. Comparing with a source read as sRGB says nothing.
    - The layers' names say `(0 EV)`, `(−1 EV)` and `(+1 EV)`, but those are nominal: the darker and brighter pictures are made by scaling the values as if they were sRGB, which is not an exact change of light in Adobe RGB. The comparison below uses each layer's own source file, so this does not matter for it.
    - Compare the two documents on the colour patches (red, green, blue, cyan, magenta, yellow) and on the gray ramp, first by eye, then with the Info panel on the same patch of each. Its RGB values match within a level or two (both hold the same numbers, up to JPEG decoding). Set its second readout to *Lab Color*: the Lab values match within a unit or two too. They describe the colour under each document's profile, so they show a mix-up that the RGB values and an sRGB display can hide: a document read as sRGB gives the green and cyan patches an a\* tens of units less negative than its source.

**Decision for checkpoint 1.** If `psd-16bit-zip_3.psd` warns or fails while `psd-16bit-rle_3.psd` opens cleanly, `Compression16::default()` becomes `Rle`. The stand-ins give no reason to (2026-11-11): all three read the ZIP sample, and Photopea misrenders the PackBits one (finding 3).

## Checkpoints 1 and 2 (2026-11)

The development machine has no Photoshop, so the checkpoint is in two parts:

- **the stand-ins**, below, which are run again after every change to a writer;
- **Photoshop itself**, still to be run on this set, with "What to look at", before the first public release. Until then `Compression16::default()` (ZIP) is provisional, and so is the absence of tag 34377 from the layered TIFFs.

Regenerate the set with `just ps-samples --bigtiff` before checking, if the write or decode code has changed since. Every layered TIFF carries the Software tag `Photostacker <version>`, which discovery uses to skip Photostacker's own TIFFs.

### The stand-ins, 2026-11-11

GIMP 3.2.8, PhotoCraft 0.22.0 and Photopea as served that day, on a set made after finding 1 was fixed. The script ended with no unexpected difference. In the table, **agrees** means everything "The stand-ins" lists for that reader held: the file opened with no message or warning; its layers are those of "The files", by their exact names, in order; every layer is visible, Normal, at 100 %, unlocked and without a mask; and the pixels compared are the same.

| File | GIMP | PhotoCraft | Photopea | Photoshop | Notes |
|---|---|---|---|---|---|
| `psd-8bit-rle_3.psd` | agrees | agrees | agrees | not run | |
| `psd-8bit-rgba_3.psd` | agrees; the merged image, unblended, is the top layer within 1 | agrees | agrees | not run | item 6, finding 1 |
| `psd-16bit-zip_3.psd` | agrees; profile "Photostacker linear sRGB" read as linear | agrees | agrees | not run | item 3 |
| `psd-16bit-rle_3.psd` | agrees | agrees | the layers agree; **its render is wrong** | not run | finding 3 |
| `psd-16bit-rgba_3.psd` | the layers agree; **the merged image, unblended, is not the top layer** | agrees; its re-save stores our merged image | agrees | not run | item 8, finding 4 |
| `psd-8bit-gray-source_3.psd` | agrees; no profile; each layer's red, green and blue are its source's gray | agrees | agrees | not run | item 10 |
| `psd-8bit-adobe-rgb_3.psd` | agrees; profile "Photostacker Adobe RGB (1998)"; the layers hold their sources' numbers | agrees | agrees | not run | item 11 |
| `psb-wide_2.psb` | agrees | agrees | agrees | not run | |
| `psb-wide-16bit_2.psb` | agrees | agrees | agrees | not run | |
| `tiff-8bit_3.tif` | agrees | agrees | agrees | not run | items 5, 7 |
| `tiff-16bit_3.tif` | agrees | agrees | agrees | not run | items 5, 7 |
| `tiff-8bit-rgba_3.tif` | agrees; the TIFF's own image is the top layer, alpha included | agrees | agrees | not run | item 9 |
| `bigtiff-16bit_2.tif` | opens; **the composite only, no layers** | **refused: too much memory** | **not given to it** | not run | item 4, finding 2 |
| `DSC_0039_5.psd` | agrees; profile "Photostacker sRGB" | agrees (its re-save was not compared: the file is over 64 MB) | agrees | not run | |
| `DSC_0039_5.tif` | agrees | agrees | agrees | not run | |

What the stand-ins found:

1. **The merged image of a document with transparency was not blended against white.** The writer stored the top layer's plain colour; Photoshop's convention, which GIMP's loader undoes and PhotoCraft's writer follows, is the colour blended against white. A reader of the merged image alone therefore showed darker soft edges. Fixed (spec §6.2); the table is from the set made after the fix. The merged image is still the top layer alone, where Photoshop's would also show partly transparent layers below (spec §5.6).
2. **No stand-in confirms the BigTIFF's layers** (item 4). GIMP opens the file and reads its profile, but shows the composite only: its loader does not know the BigTIFF form of the layer block. PhotoCraft estimates 15.4 GB of pixel memory for it and refuses. Photopea had not opened it after 15 minutes, so the script no longer gives it the file. Photoshop alone can answer for now. A small sample forced into the BigTIFF container would let PhotoCraft and Photopea be asked.
3. **Photopea misrenders PackBits 16-bit layers** (`psd-16bit-rle_3.psd`: black and coloured noise where the picture should be). GIMP and PhotoCraft read the same file sample for sample like the ZIP one, so the encoding follows the specification as two readers understand it. ZIP is the default, and all three read it.
4. **GIMP undoes the blend against white in sRGB encoding, whatever the document's profile** (`psd-load.c` unblends in `R'G'B'A float`). On `psd-16bit-rgba_3.psd`, whose profile is linear, it therefore does not get the top layer back (it is off by up to 10,737 of 65,535 where the layer is at least half opaque). PhotoCraft, saving the same document, stores our merged image: equal where the top layer is opaque or clear, and within 1 wherever both store the same alpha. On the 8-bit sample, which has no profile, GIMP agrees within 1.
5. **Every layered TIFF shows its layers in all three readers without tag 34377** (item 5), and all three accept the big-endian byte order (item 7).
6. **All three read the three generated profiles** (linear sRGB, sRGB, Adobe RGB (1998)) where they report one: GIMP by name, and the linear one as linear (item 3); PhotoCraft carries each into its PNG.
