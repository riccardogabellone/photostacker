<p align="center">
  <img src="assets/icons/icon-128.png" width="96" height="96" alt="">
</p>

# Photostacker

Photostacker finds the series of one shot in a folder of photos (exposure brackets, HDR sets, fast bursts) and stacks each series as layers in one PSD, PSB or layered TIFF, ready for Photoshop or any editor that reads layers.

It is a desktop app and a command-line tool. It works offline, only reads your photos, and never writes over a file.

![The review: proposed stacks, the unstacked tray, the sliders and the inspector](assets/screenshots/ui.png)

## Status

Photostacker is at 0.1.0 and has no installers yet: for now it is built from source.

- Developed and tested on Linux, natively and under WSL.
- A script builds the app for Windows; that build has no HEIC support yet.
- Nothing has been built or run on macOS yet.
- Photoshop itself has not opened the files yet. Three other readers have (GIMP, PhotoCraft and Photopea): see [docs/photoshop-checks.md](docs/photoshop-checks.md).

## How it works

1. **Scan.** A folder is read (its subfolders too, if you ask): JPEG, PNG, TIFF, HEIC and RAW files (CR2, CR3, NEF, ARW, RAF, ORF, RW2, PEF, DNG and the other formats [rawler](https://github.com/dnglab/dnglab) decodes). A RAW and the JPEG beside it count as one photo.
2. **Group.**
   - *By time and camera.* Only shots from the same camera, lens and focal length are compared, each within the time window of the shot before it (30 s by default). A bracket sequence that a Canon, Nikon, Sony or Fujifilm camera records in its metadata is kept to that sequence.
   - *By look.* Two perceptual hashes of a brightness-normalised thumbnail link the shots that show the same frame, so the dark and bright ends of an exposure bracket still match.
   - *By a visual check, if you turn it on.* A DINOv2 model compares the photos of each doubtful stack and splits the ones that only look alike. The model (88 MB for the default one) is downloaded once, after the app has asked, and runs on your machine.
3. **Review.** Every proposed stack is shown with its photos. Confirm or reject stacks, drag photos between them, split or merge them, and move the sliders (hash distance, time window, similarity) to regroup at once.
4. **Write.** Each confirmed stack becomes one file, in `<input folder>/stacked` unless you choose another folder:
   - the first shot is the top layer, and each layer is named after its photo, with the exposure of a bracket (`IMG_0412 (−2 EV)`);
   - 8 or 16 bits, with the photos' colour profile; PSB or BigTIFF when a stack is too large for PSD or TIFF;
   - one full-size photo is in memory at a time, so large brackets do not run out of memory;
   - a name that is taken gets `-1`, `-2` and so on.

Jobs are kept in a local database, so a scan or a review can be closed and picked up again. The exact rules are in the [design specification](docs/design.md).

## Build and run

You need a [Rust](https://rustup.rs) toolchain (1.99 or later), a C and C++ compiler, and CMake. The recipes use [just](https://github.com/casey/just), and the tests [cargo-nextest](https://nexte.st).

### Linux

```sh
sudo apt install build-essential cmake pkg-config libde265-dev libegl1 zenity
just run                                   # the desktop app
just run-cli run <folder of photos>        # the command line: group and write the stacks
```

`libde265-dev` is the HEIC decoder, and `zenity` gives the app its file dialogs on a desktop without an XDG portal. Without `just`: `cargo run --release -p photostacker-ui`, and `cargo run --release -p photostacker-cli -- run <folder of photos>`.

### Windows

With the MSVC toolchain and the Windows SDK (Visual Studio's "Desktop development with C++"), in PowerShell 7:

```powershell
pwsh scripts/win.ps1                       # builds dist/windows/Photostacker.exe, with ONNX Runtime beside it
```

### The visual check

The model runs on Microsoft's [ONNX Runtime](https://onnxruntime.ai), which is loaded when the check first runs, not linked into the app: 1.31.1, or 1.19.2 on Intel Macs. On Linux x64 and macOS, `just ort` downloads and verifies it once. Elsewhere, put the library beside the executable or set `ORT_DYLIB_PATH` to it. Without the library the app and the tool work as before, and the visual check is skipped.

## Command line

```sh
photostacker-cli run <folder>                   # group the photos and write the stacks into <folder>/stacked
photostacker-cli run <folder> -o <output> --format tiff --recursive
photostacker-cli run <folder> --dry-run --plan plan.json   # group only, and save the plan
photostacker-cli run <folder> --window 45 --hamming 12     # a longer time window, a looser match
photostacker-cli run <folder> --no-ml           # without the visual check
photostacker-cli models list                    # the models of the visual check: also download, import, remove
photostacker-cli info                           # what this build holds: HEIC, ONNX Runtime, the data folder
```

`photostacker-cli run --help` lists every option with its default.

## Where your data is

The job database, the downloaded models and the logs live in your system's application-data folder, and the thumbnails in its cache folder. The app's Settings has `Open the logs folder`. Setting `PHOTOSTACKER_DATA_DIR` to a folder moves all of it there, for the app and the command line alike; use it to try things without touching your real data.

## Documentation

| | |
|---|---|
| [docs/design.md](docs/design.md) | The design specification: every rule of the pipeline, the writers, the database, the app and the command line |
| [docs/history.md](docs/history.md) | Where the project came from, and what is still open |
| [docs/calibration.md](docs/calibration.md) | The measurements behind the grouping thresholds |
| [docs/photoshop-checks.md](docs/photoshop-checks.md) | What other programs make of the files Photostacker writes |
| [docs/exif-fixture-coverage.md](docs/exif-fixture-coverage.md) | Which cameras the metadata reader has been checked against |
| [crates/ui/DESIGN.md](crates/ui/DESIGN.md) | The design choices of the desktop app |
| [AGENTS.md](AGENTS.md) | How to work in this repository: layout, commands, rules |

## Contributing

`just check` is the gate before every commit: formatting, lints and the tests. [AGENTS.md](AGENTS.md) has the rest.

## Licence

Photostacker is released under the [MIT licence](LICENSE). Code ported from other projects, and the bundled font, keep their own notices in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). The DINOv2 models are downloaded, not bundled; they are Apache-2.0.
