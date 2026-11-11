# Calibration

Measured numbers behind the grouping defaults. Distances are the combined Hamming distance of pHash and dHash (0–128, spec §5.3); cosines are those of Stage 3 (spec §5.4).

## Grouping

Wide exposure brackets group at the default `hamming_threshold` of 8 through camera data and normalised hashes. Two rules do it (spec §5.3):

- Every distance is over the **normalised** hashes: pHash and dHash after a histogram equalisation (Pillow's `ImageOps.equalize`). The plain hashes are still stored, for parity with Python's `imagehash` (spec §12.3).
- **Bracket runs:** consecutive shots of one bucket that the camera marks as an exposure bracket, with a shot number or a bracket value, link up to `BRACKET_RUN_MAX_DISTANCE` = 40, whatever the threshold.

### The sets

These are the public bracket sets under `$PHOTOSTACKER_FIXTURES/brackets/<vendor>/<set>`. The fixture test `public_brackets_group_at_the_default_threshold` in `crates/core/tests/scenarios.rs` runs `analyze` with the default settings on each set. It prints these numbers, and it requires one group holding every file for each set whose files all read as `BracketMode::Exposure`.

All six sets read as exposure brackets, so none was left unchecked. The Sony set is a pixel shift of four frames. The A7R III records ReleaseMode 5 for those frames, which the Sony MakerNote parser reads as exposure bracketing, so the set counts as an exposure bracket here. Its frames hash nearly alike in any case.

### Spike (Python, 2026-11-08)

Pillow on the same sets. The Python thumbnail path differs from ours for RAWs (a rawpy half-size develop), so its numbers differ somewhat from the Rust ones.

| Set | Plain max | Normalised max | Normalised consecutive max |
|---|---|---|---|
| Canon 450D AEB3, ±2 EV | 22 | 3 | 3 |
| Canon 1D Mk IV AEB7, ±6 EV | 57 | 37 | 18 |
| Nikon D80 AEB3, JPEG | 24 | 26 | 26 |
| Nikon D200 AEB7, JPEG | 30 | 9 | 6 |
| Nikon D200 AEB5, NEF | 12 | 8 | 8 |

Different scenes stayed at a normalised distance of at least 49.

### Rust (`analyze`, 2026-11-08)

"Consecutive" is consecutive in capture order, the order bracket runs link in. All six sets give one group of all their files.

| Set | Files | Plain max | Normalised max | Normalised consecutive max |
|---|---|---|---|---|
| Canon 450D AEB3, ±2 EV (CR2) | 3 | 33 | 3 | 3 |
| Canon 1D Mk IV AEB7, ±6 EV (CR2) | 7 | 57 | 40 | 27 |
| Nikon D80 AEB3, JPEG | 3 | 25 | 26 | 26 |
| Nikon D200 AEB7, JPEG | 7 | 29 | 10 | 7 |
| Nikon D200 AEB5, NEF | 5 | 12 | 9 | 7 |
| Sony A7R III pixel shift ×4 (ARW) | 4 | 0 | 2 | 2 |

Across sets, the smallest normalised distance between two files of different scenes is 48. The fixture test requires it to stay above 40.

### Reading the numbers

- With plain hashes, every set but the pixel shift is over the threshold of 8; the 1D Mk IV reaches 57.
- Normalisation brings the 450D and D200 sets to 3–10, but leaves the 1D Mk IV at 40 and the D80 at 26. Clipped highlights and crushed shadows erase detail that no tone curve brings back, which is why the D80 gets no better.
- The bracket runs close that gap. The worst consecutive normalised distance is 27, against the limit of 40. The closest different scenes are 48 apart, so the limit leaves 13 of headroom on the bracket side and 8 on the other.
- The synthetic scenario `wide_exposure_bracket_groups_at_default_threshold` (a testkit scene at −3, 0 and +3 EV, with the +3 EV shot clipped) measures plain 14 and normalised 9. It groups through its run links: it is still one stack at a threshold of 0.

## Stage 3

The default cosine threshold of dinov2-small (spec §5.4, §12.4), measured on 2026-11-09 with `just calibrate-ml` (`crates/core/examples/calibrate_ml.rs`). It embeds each photo as Stage 3 does: the active files that hash, from their 512 px upright thumbnail, through ONNX Runtime 1.31.1 on the CPU.

### How the threshold is chosen

On labelled data only: the six public bracket sets of § "Grouping". The pairs within a set are the positives; the pairs across two sets are the negatives. The suggested threshold is the midpoint of the gap between the smallest positive and the largest negative, or the smallest positive when they overlap. It is rounded to 0.01, never above the smallest positive, and then clamped to 0.5–0.99, the range a job accepts. The clamp is applied last, so it can lift the value above the smallest positive.

The user's own folders are a sanity check only, read in place and reported as aggregate numbers: they hold real stacks and unrelated neighbouring shots, with no labels.

### The public sets

Cosine similarity within each set (all its pairs):

| Set | Photos | dinov2-small min | median | dinov2-base min | median |
|---|---|---|---|---|---|
| Canon 450D AEB3, ±2 EV (CR2) | 3 | 0.833 | 0.946 | 0.864 | 0.954 |
| Canon 1D Mk IV AEB7, ±6 EV (CR2) | 7 | 0.097 | 0.562 | 0.066 | 0.493 |
| Nikon D80 AEB3, JPEG | 3 | 0.917 | 0.963 | 0.928 | 0.967 |
| Nikon D200 AEB7, JPEG | 7 | 0.795 | 0.946 | 0.813 | 0.937 |
| Nikon D200 AEB5, NEF | 5 | 0.873 | 0.956 | 0.924 | 0.970 |
| Sony A7R III pixel shift ×4 (ARW) | 4 | 1.000 | 1.000 | 0.999 | 0.999 |

| | dinov2-small | dinov2-base |
|---|---|---|
| Within sets (64 pairs): min, median | 0.097, 0.923 | 0.066, 0.930 |
| Across sets (342 pairs): median, max | 0.038, 0.417 | 0.026, 0.302 |
| Overlap (smallest positive below the largest negative) | 0.320 | 0.237 |
| Suggested threshold | **0.50** | 0.50 |
| Within-set pairs below 0.50 | 9 of 64, all of the 1D Mk IV | 11 of 64, all of the 1D Mk IV |
| Across-set pairs at or above the threshold | 0 | 0 |

The closest different scenes are the 1D Mk IV and the D200 AEB7 JPEG for dinov2-small (0.417), and the 450D and the 1D Mk IV for dinov2-base (0.302).

No public near-duplicate distractor set (another angle of the same scene) exists yet. With one, the harness takes it as `<distractors-root>`: its pairs join the negatives.

### Reading the numbers

- **The positives and negatives overlap,** because of one set: the ends of the ±6 EV 1D Mk IV bracket are nearly black and nearly white, and DINOv2 sees them as unalike as different scenes (0.10). So the rule gives the smallest positive, which the clamp lifts to the floor of the range: **dinov2-small's `default_cosine` is 0.50**. Every cross-set pair is below it, with 0.08 to spare.
- **A camera-confirmed bracket never depends on the threshold.** All six public sets group through their bracket-run links, which are fixed edges of Stage 3's graph (spec §5.4), so no threshold splits them. Their within-set cosines show why those edges are needed.
- **Without the 1D Mk IV,** the smallest positive would be 0.795 (dinov2-small, the D200 AEB7 JPEG), and the midpoint of its gap to the largest negative (0.417 with the 1D Mk IV, no more without it) would be at most 0.61. The rule keeps 0.50 because it is chosen on all the labelled data; a near-duplicate distractor set would show whether a higher value is worth it.
- **dinov2-base's 0.85** (the value the earlier Python app used with that model) does not separate the public sets: 20 of the 64 within-set pairs are below it, in the 1D Mk IV and the D200 AEB7 JPEG (down to 0.81). It splits none of them in practice, for the reason above.

### Sanity checks on the user's own folders

- **A private folder of 102 JPEGs from one camera** (real stacks among unrelated neighbouring shots, no camera bracket data):
  - At a time window of 4 s, the default when this was measured (it is 30 s since 2026-11-11, spec §5.2), Stage 2 makes no group: its stacks were shot more than 4 s apart.
  - At a 60 s window: 19 Stage-2 groups of 81 photos, 12 of them above the floor of 6. The cosines inside the groups are at least 0.886 (dinov2-small; median 0.992) and 0.893 (dinov2-base; median 0.991); inside the groups above the floor, at least 0.947 and 0.901.
  - Stage 3 splits none of the 19 groups at any threshold from 0.50 to 0.90, with either model.
- **A private phone series of 13 shots of one scene**: pairwise cosines from 0.204 to 0.995, median 0.546 (dinov2-small); 0.197 to 0.985, median 0.487 (dinov2-base). The frames differ in framing and in what little detail they show, so a threshold of 0.50 would keep only about half of their pairs together.

The sanity checks do not contradict 0.50: it splits none of the real stacks.
