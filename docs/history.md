# History

This is the history of Photostacker in brief: where the project came from and what was built, in the order it happened.
It was written when the repository was published with a fresh git history, so it stands in for the commits that are not there.
How the app works today is in [design.md](design.md) and [../README.md](../README.md).

## The Python app (2026-05)

The first Photostacker was a Python 3.14 web app: FastAPI and Uvicorn, pages rendered with Jinja2 and HTMX, SQLite through SQLModel, and an asyncio queue over a process pool, shipped as a Docker compose stack.
It read EXIF through pyexiftool (Perl exiftool), decoded with Pillow, rawpy and pillow-heif, hashed with imagehash, ran DINOv2-base as ONNX weights through onnxruntime (no PyTorch), and wrote PSD/PSB with PhotoshopAPI (pytoshop as a fallback) and layered TIFF with psdtags, tifffile and imagecodecs.

- **2026-05-21** Bootstrap the app around a three-stage funnel: EXIF buckets, perceptual hashes, then an optional visual check.
- **2026-05-21** Bucket photos by camera model, lens and focal length within a 4 s time window; shots with MakerNote bracket tags bucket by camera and capture second.
- **2026-05-21** Link photos in a bucket when the summed Hamming distance of pHash and dHash is at most 8, from 512 px orientation-corrected thumbnails.
- **2026-05-21** Profile each stack before writing: the most common resolution is the canvas, other resolutions are left out, and any 16-bit member makes the stack 16-bit.
- **2026-05-21** Write PSD through PhotoshopAPI 0.9, switching to PSB for oversized canvases, and layered TIFF through psdtags.
- **2026-05-21** Review before writing: a job stops at review, each proposed group is confirmed or rejected on an HTMX page with cached 256 px thumbnails, and only confirmed groups are written.
- **2026-05-21** Stage 3: DINOv2-base embeddings, downloaded from Hugging Face on first use, split ambiguous clusters by cosine similarity (threshold 0.85); the transformers dependency was dropped for inline preprocessing.
- **2026-05-21** Stage 3 on by default, with an ML badge and score per group, a per-job embeddings cache (`.npz`) with cleanup, and a re-analyze action for a new cosine threshold.
- **2026-05-21** Docker image and compose stack with bind-mounted inputs, outputs, thumbnails, model cache and database.
- **2026-05-21** RAW input through rawpy (LibRaw), HEIC through pillow-heif; one corrupt file no longer fails the whole job.
- **2026-05-22** Advanced options and host-path checks in the job form, a finer cosine slider (step 0.001), and toasts for failed requests.
- **2026-05-22** Stage 3's ambiguity floor (6) decoupled from the hash threshold; re-analyze drops singletons and keeps the group counts in sync.
- **2026-05-23** Saved preferences; scan, clear and open the inputs folder; upload a folder by dropping it on the page.
- **2026-05-23** "Liquid Glass" redesign of the home and review pages, with themed modals and toasts, then a dark mode.
- **2026-05-23** Delete jobs; the jobs list is pushed by server-sent events instead of polled.
- **2026-05-23** Reveal output paths on the host through container-to-host path mappings, with a copy to the clipboard.
- **2026-05-23** Security fixes: no script injection through paths, a whitelist before a thumbnail folder is deleted, symlinks in inputs removed rather than followed, a cap on upload size.
- **2026-05-23** Layers centred on the canvas instead of anchored at a corner (adjusted on 2026-05-25 for PhotoshopAPI's top-left origin).
- **2026-05-25** Name each output after its first stacked image and its layer count.
- **2026-05-26** Upload progress shown on the drop zone; the source-path field moved behind Advanced.
- **2026-05-27** Recover from a broken process pool, and fall back to pytoshop on CPUs without AVX2.
- **2026-05-27** Write the merged image data uncompressed, since Photoshop and PhotoshopAPI misread it ZIP-compressed.

What it could not do:

- It was a web server in Docker, so photos had to be uploaded or bind-mounted, and host paths translated.
- Its fast PSD writer needed a CPU with AVX2; it needed Perl exiftool, and Python and ML wheels installed.
- It had no installers or release artefacts.
- It held full-resolution images of a stack in memory together, which ran out of memory on large brackets.
- A job interrupted by a crash or restart could not resume, and the review could not move, split or merge photos.
- Any non-empty MakerNote bracket value counted as bracketing, an "off" value included.
- Wide exposure brackets often stayed above the hash threshold of 8: a public ±2 EV set measured 22 with its plain hashes.

## The Rust rewrite (2026-11)

The rewrite keeps the pipeline and changes how it is delivered: an MIT-licensed, offline desktop app and a headless CLI on one core library, with no server, no Docker, no Python and no Perl at run time.
EXIF and MakerNotes are read by the project's own parser; jobs, hashes and embeddings live in one SQLite file with sqlite-vec; the visual check runs on ONNX Runtime with DINOv2-small, opt-in and downloaded once; PSD, PSB and layered TIFF are written by the project's own encoders; RAW is decoded by rawler and HEIC by libheif; the UI is egui.
The Python app stayed in the tree as the parity reference for hashing and grouping until the rewrite replaced it.

### Foundations and grouping

- **2026-11-07** Cargo workspace with the core library, the CLI and a test-fixture crate; the Python app moved aside as the parity reference.
- **2026-11-07** Every job parameter in one `JobConfig`, validated once.
- **2026-11-07** Folder discovery with a content fingerprint per file (BLAKE3 of size, mtime and the first 64 KiB); unreadable entries no longer stop a scan.
- **2026-11-07** Pair a RAW with the JPEG or HEIC of the same folder and stem; the RAW is stacked by default.
- **2026-11-07** Own EXIF reader for JPEG and the TIFF family of RAWs, then for RAF and CR3 containers.
- **2026-11-07** MakerNote bracket fields for Canon, Nikon and Sony (the unencrypted ones) and Fujifilm; a bracket mode of "off" now means not bracketing.
- **2026-11-07** EXIF compared against exiftool output on test fixtures; exiftool is used only to make those golden files ([exif-fixture-coverage.md](exif-fixture-coverage.md)).
- **2026-11-07** Thumbnails with a Pillow-compatible resize, and pHash and dHash matching Python's imagehash on golden values.
- **2026-11-08** RAW thumbnails through rawler, from the embedded preview or a binned develop; HEIC decoding and EXIF through a built-in libheif.
- **2026-11-08** Stage 1: one open bucket per camera key, joined while a shot is within the time window of the previous one; a shot-number reset closes a bracket's bucket.
- **2026-11-08** Stage 2: order-independent linking (union-find); one-photo groups kept for an Unstacked tray instead of dropped; first shot on top.
- **2026-11-08** `analyze()` with a JSON plan; a panic in one file's analysis becomes that file's error.
- **2026-11-08** CLI `photostacker run --dry-run --plan`.

### Writers

- **2026-11-08** Sniff a still image's content before decoding, so a JPEG named `.heic` is read as a JPEG.
- **2026-11-08** For a RAW that rawler cannot decode, fall back to its largest embedded JPEG, or promote its paired JPEG or HEIC.
- **2026-11-08** Never overwrite: write to `.partial`, then hard-link to a free name (`-1`, `-2`, …); check the output folder canonically.
- **2026-11-08** Full-resolution decode and a header-only probe for stills and HEIC; RAW develop to 16-bit sRGB or linear, with generated ICC profiles.
- **2026-11-08** Canvas profile per stack: most common resolution, 16-bit or alpha if any member has it, the first member's ICC profile, and only RGB profiles embedded.
- **2026-11-08** Shared layer encoder: one full-resolution layer in memory at a time, compressed into spill files (PackBits for 8-bit, ZIP with prediction for 16-bit).
- **2026-11-08** PSD and PSB writer: `Lr16` for 16-bit, a negative layer count with alpha, the top layer as uncompressed merged image, PSB past 30,000 px or 2 GB.
- **2026-11-08** Photoshop-layered TIFF: the composite as TIFF image data and the layers in tag 37724, big-endian, BigTIFF past 30,000 px or 4 GB.
- **2026-11-08** Layer names from the file stem plus the exposure of an exposure bracket (`IMG_0412 (−2 EV)`), with a Unicode name; files named `<first stem>_<layers>`.
- **2026-11-08** A Photoshop sample set (`just ps-samples`), read back by the project's own strict PSD reader.
- **2026-11-08** CLI `run` writes the stacks, by default into `<input>/stacked`.

### Store, engine and the visual check

- **2026-11-08** Lenient EXIF: one tag that cannot be parsed no longer drops a file's EXIF.
- **2026-11-08** Files timed by mtime bucket apart; a repeated bracket value closes a bucket; a Google Pixel's RAW and JPEG pair.
- **2026-11-08** Group wide exposure brackets by brightness-normalised hashes and bracket-run links ([calibration.md](calibration.md)).
- **2026-11-08** A shared 2 GiB decode memory budget replaces fixed decode limits.
- **2026-11-08** Recursive runs mirror their subfolders in the output; Photostacker's own TIFFs are never scanned as photos.
- **2026-11-08** Tracing in the core and the CLI; untagged Adobe RGB camera JPEGs get a generated Adobe RGB (1998) profile.
- **2026-11-08** `regroup`: one pure function for Stages 2 and 3; a group whose members are unchanged keeps its decision, a changed one is flagged "look again".
- **2026-11-08** SQLite store with sqlite-vec and numbered migrations; resumable analysis through a cache of each file's examination.
- **2026-11-09** Engine on a worker thread: job queue, events, checkpoints, review edits (move, split, merge, swap a pair), live regrouping and writing.
- **2026-11-09** Resume interrupted jobs at start; unfinished files of a crashed write are removed before it is written again.
- **2026-11-09** Model registry (dinov2-small, dinov2-base) with a resumable, SHA-256-checked download, a mirror URL and an offline import.
- **2026-11-09** DINOv2 embedder on ONNX Runtime 1.31.1, loaded at run time.
- **2026-11-09** Stage 3 in the engine: bracket-run links are fixed edges; a model that fails skips the check with one warning.
- **2026-11-09** CLI on the engine, with `models list|download|import|remove`; Ctrl+C cancels and removes the unfinished stack.
- **2026-11-09** dinov2-small calibrated on public bracket sets: default cosine 0.50.

### The desktop app

- **2026-11-10** User files are read into memory instead of mapped, so a file truncated during a scan cannot kill the process.
- **2026-11-10** One engine per database, held by a lock file; a worker that dies is reported.
- **2026-11-10** `retry` rewrites stranded stacks: those that failed, or were skipped because a photo could not be read.
- **2026-11-10** Cheaper review reads and edits, measured by a kept benchmark.
- **2026-11-11** Model download, import and removal through the engine; a prediction of the file each stack will become, from headers only.
- **2026-11-11** The `Photostacker` app (egui/eframe): logo, icons, the Inter font, light and dark themes, and a backend thread for every engine call.
- **2026-11-11** Toolbar, jobs list, status bar and toasts; the new-job form, the folder picker and dropping a folder.
- **2026-11-11** Scan views with live progress and the files that could not be read; thumbnails loaded off the UI thread.
- **2026-11-11** Review: stacks with thumbnails, live sliders, confirm and reject, bulk actions and keyboard shortcuts.
- **2026-11-11** The Unstacked tray and drag and drop between stacks; refusals worded in stacks and photos.
- **2026-11-11** Inspector: a stack's prediction, members and actions; a photo's preview and camera data.
- **2026-11-11** Writing and results views, with Reveal and a retry of stranded stacks.
- **2026-11-11** Settings: theme, defaults of new jobs, saved window and panel sizes, the Stage 3 models, and an ML chip in the toolbar.
- **2026-11-11** A model's download asks first and names every host it may reach.
- **2026-11-11** A log file a day, a window for a failed start, and a question before closing while a job runs.
- **2026-11-11** End-to-end tests through the real widgets; the README screenshot drawn from synthetic photos ([../crates/ui/DESIGN.md](../crates/ui/DESIGN.md)).
- **2026-11-11** An Adobe RGB sample in the Photoshop set, and a script for a native Windows build (`scripts/win.ps1`).

### After the first hand trial

- **2026-11-11** The first native Windows build: the `.exe` built without changes to the code, and a smoke run opened the window and exited cleanly. HEIC is left out of it for now.
- **2026-11-11** New jobs compare photos shot up to 30 s apart: at 4 s, a first trial found no stack in folders whose series were shot 5 to 30 s apart.
- **2026-11-11** A time window slider in the review (1 to 60 s), beside hash distance and similarity, regrouping live.
- **2026-11-11** When no stack is found, the review says so and suggests a longer time window.
- **2026-11-11** On a canvas with alpha, the PSD/PSB merged image is the top layer blended against white, Photoshop's convention; before, readers of the merged image showed dark soft edges.
- **2026-11-11** A script opens every sample in three stand-in readers: GIMP 3.2, PhotoCraft and Photopea ([photoshop-checks.md](photoshop-checks.md)). They found:
  - the missing blend against white above, fixed the same day;
  - no stand-in can confirm the BigTIFF's layers;
  - Photopea misrenders PackBits 16-bit layers, while all three read the default ZIP ones;
  - GIMP undoes the blend in sRGB encoding whatever the profile, so it does not get the top layer back from a linear 16-bit document;
  - every layered TIFF shows its layers in all three without tag 34377, big-endian included.
- **2026-11-11** A hand trial of the Windows build raised nothing further.

## What changed between the two

| | Python app | Rust app |
|---|---|---|
| How it runs | FastAPI and HTMX web app, in Docker or through uv; photos bind-mounted or uploaded | Desktop app and CLI, native binaries; folders read in place; no server |
| EXIF reading | pyexiftool driving Perl exiftool | Own EXIF and MakerNote reader (Canon, Nikon, Sony, Fujifilm) |
| Hashes for grouping | Plain pHash + dHash, distance ≤ 8, linked greedily from a seed | Brightness-normalised pHash + dHash, distance ≤ 8, union-find, plus bracket-run links up to 40 |
| Visual check | DINOv2-base through onnxruntime, cosine 0.85, on when its extras were installed | Opt-in; DINOv2-small (cosine 0.50) or DINOv2-base (0.85), downloaded once after a question, checked by SHA-256 |
| Time window default | 4 s (up to 600 s) | 30 s (1 to 60 s), changeable in the review |
| Output formats | PSD/PSB (PhotoshopAPI, pytoshop), layered TIFF (psdtags) | Own PSD/PSB, layered TIFF and BigTIFF writers, 8 or 16-bit, never over an existing file |
| Merged image | The first layer written | The top layer (the first shot) |
| Where data lives | SQLite, `.npz` embeddings and thumbnails in Docker volumes | One SQLite file with sqlite-vec in the platform's data folder, thumbnails in its cache folder; `PHOTOSTACKER_DATA_DIR` moves both |
| RAW support | rawpy (LibRaw) | rawler (pure Rust), 16-bit sRGB or linear, embedded-JPEG fallback for unknown bodies |
| HEIC | pillow-heif | libheif with libde265, built in; not yet in the Windows build |
| Review | Confirm or reject each group; re-analyze at another cosine | Also move, split and merge, an Unstacked tray, live sliders, and jobs that resume after a quit |

## Decisions worth knowing

- **Normalised hashes and bracket runs.** Plain hash distances of real ±2 EV and ±6 EV brackets reach 33 and 57; equalising the histogram first brings several sets to 10 or less, and camera-marked bracket shots link up to 40 for the rest, while different scenes stay at least 48 apart.
- **Stage 1 only bounds Stage 2.** Buckets never merge photos by themselves; they limit which photos the hashes may link, and each camera body has its own, so two bodies shooting at once stay apart.
- **Order-independent grouping.** Union-find over every pair within the threshold gives the same groups whatever the order of the files; the Python app linked greedily from a seed.
- **The EXIF reader is the project's own.** Third-party crates do not reliably expose the TIFF block that MakerNote offsets need, the tag set is small, and exiftool-rs is GPL; the MakerNote parser is written from public tag documentation.
- **ONNX Runtime is loaded at run time, not linked.** The prebuilt static build needs glibc 2.38 or later, newer than the Ubuntu 22.04 release host; the app and the CLI run without the library, and the visual check is then skipped.
- **The visual check never splits a camera-confirmed bracket.** Bracket-run links are fixed edges of its graph, because the ends of a ±6 EV bracket look unalike to DINOv2 (cosine 0.10).
- **16-bit layers use ZIP with prediction.** All three stand-in readers read it, and Photopea misrenders the PackBits alternative; the choice is provisional until Photoshop opens the samples.
- **The merged image is the top layer.** Every layer covers the canvas at 100 % in Normal mode, so the top layer is exactly what Photoshop shows where it is opaque; compositing all layers would need a canvas-sized buffer beside the layer being encoded.
- **One full-resolution layer in memory at a time.** Each layer is decoded, compressed to a spill file and freed before the next, which keeps large brackets within memory.
- **Never overwrite.** Stacks are written as `.partial` and hard-linked to a free name, into `<input>/stacked` by default, which the scan always skips.
- **Stand-in readers instead of Photoshop.** The development machine has no Photoshop, so GIMP, PhotoCraft and Photopea, written independently of the writers, gate the files; Photoshop itself is to open them before the first public release.
- **No GPL dependency.** The app is MIT; LGPL is allowed only for rawler and libheif/libde265, and a test fails if libheif links a GPL encoder such as x265.

## Still open

- Photoshop has opened none of the files yet; until it does, the 16-bit ZIP default and the absence of tag 34377 in layered TIFFs are provisional.
- No reader has confirmed the layers of a BigTIFF.
- CI, packaging, the release workflow and the licence checks are not written; there are no installers yet.
- HEIC is missing from the Windows build, and nothing has been built or run on macOS yet.
- The visual check's default of 0.50 needs calibration on near-duplicate photos (the same scene from another angle), which no public set provides.
- The time window's range ends at 60 s; one trial folder still gained stacks at 45 and 60 s, and the Python app allowed 600 s.
- On partly transparent stacks the merged image is the top layer alone, where Photoshop's composite would also show the layers below.
- The bracketing path of the MakerNote parser has been checked on public bracket sets, not yet on the owner's own camera files.
- Names in scripts the Inter font does not cover (CJK, Arabic, Hebrew, Thai) are drawn with the system's fonts, and as empty boxes where the system has no font for their script. A right-to-left name of several words shows its words in the wrong order.

The rules for working in the repository are in [../AGENTS.md](../AGENTS.md).
